//! Forge integration (GitHub) via the `gh` CLI.
//!
//! We shell out to `gh`, reusing the user's existing `gh auth` — no token is
//! stored by this app. `gh` is used for create/merge; inline review comments
//! come from `gh api repos/{owner}/{repo}/pulls/<n>/comments` (the `{owner}`/
//! `{repo}` placeholders are auto-filled by `gh` from the repo's remote), since
//! `gh pr view` does not expose inline line comments.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

use crate::domain::model::{
    CheckStatus, DraftComment, Mergeable, PrInfo, ReviewComment, ReviewEvent, ReviewSummary,
    ReviewThread,
};
use crate::error::{CoreError, Result};

/// Whether the forge can merge a PR as-is.
///
/// `Unknown` is a real answer, not an error: GitHub recomputes mergeability
/// asynchronously after every push, and reports `UNKNOWN` until it lands. A
/// caller that needs certainty must poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStatus {
    Mergeable,
    Conflicting,
    Unknown,
}

/// A one-line summary of an open PR discovered by the review poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_ref: String,
    pub base_ref: String,
    pub url: String,
    /// The PR description, as written by the author.
    pub body: String,
    /// Rolled-up CI state (see [`rollup_status`]).
    pub checks: CheckStatus,
    /// Whether it merges cleanly into its base.
    pub mergeable: Mergeable,
}

/// A PR's live lifecycle state on the forge, as opposed to the snapshot taken
/// at creation. What the reconciliation passes read to notice a PR that was
/// merged or closed on GitHub directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePrState {
    Open { draft: bool },
    Merged,
    Closed,
}

/// One failing check from a PR's `statusCheckRollup` — enough to name it in a
/// dialog and (via `url`, when it points at a GitHub Actions run) fetch its
/// failed-step log for the fixing agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    /// The check's name (e.g. `test`), or the status context (e.g. `ci/lint`).
    pub name: String,
    /// The workflow the check belongs to, when reported (`CheckRun`s only).
    pub workflow: String,
    /// The check's details page — an Actions run URL for GitHub Actions checks.
    pub url: String,
}

/// Cap on any single `gh` invocation so a hung command (auth prompt, network
/// stall) can't block a run actor indefinitely.
const GH_TIMEOUT: Duration = Duration::from_secs(120);

// --- argv builders (unit-tested) -------------------------------------------

pub fn create_pr_args(
    title: &str,
    body: &str,
    base: &str,
    head: &str,
    reviewer: Option<&str>,
    draft: bool,
) -> Vec<String> {
    let mut args = vec![
        "pr".into(),
        "create".into(),
        "--title".into(),
        title.into(),
        "--body".into(),
        body.into(),
        "--base".into(),
        base.into(),
        "--head".into(),
        head.into(),
    ];
    if draft {
        args.push("--draft".into());
    }
    if let Some(reviewer) = reviewer.filter(|r| !r.is_empty()) {
        args.push("--reviewer".into());
        args.push(reviewer.into());
    }
    args
}

/// The reviewer login to record on a created PR: trimmed, with the empty/absent
/// case collapsed to `None` (matching the `--reviewer` arg being omitted).
fn normalize_reviewer(reviewer: Option<&str>) -> Option<String> {
    reviewer
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
}

/// List a repo's collaborators (the people who can be requested as reviewers).
/// One paginated `gh api` call; `{owner}/{repo}` are auto-filled from the remote.
pub fn reviewers_args() -> Vec<String> {
    vec![
        "api".into(),
        "--paginate".into(),
        "repos/{owner}/{repo}/collaborators".into(),
        "--jq".into(),
        ".[].login".into(),
    ]
}

pub fn comments_args(pr_number: u64) -> Vec<String> {
    vec![
        "api".into(),
        format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments"),
    ]
}

/// Squash-merge the PR. Deliberately *without* `--delete-branch`: `gh` deletes
/// the local branch as part of that flag, which fails while the card's worktree
/// still has the branch checked out — and its non-zero exit would then abort a
/// merge GitHub has already committed to. The branch cleanup is ours to do,
/// after the worktree is gone (see `Executor::merge`).
pub fn merge_args(pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "merge".into(),
        pr_number.to_string(),
        "--squash".into(),
    ]
}

/// Read a PR's merge state, so a failed merge can be told apart from one that
/// already landed on a previous attempt.
pub fn pr_state_args(pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--json".into(),
        "state".into(),
        "--jq".into(),
        ".state".into(),
    ]
}

/// Read a PR's live lifecycle state plus its draft flag, so the reconciliation
/// passes can tell an open PR from one merged or closed on GitHub directly.
pub fn pr_live_state_args(pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--json".into(),
        "state,isDraft".into(),
    ]
}

/// Map `gh pr view --json state,isDraft` onto [`LivePrState`]. Anything other
/// than the three known states is `None` — an unrecognized answer must not be
/// read as "closed" and tear local state down.
pub fn parse_live_pr_state(json: &str) -> Option<LivePrState> {
    let v: Value = serde_json::from_str(json).ok()?;
    let state = v.get("state").and_then(Value::as_str)?;
    match state.to_ascii_uppercase().as_str() {
        "OPEN" => Some(LivePrState::Open {
            draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        }),
        "MERGED" => Some(LivePrState::Merged),
        "CLOSED" => Some(LivePrState::Closed),
        _ => None,
    }
}

/// Read whether the PR still merges cleanly onto its base. Asked *after* a merge
/// fails, so a conflict can be told apart from every other reason `gh pr merge`
/// exits non-zero (auth, protected branch, failing checks) without matching on
/// gh's error prose.
pub fn pr_mergeable_args(pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--json".into(),
        "mergeable".into(),
        "--jq".into(),
        ".mergeable".into(),
    ]
}

/// Map GitHub's `mergeable` field onto [`MergeStatus`]. Anything other than the
/// two decided answers — including the empty output of a PR that has vanished —
/// is `Unknown`, never a guess.
fn parse_merge_status(out: &str) -> MergeStatus {
    match out.trim() {
        "CONFLICTING" => MergeStatus::Conflicting,
        "MERGEABLE" => MergeStatus::Mergeable,
        _ => MergeStatus::Unknown,
    }
}

/// Delete the PR's head branch on the remote (what `--delete-branch` would have
/// done for us had it not tripped over the local branch first).
pub fn delete_remote_branch_args(branch: &str) -> Vec<String> {
    vec![
        "api".into(),
        "--method".into(),
        "DELETE".into(),
        format!("repos/{{owner}}/{{repo}}/git/refs/heads/{branch}"),
    ]
}

/// Flip a draft PR to ready-for-review.
pub fn mark_ready_args(pr_number: u64) -> Vec<String> {
    vec!["pr".into(), "ready".into(), pr_number.to_string()]
}

/// The latest submitted review per reviewer (who reviewed + their verdict).
pub fn submitted_reviews_args(pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--json".into(),
        "latestReviews".into(),
    ]
}

/// Post a reply on a specific PR review comment (`comment_id`).
pub fn reply_args(pr_number: u64, comment_id: u64, body: &str) -> Vec<String> {
    vec![
        "api".into(),
        "--method".into(),
        "POST".into(),
        format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments/{comment_id}/replies"),
        "-f".into(),
        format!("body={body}"),
    ]
}

/// Authoritative PR metadata for a head branch — used as a fallback to recover
/// the PR number when `gh pr create`'s printed URL can't be parsed.
pub fn pr_view_args(head: &str) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        head.into(),
        "--json".into(),
        "number".into(),
    ]
}

/// The open PR (if any) whose head is `head` — the adopt dialog's "this branch
/// already has a PR" warning. `--json` fields cover a displayable [`PrInfo`].
pub fn pr_for_head_args(head: &str) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        head.into(),
        "--json".into(),
        "number,url,title,state".into(),
    ]
}

/// Open PRs authored by any of `authors` that the current user hasn't yet
/// reviewed. `author:` qualifiers OR together; `-reviewed-by:@me` is how "PRs I
/// haven't reviewed" is expressed; drafts are excluded. Scoped to the current
/// repo by `gh pr list`.
pub fn review_prs_args(authors: &[String]) -> Vec<String> {
    let mut search = String::from("-reviewed-by:@me -is:draft");
    for a in authors.iter().filter(|a| !a.is_empty()) {
        search.push_str(&format!(" author:{a}"));
    }
    vec![
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        // `body`, `statusCheckRollup` and `mergeable` ride along on the same
        // listing so the board can show intent and CI state without an extra
        // round-trip per PR.
        "--json".into(),
        "number,title,author,headRefName,baseRefName,url,body,statusCheckRollup,mergeable".into(),
        "--search".into(),
        search,
    ]
}

/// Collapse a PR's `statusCheckRollup` array into a single [`CheckStatus`].
///
/// The array mixes two node shapes: `CheckRun` (GitHub Actions et al.), which
/// reports `status` + `conclusion`, and `StatusContext` (legacy commit statuses),
/// which reports a single `state`. Failure wins over pending, and pending wins
/// over success — the board should never call a PR green while something is
/// still running. A skipped or neutral check is not a failure.
pub fn rollup_status(rollup: &Value) -> CheckStatus {
    let Some(nodes) = rollup.as_array() else {
        return CheckStatus::None;
    };
    if nodes.is_empty() {
        return CheckStatus::None;
    }
    let mut pending = false;
    let mut reported = false;
    for node in nodes {
        // `CheckRun` in flight: `status` is QUEUED / IN_PROGRESS / WAITING and
        // `conclusion` is absent until it settles.
        let status = node.get("status").and_then(Value::as_str).unwrap_or("");
        if matches!(
            status.to_ascii_uppercase().as_str(),
            "QUEUED" | "IN_PROGRESS" | "WAITING" | "PENDING" | "REQUESTED"
        ) {
            pending = true;
            reported = true;
            continue;
        }
        // Settled: a `CheckRun`'s `conclusion`, or a `StatusContext`'s `state`.
        let outcome = node
            .get("conclusion")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| node.get("state").and_then(Value::as_str))
            .unwrap_or("");
        match outcome.to_ascii_uppercase().as_str() {
            o if is_failed_outcome(o) => return CheckStatus::Failing,
            "PENDING" | "EXPECTED" => {
                pending = true;
                reported = true;
            }
            "" => {}
            _ => reported = true,
        }
    }
    match (reported, pending) {
        (_, true) => CheckStatus::Pending,
        (true, false) => CheckStatus::Passing,
        (false, false) => CheckStatus::None,
    }
}

/// Read one PR's `statusCheckRollup`. Deliberately `gh pr view` and not
/// `gh pr checks`: the latter exits non-zero when checks are failing or still
/// running — exactly the states this call exists to observe — which would trip
/// `run_gh`'s error handling.
pub fn pr_checks_args(pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--json".into(),
        "statusCheckRollup".into(),
    ]
}

/// Whether a settled check outcome (a `CheckRun`'s `conclusion` or a
/// `StatusContext`'s `state`, uppercased) counts as a failure. Shared between
/// [`rollup_status`] (which only needs the verdict) and [`rollup_failures`]
/// (which needs the failing nodes themselves).
fn is_failed_outcome(outcome: &str) -> bool {
    matches!(
        outcome,
        "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ERROR" | "STARTUP_FAILURE"
    )
}

/// The failing nodes of a `statusCheckRollup` array, named for the "fix checks"
/// dialog and prompt. Handles both node shapes: `CheckRun` (name + workflowName
/// + detailsUrl) and `StatusContext` (context + targetUrl).
pub fn rollup_failures(rollup: &Value) -> Vec<FailedCheck> {
    let Some(nodes) = rollup.as_array() else {
        return Vec::new();
    };
    nodes
        .iter()
        .filter(|node| {
            let outcome = node
                .get("conclusion")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| node.get("state").and_then(Value::as_str))
                .unwrap_or("");
            is_failed_outcome(&outcome.to_ascii_uppercase())
        })
        .map(|node| {
            let text = |key: &str| {
                node.get(key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let name = match text("name") {
                n if !n.is_empty() => n,
                _ => text("context"),
            };
            let url = match text("detailsUrl") {
                u if !u.is_empty() => u,
                _ => text("targetUrl"),
            };
            FailedCheck {
                name,
                workflow: text("workflowName"),
                url,
            }
        })
        .collect()
}

/// The GitHub Actions run id embedded in a check's details URL
/// (`…/actions/runs/<id>[/job/<job>]`), if it is an Actions URL at all — a
/// third-party status context links elsewhere and yields `None`.
pub fn run_id_from_url(url: &str) -> Option<u64> {
    let (_, rest) = url.split_once("/actions/runs/")?;
    let id = rest.split(['/', '?', '#']).next()?;
    id.parse().ok()
}

/// The failed steps' log of one Actions run — the raw material for the fixing
/// agent's prompt.
pub fn run_log_args(run_id: u64) -> Vec<String> {
    vec![
        "run".into(),
        "view".into(),
        run_id.to_string(),
        "--log-failed".into(),
    ]
}

/// Map GitHub's `mergeable` enum onto [`Mergeable`]. Anything other than the two
/// definitive answers (notably `UNKNOWN`, returned while GitHub computes the
/// merge) stays `Unknown` so the UI can stay silent rather than guess.
pub fn parse_mergeable(raw: &str) -> Mergeable {
    match raw.to_ascii_uppercase().as_str() {
        "MERGEABLE" => Mergeable::Clean,
        "CONFLICTING" => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    }
}

/// The JSON body for [`submit_review_args`]: the verdict, the summary text, and
/// the inline comments, each anchored to a line on the PR's new (RIGHT) side.
///
/// Only line-anchored comments are included. The reviews endpoint has no
/// file-level comments — `subject_type` belongs to the standalone comment API,
/// and a comment with neither `line` nor `position` fails the whole review with
/// HTTP 422 — so a line-less draft is the caller's to fold into `body` first
/// (see [`crate::diff::fold_unanchorable`]).
pub fn review_payload(event: ReviewEvent, body: &str, comments: &[DraftComment]) -> Value {
    let comments_json: Vec<Value> = comments
        .iter()
        .filter_map(|c| {
            let line = c.line?;
            Some(serde_json::json!({
                "path": c.path, "line": line, "side": "RIGHT", "body": c.body,
            }))
        })
        .collect();
    serde_json::json!({
        "event": event.api_value(),
        "body": body,
        "comments": comments_json,
    })
}

/// Submit a review on a PR. The JSON body (event + inline comments) is passed on
/// stdin via `--input -`, since a nested `comments[]` array can't go through
/// repeated `-f` flags.
pub fn submit_review_args(pr_number: u64) -> Vec<String> {
    vec![
        "api".into(),
        "--method".into(),
        "POST".into(),
        format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/reviews"),
        "--input".into(),
        "-".into(),
    ]
}

/// GraphQL to list a PR's review threads with each thread's id, resolved flag,
/// and its comments' database ids + whether the authenticated user wrote them —
/// enough to map a fixed comment back to its thread AND to tell an answered
/// thread (we replied, or it's resolved) from one still awaiting us. Resolving
/// is GraphQL-only (no REST endpoint) and keyed by the *thread* node id, not
/// the comment id.
const REVIEW_THREADS_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!){\
repository(owner:$owner,name:$repo){pullRequest(number:$number){\
reviewThreads(first:100){nodes{id isResolved comments(first:100){nodes{databaseId viewerDidAuthor}}}}}}}";

/// Parse the review-threads GraphQL response into [`ReviewThread`]s. A thread
/// node without an id is dropped (nothing could be done with it); missing
/// booleans default to the conservative side (`resolved: false`,
/// `last_by_viewer: false` — i.e. "still awaiting us").
pub fn parse_review_threads(value: &Value) -> Vec<ReviewThread> {
    let nodes = value
        .pointer("/data/repository/pullRequest/reviewThreads/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    nodes
        .iter()
        .filter_map(|t| {
            let id = t.get("id")?.as_str()?.to_string();
            let resolved = t
                .get("isResolved")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let comments = t
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let comment_ids = comments
                .iter()
                .filter_map(|c| c.get("databaseId").and_then(Value::as_u64))
                .collect();
            // Keyed off the last comment *node* (not the last parsed id), so a
            // trailing node with a null databaseId still decides who spoke last.
            let last_by_viewer = comments
                .last()
                .and_then(|c| c.get("viewerDidAuthor"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(ReviewThread {
                id,
                resolved,
                comment_ids,
                last_by_viewer,
            })
        })
        .collect()
}

/// GraphQL mutation to resolve one review thread by its node id.
const RESOLVE_THREAD_MUTATION: &str =
    "mutation($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){thread{id isResolved}}}";

/// `gh` args for the review-threads query. GraphQL (unlike the REST helpers)
/// can't use `{owner}/{repo}` templating, so owner/repo are passed explicitly.
pub fn review_threads_query_args(owner: &str, repo: &str, pr_number: u64) -> Vec<String> {
    vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={REVIEW_THREADS_QUERY}"),
        "-f".into(),
        format!("owner={owner}"),
        "-f".into(),
        format!("repo={repo}"),
        // `-F` sends a typed value, so `number` reaches GraphQL as an Int (not a
        // string) to satisfy the `Int!` variable.
        "-F".into(),
        format!("number={pr_number}"),
    ]
}

/// `gh` args to resolve a single review thread by node id.
pub fn resolve_thread_args(thread_id: &str) -> Vec<String> {
    vec![
        "api".into(),
        "graphql".into(),
        "-f".into(),
        format!("query={RESOLVE_THREAD_MUTATION}"),
        "-f".into(),
        format!("threadId={thread_id}"),
    ]
}

/// `gh` args to read the repo's `owner/name` (needed for the GraphQL query).
pub fn name_with_owner_args() -> Vec<String> {
    vec![
        "repo".into(),
        "view".into(),
        "--json".into(),
        "nameWithOwner".into(),
        "--jq".into(),
        ".nameWithOwner".into(),
    ]
}

// --- trait (real + simulated) ----------------------------------------------

#[async_trait]
pub trait Forge: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn create_pr(
        &self,
        repo: &Path,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
        reviewer: Option<&str>,
        draft: bool,
    ) -> Result<PrInfo>;

    async fn fetch_comments(&self, repo: &Path, pr_number: u64) -> Result<Vec<ReviewComment>>;

    /// Open PRs by any of `authors` that the current user hasn't yet reviewed.
    async fn list_review_prs(&self, repo: &Path, authors: &[String]) -> Result<Vec<PrSummary>>;

    /// Submit a review (a batch of inline comments + an overall verdict) on a PR.
    async fn submit_review(
        &self,
        repo: &Path,
        pr_number: u64,
        event: ReviewEvent,
        body: &str,
        comments: &[DraftComment],
    ) -> Result<()>;

    /// GitHub logins that can be requested as PR reviewers on this repo.
    async fn list_reviewers(&self, repo: &Path) -> Result<Vec<String>>;

    /// The latest submitted review per reviewer (who actually reviewed).
    async fn list_submitted_reviews(
        &self,
        repo: &Path,
        pr_number: u64,
    ) -> Result<Vec<ReviewSummary>>;

    /// Post a reply on a specific PR review comment.
    async fn reply_to_comment(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<()>;

    /// Flip a draft PR to ready-for-review.
    async fn mark_ready(&self, repo: &Path, pr_number: u64) -> Result<()>;

    /// Squash-merge the PR. Branch cleanup is the caller's job (see `merge_args`).
    async fn merge(&self, repo: &Path, pr_number: u64) -> Result<()>;

    /// Whether the PR is already merged on the forge. Used to recover a merge
    /// whose local cleanup failed after the merge itself landed.
    async fn is_merged(&self, repo: &Path, pr_number: u64) -> Result<bool>;

    /// Whether the PR still merges cleanly onto its base. Asked after a failed
    /// merge to recognize a conflict.
    async fn merge_status(&self, repo: &Path, pr_number: u64) -> Result<MergeStatus>;

    /// Delete the PR's head branch on the remote.
    async fn delete_remote_branch(&self, repo: &Path, branch: &str) -> Result<()>;

    /// Mark the review threads of the given comments *resolved* on the PR. Best
    /// effort: unknown or already-resolved threads are skipped. Returns how many
    /// threads were newly resolved.
    async fn resolve_threads(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_ids: &[u64],
    ) -> Result<usize>;

    /// List the PR's review threads (resolved flag, comment ids, who spoke
    /// last). This is what tells an answered comment from one still awaiting a
    /// reaction — the flat comment list can't.
    async fn list_threads(&self, repo: &Path, pr_number: u64) -> Result<Vec<ReviewThread>>;

    /// The PR's rolled-up CI state plus the failing checks, if any. Defaults to
    /// "no checks" so forges that don't model CI (the sim, test doubles) keep
    /// merging unimpeded.
    async fn pr_checks(
        &self,
        _repo: &Path,
        _pr_number: u64,
    ) -> Result<(CheckStatus, Vec<FailedCheck>)> {
        Ok((CheckStatus::None, Vec::new()))
    }

    /// The failed-step log of one GitHub Actions run. Best-effort context for
    /// the fixing agent; the default has nothing to offer.
    async fn failed_run_log(&self, _repo: &Path, _run_id: u64) -> Result<String> {
        Ok(String::new())
    }

    /// The PR's live lifecycle state on the forge. `Ok(None)` means "can't
    /// tell" and is the default, so forges that don't model it (the sim, test
    /// doubles) leave every card and task untouched. A transport failure is an
    /// `Err`, never `Closed` — a network hiccup must not move or tear down
    /// anything.
    async fn pr_live_state(&self, _repo: &Path, _pr_number: u64) -> Result<Option<LivePrState>> {
        Ok(None)
    }

    /// The open PR whose head branch is `head`, if one exists. Best-effort
    /// dialog context (the adopt probe's "open PR" warning): "no PR" and "can't
    /// tell" both come back `None`, so forges that don't model it — the sim,
    /// test doubles — need no override.
    async fn pr_for_head(&self, _repo: &Path, _head: &str) -> Result<Option<PrInfo>> {
        Ok(None)
    }
}

/// Real GitHub forge via the `gh` CLI.
pub struct GhForge;

#[async_trait]
impl Forge for GhForge {
    async fn create_pr(
        &self,
        repo: &Path,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
        reviewer: Option<&str>,
        draft: bool,
    ) -> Result<PrInfo> {
        let out = run_gh(
            repo,
            &create_pr_args(title, body, base, head, reviewer, draft),
        )
        .await?;
        // `gh pr create` prints the PR URL, but may emit tips/warnings on other
        // lines — pick the line that is the PR URL and parse its trailing number.
        let url = out
            .lines()
            .map(str::trim)
            .rfind(|l| l.contains("/pull/"))
            .unwrap_or_else(|| out.trim())
            .to_string();
        let number = match url.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()) {
            Some(n) => n,
            // Couldn't parse a number from the output — ask gh authoritatively
            // rather than silently proceeding with PR #0.
            None => {
                let json = run_gh(repo, &pr_view_args(head)).await?;
                serde_json::from_str::<Value>(&json)?
                    .get("number")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| CoreError::forge("could not determine PR number"))?
            }
        };
        Ok(PrInfo {
            number,
            url,
            title: title.to_string(),
            state: if draft { "draft" } else { "open" }.to_string(),
            reviewer: normalize_reviewer(reviewer),
            reviewer_recorded: true,
        })
    }

    async fn fetch_comments(&self, repo: &Path, pr_number: u64) -> Result<Vec<ReviewComment>> {
        let json = run_gh(repo, &comments_args(pr_number)).await?;
        let value: Value = serde_json::from_str(&json)?;
        let arr = value.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .map(|c| ReviewComment {
                id: c.get("id").and_then(Value::as_u64).unwrap_or(0),
                author: c
                    .pointer("/user/login")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                path: c
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                line: c.get("line").and_then(Value::as_u64),
                body: c
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect())
    }

    async fn list_review_prs(&self, repo: &Path, authors: &[String]) -> Result<Vec<PrSummary>> {
        let authors: Vec<String> = authors.iter().filter(|a| !a.is_empty()).cloned().collect();
        if authors.is_empty() {
            return Ok(Vec::new());
        }
        let json = run_gh(repo, &review_prs_args(&authors)).await?;
        let value: Value = serde_json::from_str(&json)?;
        let arr = value.as_array().cloned().unwrap_or_default();
        Ok(arr
            .iter()
            .map(|p| PrSummary {
                number: p.get("number").and_then(Value::as_u64).unwrap_or(0),
                title: p
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                author: p
                    .pointer("/author/login")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                head_ref: p
                    .get("headRefName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                base_ref: p
                    .get("baseRefName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: p
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                body: p
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                checks: p
                    .get("statusCheckRollup")
                    .map(rollup_status)
                    .unwrap_or_default(),
                mergeable: p
                    .get("mergeable")
                    .and_then(Value::as_str)
                    .map(parse_mergeable)
                    .unwrap_or_default(),
            })
            .filter(|p| p.number != 0)
            .collect())
    }

    async fn submit_review(
        &self,
        repo: &Path,
        pr_number: u64,
        event: ReviewEvent,
        body: &str,
        comments: &[DraftComment],
    ) -> Result<()> {
        let stdin = serde_json::to_string(&review_payload(event, body, comments))?;
        run_gh_stdin(repo, &submit_review_args(pr_number), &stdin)
            .await
            .map(|_| ())
    }

    async fn list_reviewers(&self, repo: &Path) -> Result<Vec<String>> {
        let out = run_gh(repo, &reviewers_args()).await?;
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    async fn list_submitted_reviews(
        &self,
        repo: &Path,
        pr_number: u64,
    ) -> Result<Vec<ReviewSummary>> {
        let json = run_gh(repo, &submitted_reviews_args(pr_number)).await?;
        let value: Value = serde_json::from_str(&json)?;
        let arr = value
            .get("latestReviews")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(arr
            .iter()
            .map(|r| ReviewSummary {
                author: r
                    .pointer("/author/login")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                state: r
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
            .collect())
    }

    async fn reply_to_comment(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        run_gh(repo, &reply_args(pr_number, comment_id, body))
            .await
            .map(|_| ())
    }

    async fn mark_ready(&self, repo: &Path, pr_number: u64) -> Result<()> {
        run_gh(repo, &mark_ready_args(pr_number)).await.map(|_| ())
    }

    async fn merge(&self, repo: &Path, pr_number: u64) -> Result<()> {
        run_gh(repo, &merge_args(pr_number)).await.map(|_| ())
    }

    async fn is_merged(&self, repo: &Path, pr_number: u64) -> Result<bool> {
        let out = run_gh(repo, &pr_state_args(pr_number)).await?;
        Ok(out.trim() == "MERGED")
    }

    async fn merge_status(&self, repo: &Path, pr_number: u64) -> Result<MergeStatus> {
        let out = run_gh(repo, &pr_mergeable_args(pr_number)).await?;
        Ok(parse_merge_status(&out))
    }

    async fn delete_remote_branch(&self, repo: &Path, branch: &str) -> Result<()> {
        run_gh(repo, &delete_remote_branch_args(branch))
            .await
            .map(|_| ())
    }

    async fn resolve_threads(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_ids: &[u64],
    ) -> Result<usize> {
        if comment_ids.is_empty() {
            return Ok(0);
        }
        // Map the fixed comments to their thread node ids.
        let threads = self.list_threads(repo, pr_number).await?;
        let wanted: std::collections::HashSet<u64> = comment_ids.iter().copied().collect();
        let mut resolved = 0usize;
        for t in &threads {
            if t.resolved || !t.comment_ids.iter().any(|id| wanted.contains(id)) {
                continue;
            }
            // Tolerate a single thread failing (e.g. lost a permission race) so one
            // bad thread doesn't abort resolving the rest.
            if run_gh(repo, &resolve_thread_args(&t.id)).await.is_ok() {
                resolved += 1;
            }
        }
        Ok(resolved)
    }

    async fn list_threads(&self, repo: &Path, pr_number: u64) -> Result<Vec<ReviewThread>> {
        // GraphQL needs owner/name explicitly (no REST-style templating).
        let nwo = run_gh(repo, &name_with_owner_args()).await?;
        let nwo = nwo.trim();
        let (owner, name) = nwo
            .split_once('/')
            .ok_or_else(|| CoreError::other(format!("unexpected repo name from gh: {nwo:?}")))?;
        let json = run_gh(repo, &review_threads_query_args(owner, name, pr_number)).await?;
        Ok(parse_review_threads(&serde_json::from_str(&json)?))
    }

    async fn pr_checks(
        &self,
        repo: &Path,
        pr_number: u64,
    ) -> Result<(CheckStatus, Vec<FailedCheck>)> {
        let json = run_gh(repo, &pr_checks_args(pr_number)).await?;
        let value: Value = serde_json::from_str(&json)?;
        let rollup = value
            .get("statusCheckRollup")
            .cloned()
            .unwrap_or(Value::Null);
        Ok((rollup_status(&rollup), rollup_failures(&rollup)))
    }

    async fn failed_run_log(&self, repo: &Path, run_id: u64) -> Result<String> {
        run_gh(repo, &run_log_args(run_id)).await
    }

    async fn pr_live_state(&self, repo: &Path, pr_number: u64) -> Result<Option<LivePrState>> {
        let json = run_gh(repo, &pr_live_state_args(pr_number)).await?;
        Ok(parse_live_pr_state(&json))
    }

    async fn pr_for_head(&self, repo: &Path, head: &str) -> Result<Option<PrInfo>> {
        // `gh pr view <branch>` exits non-zero when the branch has no PR — the
        // common case here, folded into `None` along with genuine failures
        // (offline, unauthed): the caller only wants a best-effort warning.
        let Ok(json) = run_gh(repo, &pr_for_head_args(head)).await else {
            return Ok(None);
        };
        let v: Value = serde_json::from_str(&json)?;
        let number = v.get("number").and_then(Value::as_u64).unwrap_or(0);
        if number == 0 {
            return Ok(None);
        }
        let text = |key: &str| {
            v.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        // A closed/merged PR is history, not a conflict worth warning about.
        let state = text("state");
        if !state.eq_ignore_ascii_case("open") {
            return Ok(None);
        }
        Ok(Some(PrInfo {
            number,
            url: text("url"),
            title: text("title"),
            state: state.to_lowercase(),
            reviewer: None,
            reviewer_recorded: false,
        }))
    }
}

/// Simulated forge for Phase A: canned PR + review comments so the PR-review
/// column is fully navigable without GitHub.
pub struct SimForge;

#[async_trait]
impl Forge for SimForge {
    async fn create_pr(
        &self,
        _repo: &Path,
        title: &str,
        _body: &str,
        _base: &str,
        _head: &str,
        reviewer: Option<&str>,
        draft: bool,
    ) -> Result<PrInfo> {
        Ok(PrInfo {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: title.to_string(),
            state: if draft { "draft" } else { "open" }.to_string(),
            reviewer: normalize_reviewer(reviewer),
            reviewer_recorded: true,
        })
    }

    async fn fetch_comments(&self, _repo: &Path, _pr_number: u64) -> Result<Vec<ReviewComment>> {
        Ok(vec![
            ReviewComment {
                id: 1,
                author: "reviewer".into(),
                path: "src/lib.rs".into(),
                line: Some(12),
                body: "Consider extracting this into a helper function.".into(),
            },
            ReviewComment {
                id: 2,
                author: "reviewer".into(),
                path: "src/main.rs".into(),
                line: Some(48),
                body: "Nit: typo in this comment.".into(),
            },
            ReviewComment {
                id: 3,
                author: "reviewer".into(),
                path: "src/db.rs".into(),
                line: Some(5),
                body: "This `unwrap()` could panic on malformed input.".into(),
            },
        ])
    }

    async fn list_review_prs(&self, _repo: &Path, _authors: &[String]) -> Result<Vec<PrSummary>> {
        Ok(vec![
            PrSummary {
                number: 101,
                title: "Add caching layer".into(),
                author: "octocat".into(),
                head_ref: "feat/cache".into(),
                base_ref: "main".into(),
                url: "https://github.com/example/repo/pull/101".into(),
                body: "Adds an LRU in front of the resolver so repeated lookups \
                       stop hitting the database.\n\n- bounded at 10k entries\n\
                       - invalidated on write"
                    .into(),
                checks: CheckStatus::Passing,
                mergeable: Mergeable::Clean,
            },
            PrSummary {
                number: 102,
                title: "Fix flaky integration test".into(),
                author: "hubot".into(),
                head_ref: "fix/flaky".into(),
                base_ref: "main".into(),
                url: "https://github.com/example/repo/pull/102".into(),
                body: "The fixture raced the seeder; awaits it explicitly now.".into(),
                checks: CheckStatus::Failing,
                mergeable: Mergeable::Conflicting,
            },
        ])
    }

    async fn submit_review(
        &self,
        _repo: &Path,
        _pr_number: u64,
        _event: ReviewEvent,
        _body: &str,
        _comments: &[DraftComment],
    ) -> Result<()> {
        Ok(())
    }

    async fn list_reviewers(&self, _repo: &Path) -> Result<Vec<String>> {
        Ok(vec!["octocat".into(), "hubot".into(), "monalisa".into()])
    }

    async fn list_submitted_reviews(
        &self,
        _repo: &Path,
        _pr_number: u64,
    ) -> Result<Vec<ReviewSummary>> {
        Ok(vec![
            ReviewSummary {
                author: "octocat".into(),
                state: "CHANGES_REQUESTED".into(),
            },
            ReviewSummary {
                author: "gemini-code-assist".into(),
                state: "COMMENTED".into(),
            },
        ])
    }

    async fn reply_to_comment(
        &self,
        _repo: &Path,
        _pr_number: u64,
        _comment_id: u64,
        _body: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn mark_ready(&self, _repo: &Path, _pr_number: u64) -> Result<()> {
        Ok(())
    }

    async fn merge(&self, _repo: &Path, _pr_number: u64) -> Result<()> {
        Ok(())
    }

    async fn is_merged(&self, _repo: &Path, _pr_number: u64) -> Result<bool> {
        Ok(true)
    }

    async fn merge_status(&self, _repo: &Path, _pr_number: u64) -> Result<MergeStatus> {
        Ok(MergeStatus::Mergeable)
    }

    async fn delete_remote_branch(&self, _repo: &Path, _branch: &str) -> Result<()> {
        Ok(())
    }

    async fn resolve_threads(
        &self,
        _repo: &Path,
        _pr_number: u64,
        _comment_ids: &[u64],
    ) -> Result<usize> {
        Ok(0)
    }

    async fn list_threads(&self, _repo: &Path, _pr_number: u64) -> Result<Vec<ReviewThread>> {
        // One unresolved, reviewer-last thread per canned comment, so the sim
        // triage sees exactly the comments `fetch_comments` returns.
        Ok((1..=3)
            .map(|id| ReviewThread {
                id: format!("SIMTHREAD_{id}"),
                resolved: false,
                comment_ids: vec![id],
                last_by_viewer: false,
            })
            .collect())
    }
}

/// The message for a non-zero `gh` exit. Includes stdout as well as stderr:
/// on an API error `gh api` prints only a terse `gh: <status> (HTTP nnn)` to
/// stderr and puts the response body — the part that names *which* input the
/// API refused — on stdout. Dropping stdout turns a self-explanatory 422 into
/// a mystery.
fn gh_failure_message(args: &[String], stdout: &[u8], stderr: &[u8]) -> String {
    let mut msg = format!(
        "gh {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(stderr).trim()
    );
    let body = String::from_utf8_lossy(stdout);
    let body = body.trim();
    if !body.is_empty() {
        msg.push_str(" — ");
        msg.push_str(body);
    }
    msg
}

async fn run_gh(cwd: &Path, args: &[String]) -> Result<String> {
    let out = timeout(
        GH_TIMEOUT,
        Command::new("gh").current_dir(cwd).args(args).output(),
    )
    .await
    .map_err(|_| {
        CoreError::forge(format!(
            "gh {} timed out after {}s",
            args.join(" "),
            GH_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| CoreError::forge(format!("failed to run gh (is it installed?): {e}")))?;
    if !out.status.success() {
        return Err(CoreError::forge(gh_failure_message(
            args,
            &out.stdout,
            &out.stderr,
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Like [`run_gh`] but pipes `stdin` into the process (for `gh api --input -`).
async fn run_gh_stdin(cwd: &Path, args: &[String], stdin: &str) -> Result<String> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new("gh")
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CoreError::forge(format!("failed to run gh (is it installed?): {e}")))?;
    // Write the body and close stdin so gh stops reading.
    if let Some(mut si) = child.stdin.take() {
        si.write_all(stdin.as_bytes())
            .await
            .map_err(|e| CoreError::forge(format!("failed to write gh stdin: {e}")))?;
    }
    let out = timeout(GH_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            CoreError::forge(format!(
                "gh {} timed out after {}s",
                args.join(" "),
                GH_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| CoreError::forge(format!("failed to run gh (is it installed?): {e}")))?;
    if !out.status.success() {
        return Err(CoreError::forge(gh_failure_message(
            args,
            &out.stdout,
            &out.stderr,
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_pr_includes_reviewer_when_present() {
        let args = create_pr_args("T", "B", "main", "usine/x", Some("octocat"), false);
        assert!(args.windows(2).any(|w| w == ["--reviewer", "octocat"]));
        assert!(args.windows(2).any(|w| w == ["--base", "main"]));
        assert!(args.windows(2).any(|w| w == ["--head", "usine/x"]));
    }

    #[test]
    fn normalize_reviewer_trims_and_drops_empty() {
        assert_eq!(
            normalize_reviewer(Some("octocat")).as_deref(),
            Some("octocat")
        );
        assert_eq!(
            normalize_reviewer(Some("  octocat  ")).as_deref(),
            Some("octocat")
        );
        assert_eq!(normalize_reviewer(Some("")), None);
        assert_eq!(normalize_reviewer(Some("   ")), None);
        assert_eq!(normalize_reviewer(None), None);
    }

    #[test]
    fn resolve_thread_args_pass_query_and_thread_id() {
        let args = resolve_thread_args("PRRT_kwABC");
        assert_eq!(args[0], "api");
        assert_eq!(args[1], "graphql");
        assert!(args.iter().any(|a| a == "threadId=PRRT_kwABC"));
        assert!(args.iter().any(|a| a.starts_with("query=mutation")));
    }

    #[test]
    fn review_threads_query_passes_number_as_typed_int() {
        let args = review_threads_query_args("galadrimteam", "fftir-thot", 335);
        assert_eq!(&args[0..2], ["api", "graphql"]);
        assert!(args.iter().any(|a| a == "owner=galadrimteam"));
        assert!(args.iter().any(|a| a == "repo=fftir-thot"));
        // `-F number=335` (typed) so GraphQL gets an Int, not a string.
        let i = args.iter().position(|a| a == "number=335").unwrap();
        assert_eq!(args[i - 1], "-F");
    }

    #[test]
    fn parse_review_threads_reads_resolution_and_last_speaker() {
        let json: Value = serde_json::from_str(
            r#"{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[
                {"id":"T_open","isResolved":false,"comments":{"nodes":[
                    {"databaseId":10,"viewerDidAuthor":false}]}},
                {"id":"T_replied","isResolved":false,"comments":{"nodes":[
                    {"databaseId":20,"viewerDidAuthor":false},
                    {"databaseId":21,"viewerDidAuthor":true}]}},
                {"id":"T_followup","isResolved":false,"comments":{"nodes":[
                    {"databaseId":30,"viewerDidAuthor":false},
                    {"databaseId":31,"viewerDidAuthor":true},
                    {"databaseId":32,"viewerDidAuthor":false}]}},
                {"id":"T_resolved","isResolved":true,"comments":{"nodes":[
                    {"databaseId":40,"viewerDidAuthor":false}]}},
                {"comments":{"nodes":[{"databaseId":50,"viewerDidAuthor":false}]}}
            ]}}}}}"#,
        )
        .unwrap();
        let threads = parse_review_threads(&json);
        // The id-less node is dropped; the rest come through in order.
        assert_eq!(threads.len(), 4);
        assert_eq!(threads[0].comment_ids, vec![10]);
        assert!(threads[0].is_unanswered());
        // Our reply answers the thread…
        assert!(threads[1].last_by_viewer);
        assert!(!threads[1].is_unanswered());
        // …until the reviewer follows up after it.
        assert!(threads[2].is_unanswered());
        assert_eq!(threads[2].comment_ids, vec![30, 31, 32]);
        // Resolved threads are answered regardless of who spoke last.
        assert!(threads[3].resolved);
        assert!(!threads[3].is_unanswered());
    }

    #[test]
    fn review_threads_query_asks_who_authored_each_comment() {
        // The unanswered-thread notion rides on `viewerDidAuthor`; losing it
        // from the query would silently mark every thread unanswered.
        assert!(REVIEW_THREADS_QUERY.contains("viewerDidAuthor"));
        assert!(REVIEW_THREADS_QUERY.contains("isResolved"));
        assert!(REVIEW_THREADS_QUERY.contains("databaseId"));
    }

    #[test]
    fn name_with_owner_args_read_name_with_owner() {
        let args = name_with_owner_args();
        assert!(args.windows(2).any(|w| w == ["--json", "nameWithOwner"]));
        assert!(args.iter().any(|a| a == ".nameWithOwner"));
    }

    #[test]
    fn create_pr_omits_empty_reviewer() {
        let args = create_pr_args("T", "B", "main", "usine/x", None, false);
        assert!(!args.iter().any(|a| a == "--reviewer"));
        let args = create_pr_args("T", "B", "main", "usine/x", Some(""), false);
        assert!(!args.iter().any(|a| a == "--reviewer"));
    }

    #[test]
    fn create_pr_adds_draft_flag_only_when_requested() {
        let args = create_pr_args("T", "B", "main", "usine/x", None, true);
        assert!(args.iter().any(|a| a == "--draft"));
        let args = create_pr_args("T", "B", "main", "usine/x", None, false);
        assert!(!args.iter().any(|a| a == "--draft"));
    }

    #[test]
    fn review_prs_request_body_and_check_fields() {
        let args = review_prs_args(&["octocat".into()]);
        let fields = args
            .iter()
            .find(|a| a.contains("headRefName"))
            .expect("--json field list");
        for f in ["body", "statusCheckRollup", "mergeable"] {
            assert!(fields.contains(f), "missing {f} in {fields}");
        }
    }

    #[test]
    fn rollup_is_none_when_no_checks_ran() {
        assert_eq!(rollup_status(&serde_json::json!([])), CheckStatus::None);
        assert_eq!(rollup_status(&Value::Null), CheckStatus::None);
    }

    #[test]
    fn rollup_passes_only_when_every_check_settled_green() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SKIPPED"},
            {"__typename": "StatusContext", "state": "SUCCESS"},
        ]);
        assert_eq!(rollup_status(&rollup), CheckStatus::Passing);
    }

    #[test]
    fn rollup_failure_outranks_pending_and_success() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "status": "IN_PROGRESS"},
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE"},
        ]);
        assert_eq!(rollup_status(&rollup), CheckStatus::Failing);
    }

    /// A still-running check must never let the board call a PR green.
    #[test]
    fn rollup_pending_outranks_success() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS"},
            {"__typename": "CheckRun", "status": "QUEUED"},
        ]);
        assert_eq!(rollup_status(&rollup), CheckStatus::Pending);
        let legacy = serde_json::json!([{"__typename": "StatusContext", "state": "PENDING"}]);
        assert_eq!(rollup_status(&legacy), CheckStatus::Pending);
    }

    /// The pre-merge checks read must go through `gh pr view` — `gh pr checks`
    /// exits non-zero on failing/pending checks, the very states being observed.
    #[test]
    fn pr_checks_read_the_rollup_via_pr_view() {
        let args = pr_checks_args(7);
        assert_eq!(args[..3], ["pr".to_string(), "view".into(), "7".into()]);
        assert!(args
            .windows(2)
            .any(|w| w == ["--json", "statusCheckRollup"]));
    }

    #[test]
    fn rollup_failures_extract_both_node_shapes() {
        let rollup = serde_json::json!([
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS",
             "name": "lint", "workflowName": "CI", "detailsUrl": "https://x/actions/runs/1/job/2"},
            {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "FAILURE",
             "name": "test", "workflowName": "CI",
             "detailsUrl": "https://github.com/o/r/actions/runs/42/job/7"},
            {"__typename": "StatusContext", "state": "ERROR",
             "context": "ci/build", "targetUrl": "https://ci.example.com/b/9"},
            {"__typename": "CheckRun", "status": "IN_PROGRESS", "name": "e2e"},
        ]);
        let failed = rollup_failures(&rollup);
        assert_eq!(
            failed,
            vec![
                FailedCheck {
                    name: "test".into(),
                    workflow: "CI".into(),
                    url: "https://github.com/o/r/actions/runs/42/job/7".into(),
                },
                FailedCheck {
                    name: "ci/build".into(),
                    workflow: "".into(),
                    url: "https://ci.example.com/b/9".into(),
                },
            ]
        );
        assert!(rollup_failures(&Value::Null).is_empty());
    }

    /// The failing set and the rolled-up verdict must agree: exactly the
    /// outcomes that fail the rollup produce a `FailedCheck`.
    #[test]
    fn rollup_failures_match_the_rollup_verdict() {
        for outcome in [
            "FAILURE",
            "TIMED_OUT",
            "CANCELLED",
            "ERROR",
            "STARTUP_FAILURE",
        ] {
            let rollup = serde_json::json!([
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": outcome, "name": "c"},
            ]);
            assert_eq!(rollup_status(&rollup), CheckStatus::Failing, "{outcome}");
            assert_eq!(rollup_failures(&rollup).len(), 1, "{outcome}");
        }
    }

    #[test]
    fn run_id_is_parsed_only_from_actions_urls() {
        assert_eq!(
            run_id_from_url("https://github.com/o/r/actions/runs/123456/job/789"),
            Some(123456)
        );
        assert_eq!(
            run_id_from_url("https://github.com/o/r/actions/runs/42"),
            Some(42)
        );
        assert_eq!(run_id_from_url("https://ci.example.com/build/9"), None);
        assert_eq!(run_id_from_url(""), None);
    }

    #[test]
    fn run_log_asks_for_the_failed_steps_only() {
        assert_eq!(run_log_args(42), ["run", "view", "42", "--log-failed"]);
    }

    #[test]
    fn mergeable_maps_only_the_definitive_answers() {
        assert_eq!(parse_mergeable("MERGEABLE"), Mergeable::Clean);
        assert_eq!(parse_mergeable("CONFLICTING"), Mergeable::Conflicting);
        assert_eq!(parse_mergeable("UNKNOWN"), Mergeable::Unknown);
        assert_eq!(parse_mergeable(""), Mergeable::Unknown);
    }

    #[test]
    fn reviewers_uses_collaborators_endpoint() {
        let args = reviewers_args();
        assert_eq!(args[0], "api");
        assert!(args
            .iter()
            .any(|a| a == "repos/{owner}/{repo}/collaborators"));
        assert!(args.windows(2).any(|w| w == ["--jq", ".[].login"]));
    }

    #[test]
    fn comments_uses_gh_api_placeholders() {
        let args = comments_args(7);
        assert_eq!(args[0], "api");
        assert_eq!(args[1], "repos/{owner}/{repo}/pulls/7/comments");
    }

    #[test]
    fn mark_ready_targets_the_pr() {
        assert_eq!(mark_ready_args(7), vec!["pr", "ready", "7"]);
    }

    /// `--delete-branch` makes gh delete the local branch, which fails while the
    /// card's worktree holds it — and takes the whole merge down with it. The
    /// branch cleanup belongs to the executor, after the worktree is removed.
    #[test]
    fn merge_never_asks_gh_to_delete_the_branch() {
        let args = merge_args(7);
        assert_eq!(args, vec!["pr", "merge", "7", "--squash"]);
        assert!(!args.iter().any(|a| a == "--delete-branch"));
    }

    #[test]
    fn pr_state_reads_the_state_field() {
        let args = pr_state_args(7);
        assert_eq!(&args[0..3], &["pr", "view", "7"]);
        assert!(args.windows(2).any(|w| w == ["--json", "state"]));
        assert!(args.windows(2).any(|w| w == ["--jq", ".state"]));
    }

    #[test]
    fn pr_live_state_reads_state_and_draft_together() {
        let args = pr_live_state_args(7);
        assert_eq!(&args[0..3], &["pr", "view", "7"]);
        assert!(args.windows(2).any(|w| w == ["--json", "state,isDraft"]));
    }

    /// Only the three known lifecycle states are committed to — anything else
    /// (including unparseable output) is `None`, never read as "closed".
    #[test]
    fn live_pr_state_parses_only_the_known_states() {
        assert_eq!(
            parse_live_pr_state(r#"{"state":"OPEN","isDraft":false}"#),
            Some(LivePrState::Open { draft: false })
        );
        assert_eq!(
            parse_live_pr_state(r#"{"state":"OPEN","isDraft":true}"#),
            Some(LivePrState::Open { draft: true })
        );
        assert_eq!(
            parse_live_pr_state(r#"{"state":"MERGED","isDraft":false}"#),
            Some(LivePrState::Merged)
        );
        assert_eq!(
            parse_live_pr_state(r#"{"state":"CLOSED","isDraft":false}"#),
            Some(LivePrState::Closed)
        );
        assert_eq!(parse_live_pr_state(r#"{"state":"WEIRD"}"#), None);
        assert_eq!(parse_live_pr_state("not json"), None);
        assert_eq!(parse_live_pr_state("{}"), None);
    }

    #[test]
    fn pr_mergeable_reads_the_mergeable_field() {
        let args = pr_mergeable_args(7);
        assert_eq!(&args[0..3], &["pr", "view", "7"]);
        assert!(args.windows(2).any(|w| w == ["--json", "mergeable"]));
        assert!(args.windows(2).any(|w| w == ["--jq", ".mergeable"]));
    }

    /// Only GitHub's two decided answers are decided here. `UNKNOWN` (GitHub is
    /// still computing mergeability) and anything unexpected must not be read as
    /// "no conflict" — that would silently swallow a conflicting PR.
    #[test]
    fn merge_status_only_commits_to_the_decided_answers() {
        assert_eq!(
            parse_merge_status("CONFLICTING\n"),
            MergeStatus::Conflicting
        );
        assert_eq!(parse_merge_status(" MERGEABLE "), MergeStatus::Mergeable);
        assert_eq!(parse_merge_status("UNKNOWN"), MergeStatus::Unknown);
        assert_eq!(parse_merge_status(""), MergeStatus::Unknown);
    }

    #[test]
    fn delete_remote_branch_targets_the_head_ref() {
        let args = delete_remote_branch_args("feat/licensee-deletion");
        assert!(args.windows(2).any(|w| w == ["--method", "DELETE"]));
        assert!(args
            .iter()
            .any(|a| a == "repos/{owner}/{repo}/git/refs/heads/feat/licensee-deletion"));
    }

    #[test]
    fn submitted_reviews_uses_latest_reviews() {
        let args = submitted_reviews_args(7);
        assert_eq!(&args[0..3], &["pr", "view", "7"]);
        assert!(args.windows(2).any(|w| w == ["--json", "latestReviews"]));
    }

    #[test]
    fn reply_posts_to_comment_replies_endpoint() {
        let args = reply_args(7, 55, "thanks");
        assert!(args.windows(2).any(|w| w == ["--method", "POST"]));
        assert!(args
            .iter()
            .any(|a| a == "repos/{owner}/{repo}/pulls/7/comments/55/replies"));
        assert!(args.windows(2).any(|w| w == ["-f", "body=thanks"]));
    }

    #[test]
    fn review_prs_search_ors_authors_and_excludes_reviewed_and_drafts() {
        let args = review_prs_args(&["alice".into(), "bob".into(), "".into()]);
        assert_eq!(&args[0..2], &["pr", "list"]);
        assert!(args.windows(2).any(|w| w == ["--state", "open"]));
        let search = args
            .iter()
            .skip_while(|a| *a != "--search")
            .nth(1)
            .expect("search term");
        assert!(search.contains("-reviewed-by:@me"));
        assert!(search.contains("-is:draft"));
        assert!(search.contains("author:alice"));
        assert!(search.contains("author:bob"));
        // Empty author is skipped, not emitted as a bare `author:`.
        assert!(!search.contains("author: "));
        assert!(!search.ends_with("author:"));
    }

    #[test]
    fn submit_review_posts_to_reviews_endpoint_via_stdin() {
        let args = submit_review_args(7);
        assert!(args.windows(2).any(|w| w == ["--method", "POST"]));
        assert!(args
            .iter()
            .any(|a| a == "repos/{owner}/{repo}/pulls/7/reviews"));
        assert!(args.windows(2).any(|w| w == ["--input", "-"]));
    }

    fn draft(path: &str, line: Option<u64>, body: &str) -> DraftComment {
        DraftComment {
            path: path.into(),
            line,
            body: body.into(),
            severity: String::new(),
            selected: true,
        }
    }

    #[test]
    fn review_payload_anchors_comments_on_the_right_side() {
        let payload = review_payload(
            ReviewEvent::RequestChanges,
            "summary",
            &[draft("src/a.rs", Some(12), "nit")],
        );
        assert_eq!(payload["event"], "REQUEST_CHANGES");
        assert_eq!(payload["body"], "summary");
        assert_eq!(
            payload["comments"][0],
            serde_json::json!({"path": "src/a.rs", "line": 12, "side": "RIGHT", "body": "nit"})
        );
    }

    /// The reviews endpoint rejects the whole review over a comment with no
    /// line (`subject_type` is not a thing there), so a line-less draft must
    /// never reach the payload — folding it into the body is the caller's job.
    #[test]
    fn review_payload_never_emits_line_less_comments() {
        let payload = review_payload(
            ReviewEvent::Comment,
            "s",
            &[
                draft("src/a.rs", Some(3), "inline"),
                draft("src/b.rs", None, "file-level"),
            ],
        );
        let comments = payload["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0]["line"], 3);
        assert!(!payload.to_string().contains("subject_type"));
    }

    /// `gh api` puts the API's error body (the *reason*) on stdout and only a
    /// terse status line on stderr — the message must carry both.
    #[test]
    fn gh_failure_message_includes_the_stdout_error_body() {
        let msg = gh_failure_message(
            &["api".into(), "x".into()],
            br#"{"message":"Unprocessable Entity","errors":["line must be part of the diff"]}"#,
            b"gh: Unprocessable Entity (HTTP 422)\n",
        );
        assert!(msg.contains("gh api x failed: gh: Unprocessable Entity (HTTP 422)"));
        assert!(msg.contains("line must be part of the diff"));
    }

    #[test]
    fn gh_failure_message_skips_an_empty_stdout() {
        let msg = gh_failure_message(&["pr".into()], b"", b"boom\n");
        assert_eq!(msg, "gh pr failed: boom");
    }
}
