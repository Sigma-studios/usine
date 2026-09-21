//! GitHub, through the `gh` CLI.
//!
//! We shell out to `gh`, reusing the user's existing `gh auth` — no token is
//! stored by this app. `gh` is used for create/merge; inline review comments
//! come from `gh api repos/{owner}/{repo}/pulls/<n>/comments` (the `{owner}`/
//! `{repo}` placeholders are auto-filled by `gh` from the repo's remote), since
//! `gh pr view` does not expose inline line comments.
//!
//! The argv builders and response parsers are pure and unit-tested; only
//! [`GhForge`] and the `run_gh*` helpers touch a process.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

use super::{
    normalize_reviewer, FailedCheck, Forge, LivePrState, OpenPr, PrPushTarget, PrSummary,
    ReviewScope,
};
use crate::domain::model::{
    CheckStatus, DraftComment, Mergeable, PrInfo, PrState, ReviewComment, ReviewEvent,
    ReviewSummary, ReviewThread,
};
use crate::error::{CoreError, Result};

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

/// Every inline review comment on the PR. `--paginate` because the endpoint
/// pages at 30 by default — without it a busy PR's later comments (and the
/// threads they belong to) silently never reached triage. `per_page=100`
/// keeps that to one request for nearly every PR. The pages come back as
/// back-to-back JSON arrays; see [`parse_paged_array`].
pub fn comments_args(pr_number: u64) -> Vec<String> {
    vec![
        "api".into(),
        "--paginate".into(),
        format!("repos/{{owner}}/{{repo}}/pulls/{pr_number}/comments?per_page=100"),
    ]
}

/// Flatten `gh api --paginate` output — one JSON array per page, written back
/// to back (`[…][…]`) — into one list. A single page parses the same way.
/// (`--slurp` would wrap the pages, but only newer `gh` releases have it.)
pub fn parse_paged_array(out: &str) -> Result<Vec<Value>> {
    let mut all = Vec::new();
    for page in serde_json::Deserializer::from_str(out).into_iter::<Value>() {
        match page? {
            Value::Array(items) => all.extend(items),
            other => all.push(other),
        }
    }
    Ok(all)
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

/// Read where a PR's head lives and whether maintainers may push to it.
pub fn pr_push_target_args(pr_number: u64) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        pr_number.to_string(),
        "--json".into(),
        "headRefName,baseRefName,isCrossRepository,headRepository,headRepositoryOwner,\
         maintainerCanModify"
            .into(),
    ]
}

/// Map `gh pr view --json headRefName,…` onto [`PrPushTarget`]. `None` for junk
/// or a payload with no head branch — "can't tell", never "not pushable": the
/// caller refuses to promise a fix rather than guessing either way.
pub fn parse_push_target(json: &str) -> Option<PrPushTarget> {
    let v: Value = serde_json::from_str(json).ok()?;
    let head_ref = v.get("headRefName").and_then(Value::as_str)?;
    if head_ref.is_empty() {
        return None;
    }
    // `headRepository` is the repo alone; its owner is a sibling field.
    let owner = v
        .pointer("/headRepositoryOwner/login")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = v
        .pointer("/headRepository/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let head_repo = if owner.is_empty() || name.is_empty() {
        String::new()
    } else {
        format!("{owner}/{name}")
    };
    Some(PrPushTarget {
        head_ref: head_ref.to_string(),
        base_ref: v
            .get("baseRefName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        cross_repo: v
            .get("isCrossRepository")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        head_repo,
        maintainer_can_modify: v
            .get("maintainerCanModify")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Post a plain (non-review) comment on a PR's conversation. The PR's issue
/// endpoint is the one that takes a bare body; `--input -` keeps a multi-line
/// body out of argv.
pub fn pr_comment_args(pr_number: u64) -> Vec<String> {
    vec![
        "api".into(),
        "--method".into(),
        "POST".into(),
        format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/comments"),
        "--input".into(),
        "-".into(),
    ]
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

/// Parse `gh pr view --json latestReviews` output into review summaries.
/// The `body` and `submittedAt` fields ride along with the verdict: a
/// body-only review (a bot report, or a human Comment review with no inline
/// comments) carries its entire content in `body`, and with no usable review
/// id in this payload, `author` + `submittedAt` is what identifies it (see
/// [`ReviewSummary::body_key`]).
pub fn parse_latest_reviews(value: &Value) -> Vec<ReviewSummary> {
    let arr = value
        .get("latestReviews")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    arr.iter()
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
            body: r
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            submitted_at: r
                .get("submittedAt")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect()
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
/// already has a PR" warning, and `create_pr`'s recovery when gh fails after the
/// PR was opened. `--json` fields cover a full [`PrInfo`], including the draft
/// flag and who GitHub actually asked to review.
pub fn pr_for_head_args(head: &str) -> Vec<String> {
    vec![
        "pr".into(),
        "view".into(),
        head.into(),
        "--json".into(),
        "number,url,title,state,isDraft,reviewRequests".into(),
    ]
}

/// Every open PR on the repo, with what the adopt dialog lists and prefills.
/// `--limit 100` for the same reason as [`review_prs_args`]: the default of 30
/// would silently truncate a busy repo's listing.
pub fn open_prs_args() -> Vec<String> {
    vec![
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        "--limit".into(),
        "100".into(),
        "--json".into(),
        "number,title,author,headRefName,baseRefName,url,body,isDraft,isCrossRepository".into(),
    ]
}

/// Read an [`open_prs_args`] listing into [`OpenPr`]s, marking the ones
/// `viewer` authored (case-insensitively — GitHub logins are). Entries without
/// a number or a head branch are dropped: there is nothing to adopt.
pub fn parse_open_prs(value: &Value, viewer: &str) -> Vec<OpenPr> {
    let text = |p: &Value, key: &str| {
        p.pointer(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|p| {
            let author = text(p, "/author/login");
            OpenPr {
                number: p.get("number").and_then(Value::as_u64).unwrap_or(0),
                title: text(p, "/title"),
                mine: !viewer.is_empty() && author.eq_ignore_ascii_case(viewer),
                author,
                head_ref: text(p, "/headRefName"),
                base_ref: text(p, "/baseRefName"),
                url: text(p, "/url"),
                body: text(p, "/body"),
                draft: p.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
                cross_repo: p
                    .get("isCrossRepository")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        })
        .filter(|p| p.number != 0 && !p.head_ref.is_empty())
        .collect()
}

/// The signed-in user's login — kept out of the reviewer picker, since GitHub
/// refuses to let a PR's author review it.
pub fn viewer_login_args() -> Vec<String> {
    vec!["api".into(), "user".into(), "--jq".into(), ".login".into()]
}

/// Read a [`pr_for_head_args`] response into a [`PrInfo`]: `None` unless it is
/// an open PR. `reviewer` is the first *user* review request (team requests
/// carry no login); `reviewer_recorded` is left for the caller to decide.
pub fn parse_pr_for_head(v: &Value) -> Option<PrInfo> {
    let number = v.get("number").and_then(Value::as_u64).unwrap_or(0);
    if number == 0 {
        return None;
    }
    let text = |key: &str| {
        v.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    // A closed/merged PR is history, not a conflict worth warning about.
    if !text("state").eq_ignore_ascii_case("open") {
        return None;
    }
    let draft = v.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let reviewer = v
        .get("reviewRequests")
        .and_then(Value::as_array)
        .and_then(|reqs| {
            reqs.iter()
                .filter_map(|r| r.get("login").and_then(Value::as_str))
                .find(|l| !l.is_empty())
        })
        .map(str::to_string);
    Some(PrInfo {
        number,
        url: text("url"),
        title: text("title"),
        state: PrState::open(draft),
        reviewer,
        reviewer_recorded: false,
    })
}

/// A GitHub login as typed by a human, or `None` if it can't be one.
///
/// Everything downstream interpolates the login straight into a `gh --search`
/// query, where a space turns the rest of the input into a free-text term that
/// ANDs with the whole query — so "Nathan FCG" or a pasted profile URL would
/// silently return nothing instead of that person's PRs. Accepted shapes are a
/// bare login, `@login`, and any `github.com/login` URL; the result is checked
/// against GitHub's own rule (alphanumerics and single inner hyphens, at most
/// 39 characters) so nothing else can ever reach a query.
pub fn normalize_login(input: &str) -> Option<String> {
    let raw = input.trim();
    // A pasted profile URL: keep the first path segment after the host, so
    // `https://github.com/foo/bar/pull/1` and `github.com/foo` both give `foo`.
    let raw = match raw.split_once("github.com/") {
        Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or(""),
        None => raw,
    };
    let login = raw.trim().trim_start_matches('@').trim();
    let valid = !login.is_empty()
        && login.len() <= 39
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--");
    valid.then(|| login.to_string())
}

/// Open PRs in `scope` that the current user hasn't yet reviewed.
/// [`ReviewScope::Authors`] ORs `author:` qualifiers together;
/// [`ReviewScope::Everyone`] excludes the current user's own PRs instead.
/// `-reviewed-by:@me` is how "PRs I haven't reviewed" is expressed; drafts are
/// excluded. Scoped to the current repo by `gh pr list`.
pub fn review_prs_args(scope: &ReviewScope) -> Vec<String> {
    let mut search = String::from("-reviewed-by:@me -is:draft");
    match scope {
        ReviewScope::Authors(authors) => {
            // Anything that isn't a login is dropped rather than interpolated:
            // a stray space would AND a free-text term onto the whole query and
            // quietly empty the board (see [`normalize_login`]).
            for a in authors.iter().filter_map(|a| normalize_login(a)) {
                search.push_str(&format!(" author:{a}"));
            }
        }
        ReviewScope::Everyone => search.push_str(" -author:@me"),
    }
    vec![
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        // `gh pr list` caps a `--search` listing at 30 by default, which would
        // silently truncate the listing on a busy repo.
        "--limit".into(),
        "100".into(),
        // `body`, `statusCheckRollup` and `mergeable` ride along on the same
        // listing so the board can show intent and CI state without an extra
        // round-trip per PR.
        "--json".into(),
        "number,title,author,headRefName,baseRefName,url,body,statusCheckRollup,mergeable".into(),
        "--search".into(),
        search,
    ]
}

/// The open PRs whose authors the contributor picker suggests. Mirrors the
/// scan's own filters (open, not a draft, not mine) so the picker can't
/// advertise someone the scan would find nothing for — but deliberately *not*
/// `-reviewed-by:@me`: someone whose PR you already reviewed once is still
/// someone worth tracking from now on.
pub fn pr_authors_args() -> Vec<String> {
    vec![
        "pr".into(),
        "list".into(),
        "--state".into(),
        "open".into(),
        "--limit".into(),
        "100".into(),
        "--json".into(),
        "author".into(),
        "--search".into(),
        "-is:draft -author:@me".into(),
    ]
}

/// Distinct human logins out of a `gh pr list --json author` payload, busiest
/// author first (ties broken by login) so the picker leads with whoever has the
/// most PRs waiting. Bots are dropped and logins deduped case-insensitively —
/// GitHub logins are case-insensitive, and the same person spelled two ways
/// would otherwise show up twice.
pub fn parse_pr_authors(value: &Value) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for pr in value.as_array().map(Vec::as_slice).unwrap_or_default() {
        if pr.pointer("/author/is_bot").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(login) = pr.pointer("/author/login").and_then(Value::as_str) else {
            continue;
        };
        let login = login.trim();
        if login.is_empty() {
            continue;
        }
        match counts
            .iter_mut()
            .find(|(l, _)| l.eq_ignore_ascii_case(login))
        {
            Some((_, n)) => *n += 1,
            None => counts.push((login.to_string(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.into_iter().map(|(l, _)| l).collect()
}

/// Turn a `gh pr list` review listing into [`PrSummary`]s. `drop_bots` skips
/// bot-authored PRs — without it, "everyone" mode on a repo with dependabot or
/// renovate would flood the review board. An explicitly pinned author is always
/// honoured, so the author path never drops anything.
pub fn parse_review_prs(value: &Value, drop_bots: bool) -> Vec<PrSummary> {
    value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|p| {
            !drop_bots || p.pointer("/author/is_bot").and_then(Value::as_bool) != Some(true)
        })
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
        .collect()
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

/// Whether `repo` has GitHub Actions workflows at all. A repo with any workflow
/// gets check runs on a PR's head SHA — whether the workflow triggers on
/// `pull_request` or just on `push`, both land in the PR's `statusCheckRollup` —
/// so file existence is the right question to ask offline. Only a fallback: a
/// real observation ([`crate::ProjectConfig::ci_checks`]) always wins.
pub fn repo_has_workflows(repo: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(repo.join(".github/workflows")) else {
        return false;
    };
    entries.flatten().any(|e| {
        matches!(
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("yml") | Some("yaml")
        )
    })
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

/// How a failing check is named above its log in the fix prompt: the
/// workflow when it reports one (the unit a run's log covers), else the check.
pub fn check_display_name(check: &FailedCheck) -> String {
    if check.workflow.is_empty() {
        check.name.clone()
    } else {
        check.workflow.clone()
    }
}

/// Map GitHub's `mergeable` enum onto [`Mergeable`]. Anything other than the two
/// definitive answers (notably `UNKNOWN`, returned while GitHub computes the
/// merge, and the empty output of a PR that has vanished) stays `Unknown` so
/// callers stay silent rather than guess. Trimmed because [`Forge::merge_status`]
/// feeds it raw `gh --jq` output, newline and all.
pub fn parse_mergeable(raw: &str) -> Mergeable {
    match raw.trim().to_ascii_uppercase().as_str() {
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
        // Labelled rather than echoing argv: the args hold the whole PR body,
        // and this error reaches the user's toast.
        let out = run_gh_as(
            repo,
            &create_pr_args(title, body, base, head, reviewer, draft),
            "pr create",
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
            state: PrState::open(draft),
            reviewer: normalize_reviewer(reviewer),
            reviewer_recorded: true,
        })
    }

    async fn fetch_comments(&self, repo: &Path, pr_number: u64) -> Result<Vec<ReviewComment>> {
        let json = run_gh(repo, &comments_args(pr_number)).await?;
        let arr = parse_paged_array(&json)?;
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
                review_body_of: None,
            })
            .collect())
    }

    async fn list_review_prs(&self, repo: &Path, scope: ReviewScope) -> Result<Vec<PrSummary>> {
        // No pinned authors means nothing to search for — skip the gh call
        // entirely rather than issuing an unqualified listing.
        if let ReviewScope::Authors(authors) = &scope {
            if authors.iter().all(|a| a.is_empty()) {
                return Ok(Vec::new());
            }
        }
        let drop_bots = matches!(scope, ReviewScope::Everyone);
        let json = run_gh(repo, &review_prs_args(&scope)).await?;
        let value: Value = serde_json::from_str(&json)?;
        Ok(parse_review_prs(&value, drop_bots))
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
        // GitHub refuses the author as a PR's reviewer — and gh reports that
        // refusal only after opening the PR — so leave the signed-in user out.
        // Best-effort: if the login can't be read, offer the full list. Both
        // requests run concurrently so the picker waits on one round-trip.
        let (list_args, viewer_args) = (reviewers_args(), viewer_login_args());
        let (out, me) = tokio::join!(run_gh(repo, &list_args), run_gh(repo, &viewer_args));
        let out = out?;
        let me = me.map(|s| s.trim().to_string()).unwrap_or_default();
        Ok(out
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && (me.is_empty() || !l.eq_ignore_ascii_case(&me)))
            .collect())
    }

    async fn list_pr_authors(&self, repo: &Path) -> Result<Vec<String>> {
        let json = run_gh(repo, &pr_authors_args()).await?;
        let value: Value = serde_json::from_str(&json)?;
        Ok(parse_pr_authors(&value))
    }

    async fn list_submitted_reviews(
        &self,
        repo: &Path,
        pr_number: u64,
    ) -> Result<Vec<ReviewSummary>> {
        let json = run_gh(repo, &submitted_reviews_args(pr_number)).await?;
        let value: Value = serde_json::from_str(&json)?;
        Ok(parse_latest_reviews(&value))
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

    async fn merge_status(&self, repo: &Path, pr_number: u64) -> Result<Mergeable> {
        let out = run_gh(repo, &pr_mergeable_args(pr_number)).await?;
        Ok(parse_mergeable(&out))
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

    async fn failed_check_logs(
        &self,
        repo: &Path,
        failed: &[FailedCheck],
    ) -> Vec<(String, String)> {
        // One log per Actions run: several failing jobs of one workflow share
        // a run, and its `--log-failed` output already covers all of them. A
        // check whose URL isn't an Actions run (a third-party status) has no
        // log to fetch here.
        let mut logs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for check in failed {
            let Some(run_id) = run_id_from_url(&check.url) else {
                continue;
            };
            if !seen.insert(run_id) {
                continue;
            }
            match run_gh(repo, &run_log_args(run_id)).await {
                Ok(log) if !log.trim().is_empty() => logs.push((check_display_name(check), log)),
                Ok(_) => {}
                Err(e) => tracing::warn!("fix checks: couldn't fetch log of run {run_id}: {e}"),
            }
        }
        logs
    }

    async fn pr_live_state(&self, repo: &Path, pr_number: u64) -> Result<Option<LivePrState>> {
        let json = run_gh(repo, &pr_live_state_args(pr_number)).await?;
        Ok(parse_live_pr_state(&json))
    }

    async fn pr_push_target(&self, repo: &Path, pr_number: u64) -> Result<Option<PrPushTarget>> {
        let json = run_gh(repo, &pr_push_target_args(pr_number)).await?;
        Ok(parse_push_target(&json))
    }

    async fn comment_on_pr(&self, repo: &Path, pr_number: u64, body: &str) -> Result<()> {
        let stdin = serde_json::to_string(&serde_json::json!({ "body": body }))?;
        run_gh_stdin(repo, &pr_comment_args(pr_number), &stdin)
            .await
            .map(|_| ())
    }

    async fn pr_for_head(&self, repo: &Path, head: &str) -> Result<Option<PrInfo>> {
        // `gh pr view <branch>` exits non-zero when the branch has no PR — the
        // common case here, folded into `None` along with genuine failures
        // (offline, unauthed): the caller only wants a best-effort warning.
        let Ok(json) = run_gh(repo, &pr_for_head_args(head)).await else {
            return Ok(None);
        };
        let v: Value = serde_json::from_str(&json)?;
        Ok(parse_pr_for_head(&v))
    }

    async fn list_open_prs(&self, repo: &Path) -> Result<Vec<OpenPr>> {
        // The viewer's login only orders the list and flags "not yours", so a
        // failed lookup degrades to an unflagged listing rather than an error.
        let (list_args, viewer_args) = (open_prs_args(), viewer_login_args());
        let (out, me) = tokio::join!(run_gh(repo, &list_args), run_gh(repo, &viewer_args));
        let value: Value = serde_json::from_str(&out?)?;
        let me = me.map(|s| s.trim().to_string()).unwrap_or_default();
        let mut prs = parse_open_prs(&value, &me);
        // The user's own PRs first — the ones they most plausibly opened on
        // another machine. Stable, so gh's newest-first order holds within.
        prs.sort_by_key(|p| !p.mine);
        Ok(prs)
    }

    async fn pr_by_number(&self, repo: &Path, pr_number: u64) -> Result<Option<PrInfo>> {
        // Unlike `pr_for_head`, a failure here is an error, not "no PR": the
        // caller must tell "closed" from "couldn't ask". `gh pr view` takes a
        // number as readily as a branch.
        let json = run_gh(repo, &pr_for_head_args(&pr_number.to_string())).await?;
        let v: Value = serde_json::from_str(&json)?;
        Ok(parse_pr_for_head(&v))
    }
}

/// The message for a non-zero `gh` exit. Includes stdout as well as stderr:
/// on an API error `gh api` prints only a terse `gh: <status> (HTTP nnn)` to
/// stderr and puts the response body — the part that names *which* input the
/// API refused — on stdout. Dropping stdout turns a self-explanatory 422 into
/// a mystery.
///
/// `cmd` names the command in the message — usually its full argv, but callers
/// whose args carry bulky user text (a PR body) pass a short label instead.
fn gh_failure_message(cmd: &str, stdout: &[u8], stderr: &[u8]) -> String {
    let mut msg = format!(
        "gh {cmd} failed: {}",
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
    run_gh_as(cwd, args, &args.join(" ")).await
}

/// [`run_gh`], naming the command `cmd` in its errors (see [`gh_failure_message`]).
async fn run_gh_as(cwd: &Path, args: &[String], cmd: &str) -> Result<String> {
    // `kill_on_drop`: a timeout drops the future, and without it gh keeps
    // running — a timed-out `pr create` could still open the PR behind our back.
    let out = timeout(
        GH_TIMEOUT,
        Command::new("gh")
            .current_dir(cwd)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        CoreError::forge(format!(
            "gh {cmd} timed out after {}s",
            GH_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|e| CoreError::forge(format!("failed to run gh (is it installed?): {e}")))?;
    if !out.status.success() {
        return Err(CoreError::forge(gh_failure_message(
            cmd,
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
        .kill_on_drop(true)
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
            &args.join(" "),
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
    fn pr_for_head_args_request_draft_and_review_requests() {
        let args = pr_for_head_args("feat/x");
        assert_eq!(&args[0..3], ["pr", "view", "feat/x"]);
        let fields = args.last().unwrap();
        for f in [
            "number",
            "url",
            "title",
            "state",
            "isDraft",
            "reviewRequests",
        ] {
            assert!(fields.split(',').any(|x| x == f), "missing {f}");
        }
    }

    #[test]
    fn parse_pr_for_head_reads_an_open_pr_and_its_user_reviewer() {
        let v: Value = serde_json::from_str(
            r#"{"number":42,"url":"https://github.com/o/r/pull/42","title":"T",
                "state":"OPEN","isDraft":false,
                "reviewRequests":[{"__typename":"User","login":"octocat"}]}"#,
        )
        .unwrap();
        let pr = parse_pr_for_head(&v).unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.url, "https://github.com/o/r/pull/42");
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.reviewer.as_deref(), Some("octocat"));
        assert!(!pr.reviewer_recorded);
    }

    #[test]
    fn parse_pr_for_head_marks_drafts() {
        let v: Value = serde_json::from_str(
            r#"{"number":3,"url":"u","title":"T","state":"OPEN","isDraft":true,"reviewRequests":[]}"#,
        )
        .unwrap();
        let pr = parse_pr_for_head(&v).unwrap();
        assert_eq!(pr.state, PrState::Draft);
        assert_eq!(pr.reviewer, None);
    }

    #[test]
    fn parse_pr_for_head_skips_team_requests() {
        let v: Value = serde_json::from_str(
            r#"{"number":3,"url":"u","title":"T","state":"OPEN","isDraft":false,
                "reviewRequests":[{"__typename":"Team","name":"core","slug":"core"}]}"#,
        )
        .unwrap();
        assert_eq!(parse_pr_for_head(&v).unwrap().reviewer, None);
    }

    #[test]
    fn parse_open_prs_reads_forks_drafts_and_empty_bodies() {
        let v: Value = serde_json::from_str(
            r#"[
                {"number":7,"title":"Mine","author":{"login":"Me"},"headRefName":"feat/a",
                 "baseRefName":"main","url":"u7","body":"","isDraft":true,
                 "isCrossRepository":false},
                {"number":8,"title":"Fork","author":{"login":"other"},"headRefName":"patch-1",
                 "baseRefName":"main","url":"u8","body":"why","isDraft":false,
                 "isCrossRepository":true},
                {"number":0,"headRefName":"junk"},
                {"number":9,"headRefName":""}
            ]"#,
        )
        .unwrap();
        let prs = parse_open_prs(&v, "me");
        assert_eq!(prs.len(), 2, "entries without a number or head are dropped");
        assert_eq!(prs[0].head_ref, "feat/a");
        assert!(prs[0].draft && !prs[0].cross_repo);
        assert!(prs[0].body.is_empty());
        assert!(prs[0].mine, "author match is case-insensitive");
        assert!(prs[1].cross_repo && !prs[1].draft && !prs[1].mine);
        assert_eq!(prs[1].body, "why");
        // An unknown viewer flags nothing as mine.
        assert!(parse_open_prs(&v, "").iter().all(|p| !p.mine));
    }

    #[test]
    fn open_prs_args_list_open_prs_with_fork_and_draft_flags() {
        let args = open_prs_args();
        assert_eq!(&args[0..4], ["pr", "list", "--state", "open"]);
        assert!(args.windows(2).any(|w| w == ["--limit", "100"]));
        let fields = args.last().unwrap();
        for f in [
            "headRefName",
            "baseRefName",
            "isDraft",
            "isCrossRepository",
            "body",
        ] {
            assert!(fields.split(',').any(|x| x == f), "missing {f}");
        }
    }

    #[test]
    fn parse_pr_for_head_ignores_closed_prs() {
        let v: Value = serde_json::from_str(
            r#"{"number":3,"url":"u","title":"T","state":"MERGED","isDraft":false,"reviewRequests":[]}"#,
        )
        .unwrap();
        assert!(parse_pr_for_head(&v).is_none());
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
        let args = review_prs_args(&ReviewScope::Authors(vec!["octocat".into()]));
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
        // The merge_status path hands over raw `gh --jq` output — whitespace
        // must not turn a decided answer into `Unknown`.
        assert_eq!(parse_mergeable("CONFLICTING\n"), Mergeable::Conflicting);
        assert_eq!(parse_mergeable(" MERGEABLE "), Mergeable::Clean);
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
    fn comments_uses_gh_api_placeholders_and_paginates() {
        let args = comments_args(7);
        assert_eq!(args[0], "api");
        assert!(args.contains(&"--paginate".to_string()));
        assert_eq!(
            args.last().unwrap(),
            "repos/{owner}/{repo}/pulls/7/comments?per_page=100"
        );
    }

    #[test]
    fn paged_output_is_flattened_across_pages() {
        let items = parse_paged_array("[{\"id\":1},{\"id\":2}]\n[{\"id\":3}]").unwrap();
        let ids: Vec<u64> = items.iter().filter_map(|v| v["id"].as_u64()).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert!(parse_paged_array("[]").unwrap().is_empty());
        assert!(parse_paged_array("[1,").is_err());
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
    fn latest_reviews_parse_carries_body_and_submitted_at() {
        // The shape `gh pr view --json latestReviews` actually returns — the
        // review id comes back empty, which is why the body's identity is
        // author + submittedAt.
        let json: Value = serde_json::from_str(
            r#"{"latestReviews":[
                {"author":{"login":"Argus"},"state":"COMMENTED",
                 "body":"pass · high confidence","submittedAt":"2026-08-21T13:58:00Z","id":""},
                {"author":{"login":"octocat"},"state":"APPROVED","body":""}
            ]}"#,
        )
        .unwrap();
        let reviews = parse_latest_reviews(&json);
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].author, "Argus");
        assert_eq!(reviews[0].state, "COMMENTED");
        assert_eq!(reviews[0].body, "pass · high confidence");
        assert_eq!(reviews[0].submitted_at, "2026-08-21T13:58:00Z");
        assert_eq!(reviews[0].body_key(), "argus@2026-08-21T13:58:00Z");
        // Missing fields degrade to empty, not a parse failure.
        assert_eq!(reviews[1].body, "");
        assert_eq!(reviews[1].submitted_at, "");
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
        let args = review_prs_args(&ReviewScope::Authors(vec![
            "alice".into(),
            "bob".into(),
            "".into(),
        ]));
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
        // Without an explicit limit gh silently truncates a search listing at 30.
        assert!(args.windows(2).any(|w| w == ["--limit", "100"]));
    }

    #[test]
    fn normalize_login_accepts_the_shapes_people_paste() {
        assert_eq!(normalize_login("octocat").as_deref(), Some("octocat"));
        assert_eq!(normalize_login("  @octocat ").as_deref(), Some("octocat"));
        assert_eq!(
            normalize_login("https://github.com/octocat").as_deref(),
            Some("octocat")
        );
        assert_eq!(
            normalize_login("https://github.com/octocat/repo/pull/7").as_deref(),
            Some("octocat")
        );
        assert_eq!(
            normalize_login("github.com/octo-cat").as_deref(),
            Some("octo-cat")
        );
    }

    #[test]
    fn normalize_login_rejects_anything_a_search_would_choke_on() {
        // A space is the dangerous one: it becomes a free-text term ANDed with
        // the rest of the query, so the board silently empties.
        assert_eq!(normalize_login("Nathan FCG"), None);
        assert_eq!(normalize_login(""), None);
        assert_eq!(normalize_login("@"), None);
        assert_eq!(normalize_login("-lead"), None);
        assert_eq!(normalize_login("trail-"), None);
        assert_eq!(normalize_login("do--uble"), None);
        assert_eq!(normalize_login("no_underscores"), None);
        assert_eq!(normalize_login(&"a".repeat(40)), None);
    }

    #[test]
    fn review_prs_search_drops_authors_that_are_not_logins() {
        let args = review_prs_args(&ReviewScope::Authors(vec![
            "Nathan FCG".into(),
            "https://github.com/octocat".into(),
            "@alice".into(),
        ]));
        let search = args
            .iter()
            .skip_while(|a| *a != "--search")
            .nth(1)
            .expect("search term");
        assert!(!search.contains("Nathan"));
        assert!(!search.contains("github.com"));
        assert!(search.contains("author:octocat"));
        assert!(search.contains("author:alice"));
    }

    #[test]
    fn review_prs_everyone_excludes_own_prs_and_names_no_author() {
        let args = review_prs_args(&ReviewScope::Everyone);
        let search = args
            .iter()
            .skip_while(|a| *a != "--search")
            .nth(1)
            .expect("search term");
        assert!(search.contains("-author:@me"));
        assert!(search.contains("-reviewed-by:@me"));
        assert!(search.contains("-is:draft"));
        // No positive `author:` qualifier, which would narrow it back down.
        assert!(!search.contains(" author:"));
        assert!(args.windows(2).any(|w| w == ["--limit", "100"]));
    }

    #[test]
    fn parse_review_prs_drops_bots_only_when_asked() {
        let value = serde_json::json!([
            { "number": 1, "title": "human", "author": { "login": "alice", "is_bot": false } },
            { "number": 2, "title": "bump", "author": { "login": "dependabot", "is_bot": true } },
        ]);
        let kept: Vec<u64> = parse_review_prs(&value, false)
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(kept, vec![1, 2]);
        let filtered: Vec<u64> = parse_review_prs(&value, true)
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(filtered, vec![1]);
    }

    #[test]
    fn pr_authors_args_lists_open_non_draft_prs_by_others() {
        let args = pr_authors_args();
        assert_eq!(&args[0..2], &["pr", "list"]);
        assert!(args.windows(2).any(|w| w == ["--state", "open"]));
        assert!(args.windows(2).any(|w| w == ["--json", "author"]));
        assert!(args.windows(2).any(|w| w == ["--limit", "100"]));
        let search = args
            .iter()
            .skip_while(|a| *a != "--search")
            .nth(1)
            .expect("search term");
        assert!(search.contains("-is:draft"));
        assert!(search.contains("-author:@me"));
        // Someone whose PR you reviewed once is still worth tracking, so the
        // suggestions deliberately don't inherit the scan's reviewed filter.
        assert!(!search.contains("-reviewed-by:@me"));
    }

    #[test]
    fn parse_pr_authors_ranks_by_open_prs_dedupes_and_drops_bots() {
        let value = serde_json::json!([
            { "author": { "login": "alice", "is_bot": false } },
            { "author": { "login": "bob", "is_bot": false } },
            { "author": { "login": "Alice", "is_bot": false } },
            { "author": { "login": "renovate", "is_bot": true } },
            { "author": { "login": "carol", "is_bot": false } },
        ]);
        // alice has two (case-insensitively the same person), then the
        // one-PR authors alphabetically; the bot never shows up.
        assert_eq!(parse_pr_authors(&value), vec!["alice", "bob", "carol"]);
    }

    #[test]
    fn push_target_reads_a_same_repo_pr_as_pushable() {
        let args = pr_push_target_args(7);
        assert!(args.iter().any(|a| a.contains("maintainerCanModify")));
        assert!(args.last().unwrap().split(',').any(|f| f == "baseRefName"));
        let t = parse_push_target(
            r#"{"headRefName":"feat/x","baseRefName":"dev","isCrossRepository":false,
                "maintainerCanModify":false,
                "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"me"}}"#,
        )
        .expect("parsed");
        assert_eq!(t.head_ref, "feat/x");
        assert_eq!(t.base_ref, "dev");
        assert!(t.pushable(), "our own repo is always pushable");
    }

    #[test]
    fn push_target_refuses_a_fork_without_maintainer_edits() {
        let t = parse_push_target(
            r#"{"headRefName":"feat/x","isCrossRepository":true,"maintainerCanModify":false,
                "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"octocat"}}"#,
        )
        .expect("parsed");
        assert_eq!(t.head_repo, "octocat/repo");
        assert!(!t.pushable());
        let allowed = parse_push_target(
            r#"{"headRefName":"feat/x","isCrossRepository":true,"maintainerCanModify":true,
                "headRepository":{"name":"repo"},"headRepositoryOwner":{"login":"octocat"}}"#,
        )
        .expect("parsed");
        assert!(allowed.pushable());
    }

    #[test]
    fn push_target_is_none_for_junk_or_a_headless_payload() {
        assert!(parse_push_target("not json").is_none());
        assert!(parse_push_target(r#"{"headRefName":""}"#).is_none());
        assert!(parse_push_target("{}").is_none());
    }

    #[test]
    fn pr_comment_posts_to_the_issue_endpoint_via_stdin() {
        let args = pr_comment_args(7);
        assert!(args
            .iter()
            .any(|a| a == "repos/{owner}/{repo}/issues/7/comments"));
        assert!(args.windows(2).any(|w| w == ["--input", "-"]));
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
            "api x",
            br#"{"message":"Unprocessable Entity","errors":["line must be part of the diff"]}"#,
            b"gh: Unprocessable Entity (HTTP 422)\n",
        );
        assert!(msg.contains("gh api x failed: gh: Unprocessable Entity (HTTP 422)"));
        assert!(msg.contains("line must be part of the diff"));
    }

    #[test]
    fn gh_failure_message_skips_an_empty_stdout() {
        let msg = gh_failure_message("pr", b"", b"boom\n");
        assert_eq!(msg, "gh pr failed: boom");
    }
}
