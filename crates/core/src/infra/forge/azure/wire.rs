//! The Azure DevOps REST payloads: pure request bodies and response parsers,
//! unit-tested against the documented shapes (api-version 7.1). Nothing here
//! does I/O — [`super::AzureForge`] fetches, these translate.
//!
//! Translation conventions (see the [`crate::infra::forge`] module docs):
//! - handles are lowercased `uniqueName`s (an email for Entra/MSA users);
//! - review verdicts map onto the GitHub strings: votes 10/5 → `APPROVED`,
//!   -5/-10 → `CHANGES_REQUESTED`, 0 → no review;
//! - a comment id packs `(thread id, comment id)` — see [`pack_comment_id`];
//! - paths drop Azure's leading `/`.

use serde_json::{json, Value};

use crate::domain::model::{
    CheckStatus, Mergeable, PrInfo, PrState, ReviewComment, ReviewEvent, ReviewSummary,
    ReviewThread,
};
use crate::infra::forge::remote::AzureRepo;
use crate::infra::forge::{FailedCheck, LivePrState, OpenPr, PrPushTarget, PrSummary};

// --- ids ----------------------------------------------------------------------

/// Bits reserved for the per-thread comment id. Azure caps a thread at 500
/// comments, so 12 bits (4095) is ample headroom.
const COMMENT_BITS: u32 = 12;
const COMMENT_MASK: u64 = (1 << COMMENT_BITS) - 1;

/// Pack an Azure comment's `(thread id, comment id)` into the single opaque
/// id the rest of Usine carries ([`ReviewComment::id`]). Azure comment ids are
/// only unique within a thread, and every reply/resolve is addressed to the
/// thread — so the pair is what identifies a comment, and packing makes it
/// decodable without a lookup. Thread ids are 32-bit, so the result stays
/// below 2^44: safely under 2^53 (the ids round-trip through the triage
/// agent's JSON as floats) and under the executor's synthetic review-body ids.
pub fn pack_comment_id(thread_id: u64, comment_id: u64) -> u64 {
    (thread_id << COMMENT_BITS) | (comment_id & COMMENT_MASK)
}

/// The `(thread id, comment id)` pair a [`pack_comment_id`] id came from.
pub fn unpack_comment_id(id: u64) -> (u64, u64) {
    (id >> COMMENT_BITS, id & COMMENT_MASK)
}

// --- refs, identities, URLs ---------------------------------------------------

/// `refs/heads/x` for a bare branch name (idempotent).
pub fn head_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_string()
    } else {
        format!("refs/heads/{branch}")
    }
}

/// A branch name without Azure's `refs/heads/` prefix.
pub fn short_ref(r: &str) -> String {
    r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
}

/// The handle Usine records for an `IdentityRef`: its `uniqueName` (an email
/// for users) lowercased, else its display name. Lowercased so the comparisons
/// against configured reviewers and contributors can't split on case.
pub fn identity_handle(identity: &Value) -> String {
    identity
        .get("uniqueName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| identity.get("displayName").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_ascii_lowercase()
}

fn identity_id(identity: &Value) -> &str {
    identity.get("id").and_then(Value::as_str).unwrap_or("")
}

/// Whether an identity is a group or a service rather than a person: a
/// container (a team, a security group — policies add those as required
/// reviewers), or a service principal (the build service, an extension),
/// whose descriptors start `svc.`/`s2s.`. Azure has no `is_bot` flag; this is
/// what stands in for it.
pub fn is_non_human(identity: &Value) -> bool {
    if identity.get("isContainer").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let descriptor = identity
        .get("descriptor")
        .and_then(Value::as_str)
        .unwrap_or("");
    if descriptor.starts_with("svc.") || descriptor.starts_with("s2s.") {
        return true;
    }
    let name = identity
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or("");
    name.ends_with("Build Service") || name.contains("Build Service (")
}

/// The pull request's web page from a PR payload, falling back to the repo
/// coordinates we already hold. The payload's own `url` is an API address.
pub fn pr_web_url(repo: &AzureRepo, pr: &Value) -> String {
    let id = pr_id(pr);
    match pr.pointer("/repository/webUrl").and_then(Value::as_str) {
        Some(web) if !web.is_empty() => format!("{}/pullrequest/{id}", web.trim_end_matches('/')),
        _ => repo.pr_web_url(id),
    }
}

/// A build's results page — the "details" link of a failing build check, and
/// what [`build_id_from_url`] reads back for its log.
pub fn build_results_url(repo: &AzureRepo, build_id: u64) -> String {
    format!("{}/_build/results?buildId={build_id}", repo.project_url())
}

/// The build id in a build results URL (`…?buildId=123`), if it is one.
pub fn build_id_from_url(url: &str) -> Option<u64> {
    let (_, rest) = url.split_once("buildId=")?;
    rest.split(['&', '#']).next()?.parse().ok()
}

// --- pull requests -------------------------------------------------------------

pub fn pr_id(pr: &Value) -> u64 {
    pr.get("pullRequestId").and_then(Value::as_u64).unwrap_or(0)
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Azure caps a PR description at 4000 characters and rejects a longer one
/// outright — after the branch was already pushed. Truncate on a char boundary
/// with a visible marker instead.
pub const MAX_DESCRIPTION_CHARS: usize = 4000;
/// Title cap: Azure rejects titles over 400 characters.
pub const MAX_TITLE_CHARS: usize = 400;

pub fn truncate_chars(s: &str, max: usize) -> String {
    const MARKER: &str = "\n\n…(truncated)";
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(MARKER.chars().count());
    let mut out: String = s.chars().take(keep).collect();
    out.push_str(MARKER);
    out
}

pub fn create_pr_body(
    title: &str,
    body: &str,
    base: &str,
    head: &str,
    reviewer_id: Option<&str>,
    draft: bool,
) -> Value {
    let reviewers: Vec<Value> = reviewer_id
        .map(|id| vec![json!({ "id": id })])
        .unwrap_or_default();
    json!({
        "sourceRefName": head_ref(head),
        "targetRefName": head_ref(base),
        "title": truncate_chars(title, MAX_TITLE_CHARS),
        "description": truncate_chars(body, MAX_DESCRIPTION_CHARS),
        "isDraft": draft,
        "reviewers": reviewers,
    })
}

/// The body that completes (merges) a PR: squash, and never let Azure delete
/// the source branch — the executor deletes it itself, after the card's
/// worktree is gone (see `Executor::merge`). `lastMergeSourceCommit` guards
/// against completing a head newer than the one we checked.
pub fn complete_pr_body(last_merge_source_commit: &str) -> Value {
    json!({
        "status": "completed",
        "lastMergeSourceCommit": { "commitId": last_merge_source_commit },
        "completionOptions": {
            "mergeStrategy": "squash",
            "deleteSourceBranch": false,
            "transitionWorkItems": false,
        },
    })
}

pub fn pr_state(pr: &Value) -> Option<LivePrState> {
    match pr.get("status").and_then(Value::as_str)? {
        "active" => Some(LivePrState::Open {
            draft: pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        }),
        "completed" => Some(LivePrState::Merged),
        "abandoned" => Some(LivePrState::Closed),
        _ => None,
    }
}

/// Map `mergeStatus` onto [`Mergeable`]. Only a definite `conflicts` is a
/// conflict: `rejectedByPolicy` means a branch policy refused the merge, which
/// the conflict-resolving agent can do nothing about, and `queued`/`notSet`
/// are Azure still computing — like GitHub's `UNKNOWN`.
pub fn mergeable(pr: &Value) -> Mergeable {
    match pr.get("mergeStatus").and_then(Value::as_str).unwrap_or("") {
        "succeeded" => Mergeable::Clean,
        "conflicts" => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    }
}

/// The humans reviewing a PR, with votes: the first one is the "reviewer" a
/// recovered PR records; groups and services never count.
fn human_reviewers(pr: &Value) -> impl Iterator<Item = &Value> {
    pr.get("reviewers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|r| !is_non_human(r))
}

pub fn pr_info(repo: &AzureRepo, pr: &Value) -> Option<PrInfo> {
    let number = pr_id(pr);
    if number == 0 {
        return None;
    }
    let state = match pr_state(pr)? {
        LivePrState::Open { draft } => PrState::open(draft),
        LivePrState::Merged => PrState::Merged,
        LivePrState::Closed => PrState::Closed,
    };
    Some(PrInfo {
        number,
        url: pr_web_url(repo, pr),
        title: text(pr, "title"),
        state,
        reviewer: human_reviewers(pr).next().map(identity_handle),
        reviewer_recorded: false,
    })
}

/// The submitted verdicts on a PR, one per human reviewer who voted. Azure
/// votes carry no text and no timestamp, so `body`/`submitted_at` stay empty:
/// the feedback lives in threads, which triage reads like inline comments.
pub fn reviews(pr: &Value) -> Vec<ReviewSummary> {
    human_reviewers(pr)
        .filter_map(|r| {
            let state = match r.get("vote").and_then(Value::as_i64).unwrap_or(0) {
                v if v > 0 => "APPROVED",
                v if v < 0 => "CHANGES_REQUESTED",
                _ => return None,
            };
            Some(ReviewSummary::new(identity_handle(r), state))
        })
        .collect()
}

/// The vote a submitted review casts, or `None` for a plain comment review
/// (Azure has no "commented" vote — the threads are the review). "Request
/// changes" is `-5`, *waiting for author*: the Azure idiom for "address these
/// and I'll look again", where `-10` (*rejected*) reads as a veto.
pub fn vote_for(event: ReviewEvent) -> Option<i64> {
    match event {
        ReviewEvent::Approve => Some(10),
        ReviewEvent::RequestChanges => Some(-5),
        ReviewEvent::Comment => None,
    }
}

/// My current vote on a PR (0 when I'm not a reviewer).
pub fn my_vote(pr: &Value, me_id: &str) -> i64 {
    pr.get("reviewers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .find(|r| identity_id(r).eq_ignore_ascii_case(me_id))
        .and_then(|r| r.get("vote").and_then(Value::as_i64))
        .unwrap_or(0)
}

pub fn push_target(pr: &Value) -> Option<PrPushTarget> {
    let head = short_ref(pr.get("sourceRefName").and_then(Value::as_str)?);
    if head.is_empty() {
        return None;
    }
    let fork = pr.pointer("/forkSource/repository");
    Some(PrPushTarget {
        head_ref: head,
        base_ref: short_ref(&text(pr, "targetRefName")),
        cross_repo: fork.is_some(),
        head_repo: fork
            .and_then(|r| r.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // Azure has no "allow edits by maintainers": a fork's branch is the
        // author's alone, so a fork head is never pushable from here.
        maintainer_can_modify: false,
    })
}

pub fn pr_summary(repo: &AzureRepo, pr: &Value, checks: CheckStatus) -> PrSummary {
    PrSummary {
        number: pr_id(pr),
        title: text(pr, "title"),
        author: pr
            .get("createdBy")
            .map(identity_handle)
            .unwrap_or_else(|| "unknown".into()),
        head_ref: short_ref(&text(pr, "sourceRefName")),
        base_ref: short_ref(&text(pr, "targetRefName")),
        url: pr_web_url(repo, pr),
        body: text(pr, "description"),
        checks,
        mergeable: mergeable(pr),
    }
}

/// An active PR as the adopt dialog lists it. `mine` compares identity ids,
/// so an unknown `me_id` (empty) flags nothing.
pub fn open_pr(repo: &AzureRepo, pr: &Value, me_id: &str) -> OpenPr {
    let creator = pr.get("createdBy");
    OpenPr {
        number: pr_id(pr),
        title: text(pr, "title"),
        author: creator
            .map(identity_handle)
            .unwrap_or_else(|| "unknown".into()),
        head_ref: short_ref(&text(pr, "sourceRefName")),
        base_ref: short_ref(&text(pr, "targetRefName")),
        url: pr_web_url(repo, pr),
        body: text(pr, "description"),
        draft: pr.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        cross_repo: pr.pointer("/forkSource/repository").is_some(),
        mine: !me_id.is_empty()
            && creator.is_some_and(|c| identity_id(c).eq_ignore_ascii_case(me_id)),
    }
}

pub fn pr_list(value: &Value) -> Vec<Value> {
    value
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Whether the review board should consider this open PR: not a draft, not
/// mine, and (in "everyone" mode) not a bot's. `authors` is the pinned
/// handles, already normalized; `None` means everyone.
pub fn review_candidate(pr: &Value, me_id: &str, authors: Option<&[String]>) -> bool {
    if pr.get("isDraft").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let Some(creator) = pr.get("createdBy") else {
        return false;
    };
    if identity_id(creator).eq_ignore_ascii_case(me_id) {
        return false;
    }
    match authors {
        Some(authors) => authors.contains(&identity_handle(creator)),
        None => !is_non_human(creator),
    }
}

/// Distinct human authors of the given open PRs, busiest first (ties by
/// handle) — the contributor picker's suggestions. Mirrors the GitHub
/// ordering in `parse_pr_authors`.
pub fn pr_authors(prs: &[Value], me_id: &str) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for pr in prs {
        if pr.get("isDraft").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(creator) = pr.get("createdBy") else {
            continue;
        };
        if is_non_human(creator) || identity_id(creator).eq_ignore_ascii_case(me_id) {
            continue;
        }
        let handle = identity_handle(creator);
        match counts.iter_mut().find(|(h, _)| *h == handle) {
            Some((_, n)) => *n += 1,
            None => counts.push((handle, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    counts.into_iter().map(|(h, _)| h).collect()
}

/// The humans of a team-members listing (`{value: [{identity: {…}}]}`), minus
/// me — the reviewer picker (Azure refuses nothing here, but asking yourself
/// to review your own PR is noise).
pub fn team_members(value: &Value, me_id: &str) -> Vec<String> {
    let mut out: Vec<String> = pr_list(value)
        .iter()
        .filter_map(|m| m.get("identity"))
        .filter(|i| !is_non_human(i) && !identity_id(i).eq_ignore_ascii_case(me_id))
        .map(identity_handle)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The first identity id in an identities lookup (`{value: [{id}]}`).
pub fn first_identity_id(value: &Value) -> Option<String> {
    pr_list(value)
        .iter()
        .filter_map(|i| i.get("id").and_then(Value::as_str))
        .find(|id| !id.is_empty())
        .map(str::to_string)
}

// --- threads -------------------------------------------------------------------

/// A thread status that means "handled": what GitHub calls resolved.
fn resolved_status(status: &str) -> bool {
    matches!(status, "fixed" | "wontFix" | "closed" | "byDesign")
}

/// A thread a person wrote, as opposed to Azure's own activity log. Votes,
/// pushes, reviewer changes and merge attempts all arrive as threads in the
/// same list: they carry a `CodeReviewThreadType` property, no `status`, and
/// `system` comments. Left in, they would pin the comment count above zero
/// forever — and that count must reach zero for an approval to clear the
/// merge gate.
fn is_user_thread(t: &Value) -> bool {
    if t.get("isDeleted").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    let status = t.get("status").and_then(Value::as_str).unwrap_or("");
    if status.is_empty() || status == "unknown" {
        return false;
    }
    if t.pointer("/properties/CodeReviewThreadType").is_some() {
        return false;
    }
    true
}

/// A thread's comments a person wrote and didn't delete, oldest first.
fn live_comments(t: &Value) -> Vec<&Value> {
    let mut comments: Vec<&Value> = t
        .get("comments")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|c| {
            c.get("isDeleted").and_then(Value::as_bool) != Some(true)
                && c.get("commentType")
                    .and_then(Value::as_str)
                    .unwrap_or("text")
                    != "system"
        })
        .collect();
    comments.sort_by_key(|c| c.get("id").and_then(Value::as_u64).unwrap_or(0));
    comments
}

fn user_threads(value: &Value) -> Vec<Value> {
    pr_list(value)
        .into_iter()
        .filter(is_user_thread)
        .filter(|t| !live_comments(t).is_empty())
        .collect()
}

/// Where a thread is anchored: its file (without Azure's leading `/`) and the
/// line on the new side, falling back to the old side for a comment on a
/// deleted line. A thread on no file is a PR-conversation comment: empty path.
fn thread_location(t: &Value) -> (String, Option<u64>) {
    let Some(ctx) = t.get("threadContext").filter(|c| !c.is_null()) else {
        return (String::new(), None);
    };
    let path = ctx
        .get("filePath")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start_matches('/')
        .to_string();
    let line = ctx
        .pointer("/rightFileStart/line")
        .and_then(Value::as_u64)
        .or_else(|| ctx.pointer("/leftFileStart/line").and_then(Value::as_u64))
        .filter(|l| *l > 0);
    (path, line)
}

/// Every comment of every user thread — the flat list the triage items and
/// the comment counts are built from.
pub fn thread_comments(value: &Value) -> Vec<ReviewComment> {
    let mut out = Vec::new();
    for t in user_threads(value) {
        let thread_id = t.get("id").and_then(Value::as_u64).unwrap_or(0);
        let (path, line) = thread_location(&t);
        for c in live_comments(&t) {
            let comment_id = c.get("id").and_then(Value::as_u64).unwrap_or(0);
            out.push(ReviewComment {
                id: pack_comment_id(thread_id, comment_id),
                author: c
                    .get("author")
                    .map(identity_handle)
                    .unwrap_or_else(|| "unknown".into()),
                path: path.clone(),
                line,
                body: text(c, "content"),
                review_body_of: None,
            });
        }
    }
    out
}

/// The user threads with their resolution and who spoke last (`me_id` is the
/// authenticated identity's id).
pub fn threads(value: &Value, me_id: &str) -> Vec<ReviewThread> {
    user_threads(value)
        .iter()
        .map(|t| {
            let thread_id = t.get("id").and_then(Value::as_u64).unwrap_or(0);
            let comments = live_comments(t);
            ReviewThread {
                id: thread_id.to_string(),
                resolved: resolved_status(t.get("status").and_then(Value::as_str).unwrap_or("")),
                comment_ids: comments
                    .iter()
                    .map(|c| {
                        pack_comment_id(thread_id, c.get("id").and_then(Value::as_u64).unwrap_or(0))
                    })
                    .collect(),
                last_by_viewer: comments
                    .last()
                    .and_then(|c| c.get("author"))
                    .is_some_and(|a| {
                        !me_id.is_empty() && identity_id(a).eq_ignore_ascii_case(me_id)
                    }),
            }
        })
        .collect()
}

/// Whether the thread with `thread_id` is still open (not handled).
pub fn thread_is_open(value: &Value, thread_id: u64) -> bool {
    pr_list(value).iter().any(|t| {
        t.get("id").and_then(Value::as_u64) == Some(thread_id)
            && !resolved_status(t.get("status").and_then(Value::as_str).unwrap_or(""))
    })
}

/// Whether `me_id` wrote any comment in any user thread — together with a
/// vote, what "I already reviewed this" means on Azure (a Comment review
/// casts no vote).
pub fn i_commented(value: &Value, me_id: &str) -> bool {
    !me_id.is_empty()
        && user_threads(value).iter().any(|t| {
            live_comments(t).iter().any(|c| {
                c.get("author")
                    .is_some_and(|a| identity_id(a).eq_ignore_ascii_case(me_id))
            })
        })
}

/// Thread status codes as the create/update endpoints take them.
pub const THREAD_ACTIVE: i64 = 1;
pub const THREAD_FIXED: i64 = 2;
pub const THREAD_WONT_FIX: i64 = 3;
pub const THREAD_CLOSED: i64 = 4;

/// A new thread: one comment, optionally anchored to a line on the new side.
pub fn new_thread_body(content: &str, anchor: Option<(&str, u64)>, status: i64) -> Value {
    let mut thread = json!({
        "comments": [{ "parentCommentId": 0, "content": content, "commentType": 1 }],
        "status": status,
    });
    if let Some((path, line)) = anchor {
        let path = format!("/{}", path.trim_start_matches('/'));
        thread["threadContext"] = json!({
            "filePath": path,
            "rightFileStart": { "line": line, "offset": 1 },
            "rightFileEnd": { "line": line, "offset": 1 },
        });
    }
    thread
}

pub fn reply_body(content: &str, parent_comment_id: u64) -> Value {
    json!({
        "content": content,
        "parentCommentId": parent_comment_id.max(1),
        "commentType": 1,
    })
}

/// Whether `me_id` already posted this exact comment at this spot — how a
/// re-sent review skips the threads that landed before a failure (a review is
/// several requests on Azure, not one atomic POST).
pub fn already_posted(
    value: &Value,
    me_id: &str,
    anchor: Option<(&str, u64)>,
    content: &str,
) -> bool {
    user_threads(value).iter().any(|t| {
        let (path, line) = thread_location(t);
        let here = match anchor {
            Some((p, l)) => path == p.trim_start_matches('/') && line == Some(l),
            None => path.is_empty(),
        };
        here && live_comments(t).first().is_some_and(|c| {
            c.get("author")
                .is_some_and(|a| identity_id(a).eq_ignore_ascii_case(me_id))
                && text(c, "content").trim() == content.trim()
        })
    })
}

// --- CI ------------------------------------------------------------------------

/// Policy type ids (stable across organizations) of the two policies that are
/// CI: build validation, and external status checks.
const BUILD_POLICY: &str = "0609b952-1397-4640-95ec-e00a01b2c241";
const STATUS_POLICY: &str = "cbdc66da-9728-4af8-aada-9a5a32e4a226";

/// One CI signal on a PR, before rollup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub status: CheckStatus,
    pub check: FailedCheck,
}

/// The CI signals among a PR's policy evaluations: build validation and status
/// policies only — a missing approver or an unlinked work item is a policy
/// too, but not CI, and must never be offered to the agent as a failing check.
///
/// A build whose result is stale (expired, or for an older iteration) reads
/// as pending: right after a push Azure can still show the previous build's
/// green, which must not turn the merge gate green. A manual-queue build with
/// no current run is no CI at all rather than pending forever.
pub fn policy_signals(repo: &AzureRepo, value: &Value) -> Vec<Signal> {
    pr_list(value)
        .iter()
        .filter(|e| {
            let cfg = e.get("configuration");
            let enabled = cfg
                .and_then(|c| c.get("isEnabled"))
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let type_id = cfg
                .and_then(|c| c.pointer("/type/id"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let type_name = cfg
                .and_then(|c| c.pointer("/type/displayName"))
                .and_then(Value::as_str)
                .unwrap_or("");
            enabled
                && (type_id.eq_ignore_ascii_case(BUILD_POLICY)
                    || type_id.eq_ignore_ascii_case(STATUS_POLICY)
                    || type_name.eq_ignore_ascii_case("Build")
                    || type_name.eq_ignore_ascii_case("Status"))
        })
        .filter_map(|e| {
            let ctx = e.get("context").cloned().unwrap_or(Value::Null);
            let settings = e
                .pointer("/configuration/settings")
                .cloned()
                .unwrap_or(Value::Null);
            let build_id = ctx.get("buildId").and_then(Value::as_u64);
            let stale = ctx.get("isExpired").and_then(Value::as_bool) == Some(true)
                || ctx.get("buildIsNotCurrent").and_then(Value::as_bool) == Some(true);
            let manual = settings.get("manualQueueOnly").and_then(Value::as_bool) == Some(true);
            let raw = e.get("status").and_then(Value::as_str).unwrap_or("");
            let status = match raw {
                "notApplicable" => return None,
                "queued" | "running" if manual && build_id.is_none() => return None,
                _ if stale && manual => return None,
                _ if stale => CheckStatus::Pending,
                "queued" | "running" => CheckStatus::Pending,
                "approved" => CheckStatus::Passing,
                "rejected" | "broken" => CheckStatus::Failing,
                _ => return None,
            };
            let name = settings
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| ctx.get("buildDefinitionName").and_then(Value::as_str))
                .or_else(|| settings.get("statusName").and_then(Value::as_str))
                .or_else(|| {
                    e.pointer("/configuration/type/displayName")
                        .and_then(Value::as_str)
                })
                .unwrap_or("build")
                .to_string();
            Some(Signal {
                status,
                check: FailedCheck {
                    name,
                    workflow: String::new(),
                    url: build_id
                        .map(|id| build_results_url(repo, id))
                        .unwrap_or_default(),
                },
            })
        })
        .collect()
}

/// The CI signals among a PR's statuses (posted by external services): the
/// latest status per context — a service re-posting `pending` then
/// `succeeded` leaves both in the list.
pub fn status_signals(value: &Value) -> Vec<Signal> {
    let list = pr_list(value);
    let mut latest: Vec<(String, u64, &Value)> = Vec::new();
    for s in &list {
        let genre = s
            .pointer("/context/genre")
            .and_then(Value::as_str)
            .unwrap_or("");
        let name = s
            .pointer("/context/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let key = if genre.is_empty() {
            name.to_string()
        } else {
            format!("{genre}/{name}")
        };
        let id = s.get("id").and_then(Value::as_u64).unwrap_or(0);
        match latest.iter_mut().find(|(k, _, _)| *k == key) {
            Some(entry) if entry.1 <= id => *entry = (key, id, s),
            Some(_) => {}
            None => latest.push((key, id, s)),
        }
    }
    latest
        .into_iter()
        .filter_map(|(key, _, s)| {
            let status = match s.get("state").and_then(Value::as_str).unwrap_or("") {
                "pending" => CheckStatus::Pending,
                "succeeded" => CheckStatus::Passing,
                "failed" | "error" => CheckStatus::Failing,
                _ => return None,
            };
            Some(Signal {
                status,
                check: FailedCheck {
                    name: key,
                    workflow: String::new(),
                    url: text(s, "targetUrl"),
                },
            })
        })
        .collect()
}

/// Roll signals up like GitHub's check rollup: failure outranks pending,
/// pending outranks success, and no signal at all is [`CheckStatus::None`].
pub fn rollup(signals: &[Signal]) -> (CheckStatus, Vec<FailedCheck>) {
    let failed: Vec<FailedCheck> = signals
        .iter()
        .filter(|s| s.status == CheckStatus::Failing)
        .map(|s| s.check.clone())
        .collect();
    let status = if !failed.is_empty() {
        CheckStatus::Failing
    } else if signals.iter().any(|s| s.status == CheckStatus::Pending) {
        CheckStatus::Pending
    } else if signals.is_empty() {
        CheckStatus::None
    } else {
        CheckStatus::Passing
    };
    (status, failed)
}

/// The log ids of a build timeline's failed steps (`records` with a failed
/// `result` and a `log`), task records first — they hold the actual error,
/// where a failed job's log is mostly setup noise.
pub fn failed_log_ids(timeline: &Value) -> Vec<(String, u64)> {
    let records = timeline
        .get("records")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut tasks = Vec::new();
    let mut others = Vec::new();
    for r in records {
        if r.get("result").and_then(Value::as_str) != Some("failed") {
            continue;
        }
        let Some(log_id) = r.pointer("/log/id").and_then(Value::as_u64) else {
            continue;
        };
        let name = text(r, "name");
        if r.get("type").and_then(Value::as_str) == Some("Task") {
            tasks.push((name, log_id));
        } else {
            others.push((name, log_id));
        }
    }
    if tasks.is_empty() {
        others
    } else {
        tasks
    }
}

/// The authenticated identity from `_apis/connectionData`: its id and handle.
pub fn connection_identity(value: &Value) -> Option<(String, String)> {
    let user = value.get("authenticatedUser")?;
    let id = user.get("id").and_then(Value::as_str)?.to_string();
    let handle = user
        .pointer("/properties/Account/$value")
        .and_then(Value::as_str)
        .or_else(|| user.get("providerDisplayName").and_then(Value::as_str))
        .unwrap_or("")
        .to_ascii_lowercase();
    Some((id, handle))
}

/// The human-readable reason in an Azure DevOps error payload.
pub fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| body.trim().chars().take(300).collect())
}

/// The policies blocking a PR's completion, by name, from its evaluations:
/// every enabled blocking policy that hasn't approved.
pub fn blocking_policies(value: &Value) -> Vec<String> {
    let mut names: Vec<String> = pr_list(value)
        .iter()
        .filter(|e| {
            let cfg = e.get("configuration");
            cfg.and_then(|c| c.get("isEnabled"))
                .and_then(Value::as_bool)
                != Some(false)
                && cfg
                    .and_then(|c| c.get("isBlocking"))
                    .and_then(Value::as_bool)
                    != Some(false)
                && !matches!(
                    e.get("status").and_then(Value::as_str).unwrap_or(""),
                    "approved" | "notApplicable"
                )
        })
        .map(|e| {
            e.pointer("/configuration/settings/displayName")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    e.pointer("/configuration/type/displayName")
                        .and_then(Value::as_str)
                })
                .unwrap_or("a branch policy")
                .to_string()
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> AzureRepo {
        AzureRepo {
            org: "fabrikam".into(),
            project: "Fiber".into(),
            repo: "app".into(),
        }
    }

    const ME: &str = "11111111-1111-1111-1111-111111111111";

    fn identity(id: &str, email: &str) -> Value {
        json!({ "id": id, "displayName": email, "uniqueName": email })
    }

    #[test]
    fn comment_ids_round_trip_below_2_pow_53() {
        for (t, c) in [(1, 1), (148, 3), (u32::MAX as u64, 500)] {
            let id = pack_comment_id(t, c);
            assert_eq!(unpack_comment_id(id), (t, c));
            assert!(id < (1u64 << 53));
            assert!(id < 9_000_000_000_000_000);
        }
        // Distinct threads never collide on comment 1.
        assert_ne!(pack_comment_id(1, 1), pack_comment_id(2, 1));
    }

    #[test]
    fn refs_gain_and_lose_the_heads_prefix() {
        assert_eq!(head_ref("feat/x"), "refs/heads/feat/x");
        assert_eq!(head_ref("refs/heads/feat/x"), "refs/heads/feat/x");
        assert_eq!(short_ref("refs/heads/feat/x"), "feat/x");
        assert_eq!(short_ref("feat/x"), "feat/x");
    }

    #[test]
    fn long_descriptions_are_truncated_with_a_marker() {
        let long = "é".repeat(5000);
        let t = truncate_chars(&long, MAX_DESCRIPTION_CHARS);
        assert_eq!(t.chars().count(), MAX_DESCRIPTION_CHARS);
        assert!(t.ends_with("(truncated)"));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn create_body_targets_heads_refs_and_names_the_reviewer() {
        let b = create_pr_body("T", "B", "main", "usine/x", Some("guid-1"), true);
        assert_eq!(b["sourceRefName"], "refs/heads/usine/x");
        assert_eq!(b["targetRefName"], "refs/heads/main");
        assert_eq!(b["isDraft"], true);
        assert_eq!(b["reviewers"][0]["id"], "guid-1");
        let b = create_pr_body("T", "B", "main", "usine/x", None, false);
        assert!(b["reviewers"].as_array().unwrap().is_empty());
    }

    #[test]
    fn completing_squashes_and_keeps_the_branch() {
        let b = complete_pr_body("abc");
        assert_eq!(b["status"], "completed");
        assert_eq!(b["lastMergeSourceCommit"]["commitId"], "abc");
        assert_eq!(b["completionOptions"]["mergeStrategy"], "squash");
        assert_eq!(b["completionOptions"]["deleteSourceBranch"], false);
    }

    #[test]
    fn pr_status_maps_onto_live_state_and_pr_info() {
        let pr = json!({
            "pullRequestId": 22, "status": "active", "isDraft": true, "title": "T",
            "repository": { "webUrl": "https://dev.azure.com/fabrikam/Fiber/_git/app" },
            "reviewers": [
                { "id": "g", "displayName": "[Fiber]\\Team", "uniqueName": "vstfs:///x", "isContainer": true, "vote": 0 },
                { "id": "r", "displayName": "Rita", "uniqueName": "Rita@Fabrikam.com", "vote": 0 },
            ],
        });
        assert_eq!(pr_state(&pr), Some(LivePrState::Open { draft: true }));
        let info = pr_info(&repo(), &pr).unwrap();
        assert_eq!(info.number, 22);
        assert_eq!(info.state, PrState::Draft);
        assert_eq!(
            info.url,
            "https://dev.azure.com/fabrikam/Fiber/_git/app/pullrequest/22"
        );
        // Groups are skipped; handles are lowercased.
        assert_eq!(info.reviewer.as_deref(), Some("rita@fabrikam.com"));
        assert_eq!(
            pr_state(&json!({"status": "completed"})),
            Some(LivePrState::Merged)
        );
        assert_eq!(
            pr_state(&json!({"status": "abandoned"})),
            Some(LivePrState::Closed)
        );
        assert_eq!(pr_state(&json!({"status": "weird"})), None);
    }

    #[test]
    fn only_a_definite_conflict_is_conflicting() {
        let m = |s: &str| mergeable(&json!({ "mergeStatus": s }));
        assert_eq!(m("succeeded"), Mergeable::Clean);
        assert_eq!(m("conflicts"), Mergeable::Conflicting);
        for s in ["queued", "notSet", "rejectedByPolicy", "failure", ""] {
            assert_eq!(m(s), Mergeable::Unknown, "{s}");
        }
    }

    #[test]
    fn votes_become_github_verdicts_and_zero_is_no_review() {
        let pr = json!({ "reviewers": [
            { "id": "a", "uniqueName": "a@x.com", "vote": 10 },
            { "id": "b", "uniqueName": "b@x.com", "vote": 5 },
            { "id": "c", "uniqueName": "c@x.com", "vote": -5 },
            { "id": "d", "uniqueName": "d@x.com", "vote": -10 },
            { "id": "e", "uniqueName": "e@x.com", "vote": 0 },
            { "id": "g", "uniqueName": "[p]\\team", "vote": 10, "isContainer": true },
        ]});
        let r = reviews(&pr);
        let states: Vec<(&str, &str)> = r
            .iter()
            .map(|s| (s.author.as_str(), s.state.as_str()))
            .collect();
        assert_eq!(
            states,
            vec![
                ("a@x.com", "APPROVED"),
                ("b@x.com", "APPROVED"),
                ("c@x.com", "CHANGES_REQUESTED"),
                ("d@x.com", "CHANGES_REQUESTED"),
            ]
        );
        assert!(r
            .iter()
            .all(|s| s.body.is_empty() && !s.has_body_feedback()));
        assert_eq!(my_vote(&pr, "C"), -5);
        assert_eq!(my_vote(&pr, "nobody"), 0);
        assert_eq!(vote_for(ReviewEvent::Approve), Some(10));
        assert_eq!(vote_for(ReviewEvent::RequestChanges), Some(-5));
        assert_eq!(vote_for(ReviewEvent::Comment), None);
    }

    fn threads_fixture() -> Value {
        json!({ "value": [
            // A reviewer's inline comment, answered by me.
            { "id": 141, "status": "active",
              "threadContext": { "filePath": "/src/lib.rs", "rightFileStart": { "line": 12, "offset": 1 } },
              "comments": [
                { "id": 1, "parentCommentId": 0, "author": identity("r", "rita@x.com"), "content": "Extract this.", "commentType": "text" },
                { "id": 2, "parentCommentId": 1, "author": identity(ME, "me@x.com"), "content": "Done.", "commentType": "text" },
              ] },
            // A PR-level (file-less) comment, resolved.
            { "id": 142, "status": "fixed",
              "comments": [ { "id": 1, "author": identity("r", "rita@x.com"), "content": "Add a changelog entry.", "commentType": "text" } ] },
            // A comment on a deleted line (old side only).
            { "id": 143, "status": "active",
              "threadContext": { "filePath": "/src/old.rs", "leftFileStart": { "line": 4, "offset": 1 } },
              "comments": [
                { "id": 1, "author": identity("r", "rita@x.com"), "content": "Why remove this?", "commentType": "text" },
                { "id": 2, "author": identity("r", "rita@x.com"), "content": "deleted", "commentType": "text", "isDeleted": true },
              ] },
            // System threads: a vote and a push.
            { "id": 144, "properties": { "CodeReviewThreadType": { "$type": "System.String", "$value": "VoteUpdate" } },
              "comments": [ { "id": 1, "author": identity("svc", "svc"), "content": "Rita voted 10", "commentType": "system" } ] },
            { "id": 145, "status": "active", "properties": { "CodeReviewThreadType": { "$value": "RefUpdate" } },
              "comments": [ { "id": 1, "content": "pushed", "commentType": "system" } ] },
            // A deleted thread.
            { "id": 146, "status": "active", "isDeleted": true,
              "comments": [ { "id": 1, "author": identity("r", "rita@x.com"), "content": "gone", "commentType": "text" } ] },
        ]})
    }

    #[test]
    fn system_and_deleted_threads_never_become_comments() {
        let comments = thread_comments(&threads_fixture());
        let bodies: Vec<&str> = comments.iter().map(|c| c.body.as_str()).collect();
        assert_eq!(
            bodies,
            vec![
                "Extract this.",
                "Done.",
                "Add a changelog entry.",
                "Why remove this?"
            ]
        );
        let first = &comments[0];
        assert_eq!(first.id, pack_comment_id(141, 1));
        assert_eq!(first.author, "rita@x.com");
        assert_eq!(first.path, "src/lib.rs");
        assert_eq!(first.line, Some(12));
        // PR conversation: no file, no line.
        assert_eq!(comments[2].path, "");
        assert_eq!(comments[2].line, None);
        // Deleted line: the old side's line.
        assert_eq!(comments[3].path, "src/old.rs");
        assert_eq!(comments[3].line, Some(4));
    }

    #[test]
    fn threads_carry_resolution_and_who_spoke_last() {
        let t = threads(&threads_fixture(), ME);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].id, "141");
        assert!(!t[0].resolved);
        assert!(t[0].last_by_viewer, "I replied last");
        assert!(!t[0].is_unanswered());
        assert_eq!(
            t[0].comment_ids,
            vec![pack_comment_id(141, 1), pack_comment_id(141, 2)]
        );
        assert!(t[1].resolved, "fixed counts as resolved");
        assert!(
            t[2].is_unanswered(),
            "the reviewer spoke last on an active thread"
        );
        for status in ["fixed", "wontFix", "closed", "byDesign"] {
            assert!(resolved_status(status), "{status}");
        }
        assert!(!resolved_status("active") && !resolved_status("pending"));
        assert!(thread_is_open(&threads_fixture(), 141));
        assert!(!thread_is_open(&threads_fixture(), 142));
    }

    #[test]
    fn my_comments_count_as_having_reviewed() {
        assert!(i_commented(&threads_fixture(), ME));
        assert!(!i_commented(&threads_fixture(), "someone-else"));
        assert!(!i_commented(&threads_fixture(), ""));
    }

    #[test]
    fn new_threads_anchor_on_the_new_side_with_a_leading_slash() {
        let t = new_thread_body("Fix this", Some(("src/a.rs", 7)), THREAD_ACTIVE);
        assert_eq!(t["threadContext"]["filePath"], "/src/a.rs");
        assert_eq!(t["threadContext"]["rightFileStart"]["line"], 7);
        assert_eq!(t["comments"][0]["content"], "Fix this");
        assert_eq!(t["status"], THREAD_ACTIVE);
        let general = new_thread_body("Pushed abc", None, THREAD_CLOSED);
        assert!(general.get("threadContext").is_none());
        assert_eq!(general["status"], THREAD_CLOSED);
        assert_eq!(reply_body("ok", 1)["parentCommentId"], 1);
        assert_eq!(reply_body("ok", 0)["parentCommentId"], 1);
    }

    #[test]
    fn a_resent_review_recognizes_what_already_landed() {
        let v = json!({ "value": [
            { "id": 7, "status": "active",
              "threadContext": { "filePath": "/src/a.rs", "rightFileStart": { "line": 3 } },
              "comments": [ { "id": 1, "author": identity(ME, "me@x.com"), "content": "Fix this", "commentType": "text" } ] },
        ]});
        assert!(already_posted(&v, ME, Some(("src/a.rs", 3)), "Fix this"));
        assert!(!already_posted(&v, ME, Some(("src/a.rs", 4)), "Fix this"));
        assert!(!already_posted(&v, ME, Some(("src/a.rs", 3)), "Other"));
        assert!(!already_posted(
            &v,
            "someone",
            Some(("src/a.rs", 3)),
            "Fix this"
        ));
    }

    fn evaluation(type_id: &str, name: &str, status: &str, ctx: Value, settings: Value) -> Value {
        json!({
            "status": status,
            "configuration": { "isEnabled": true, "isBlocking": true,
                "type": { "id": type_id, "displayName": name }, "settings": settings },
            "context": ctx,
        })
    }

    #[test]
    fn only_build_and_status_policies_are_ci() {
        let v = json!({ "value": [
            evaluation(BUILD_POLICY, "Build", "rejected", json!({ "buildId": 9 }), json!({ "displayName": "PR build" })),
            evaluation("fa4e907d-c16b-4a4c-9dfa-4906e5d171dd", "Minimum number of reviewers", "rejected", json!({}), json!({})),
            evaluation("40e92b44-2fe1-4dd6-b3d8-74a9c21d0c6e", "Work item linking", "queued", json!({}), json!({})),
        ]});
        let signals = policy_signals(&repo(), &v);
        assert_eq!(
            signals.len(),
            1,
            "reviewer and work-item policies aren't CI"
        );
        let (status, failed) = rollup(&signals);
        assert_eq!(status, CheckStatus::Failing);
        assert_eq!(failed[0].name, "PR build");
        assert_eq!(
            failed[0].url,
            "https://dev.azure.com/fabrikam/Fiber/_build/results?buildId=9"
        );
        assert_eq!(build_id_from_url(&failed[0].url), Some(9));
    }

    #[test]
    fn a_stale_green_build_reads_pending_and_manual_builds_are_no_ci() {
        let stale = json!({ "value": [
            evaluation(BUILD_POLICY, "Build", "approved", json!({ "buildId": 9, "isExpired": true }), json!({})),
        ]});
        assert_eq!(
            rollup(&policy_signals(&repo(), &stale)).0,
            CheckStatus::Pending
        );
        let manual = json!({ "value": [
            evaluation(BUILD_POLICY, "Build", "queued", json!({}), json!({ "manualQueueOnly": true })),
        ]});
        assert_eq!(
            rollup(&policy_signals(&repo(), &manual)).0,
            CheckStatus::None
        );
        let running = json!({ "value": [
            evaluation(BUILD_POLICY, "Build", "running", json!({ "buildId": 10 }), json!({})),
            evaluation(BUILD_POLICY, "Build", "approved", json!({ "buildId": 11 }), json!({})),
        ]});
        assert_eq!(
            rollup(&policy_signals(&repo(), &running)).0,
            CheckStatus::Pending
        );
        assert_eq!(rollup(&[]).0, CheckStatus::None);
    }

    #[test]
    fn statuses_keep_the_latest_per_context() {
        let v = json!({ "value": [
            { "id": 1, "state": "pending", "context": { "genre": "ci", "name": "lint" } },
            { "id": 2, "state": "succeeded", "context": { "genre": "ci", "name": "lint" } },
            { "id": 3, "state": "failed", "context": { "genre": "ci", "name": "test" }, "targetUrl": "https://ci/3" },
            { "id": 4, "state": "notApplicable", "context": { "name": "other" } },
        ]});
        let signals = status_signals(&v);
        assert_eq!(signals.len(), 2);
        let (status, failed) = rollup(&signals);
        assert_eq!(status, CheckStatus::Failing);
        assert_eq!(failed[0].name, "ci/test");
        assert_eq!(failed[0].url, "https://ci/3");
    }

    #[test]
    fn failed_task_logs_come_before_job_logs() {
        let t = json!({ "records": [
            { "name": "Job", "type": "Job", "result": "failed", "log": { "id": 1 } },
            { "name": "Run tests", "type": "Task", "result": "failed", "log": { "id": 7 } },
            { "name": "Checkout", "type": "Task", "result": "succeeded", "log": { "id": 2 } },
        ]});
        assert_eq!(failed_log_ids(&t), vec![("Run tests".to_string(), 7)]);
        let jobs_only = json!({ "records": [ { "name": "Job", "type": "Job", "result": "failed", "log": { "id": 1 } } ] });
        assert_eq!(failed_log_ids(&jobs_only), vec![("Job".to_string(), 1)]);
    }

    #[test]
    fn review_candidates_skip_drafts_mine_and_bots() {
        let pr = |creator: Value, draft: bool| json!({ "isDraft": draft, "createdBy": creator });
        let rita = identity("r", "Rita@x.com");
        assert!(review_candidate(&pr(rita.clone(), false), ME, None));
        assert!(!review_candidate(&pr(rita.clone(), true), ME, None));
        assert!(!review_candidate(
            &pr(identity(ME, "me@x.com"), false),
            ME,
            None
        ));
        let bot = json!({ "id": "b", "displayName": "Fiber Build Service (fabrikam)", "uniqueName": "b" });
        assert!(!review_candidate(&pr(bot, false), ME, None));
        let pinned = vec!["rita@x.com".to_string()];
        assert!(review_candidate(&pr(rita, false), ME, Some(&pinned)));
        assert!(!review_candidate(
            &pr(identity("s", "sam@x.com"), false),
            ME,
            Some(&pinned)
        ));
    }

    #[test]
    fn authors_rank_by_open_prs() {
        let prs = vec![
            json!({ "createdBy": identity("s", "sam@x.com") }),
            json!({ "createdBy": identity("r", "rita@x.com") }),
            json!({ "createdBy": identity("r", "Rita@x.com") }),
            json!({ "createdBy": identity(ME, "me@x.com") }),
            json!({ "isDraft": true, "createdBy": identity("d", "dan@x.com") }),
        ];
        assert_eq!(pr_authors(&prs, ME), vec!["rita@x.com", "sam@x.com"]);
    }

    #[test]
    fn push_targets_refuse_forks() {
        let same = json!({ "sourceRefName": "refs/heads/feat/x",
                           "targetRefName": "refs/heads/dev" });
        let t = push_target(&same).unwrap();
        assert_eq!(t.head_ref, "feat/x");
        assert_eq!(t.base_ref, "dev");
        assert!(t.pushable());
        let fork = json!({ "sourceRefName": "refs/heads/feat/x", "forkSource": { "repository": { "name": "fork" } } });
        let t = push_target(&fork).unwrap();
        assert!(t.cross_repo && !t.pushable());
        assert!(push_target(&json!({})).is_none());
    }

    #[test]
    fn open_prs_read_forks_drafts_and_mine() {
        let repo = repo();
        let pr = json!({ "pullRequestId": 7, "title": "Mine", "isDraft": true,
            "sourceRefName": "refs/heads/feat/a", "targetRefName": "refs/heads/main",
            "description": "why", "createdBy": identity(ME, "Me@x.com") });
        let p = open_pr(&repo, &pr, ME);
        assert_eq!(
            (p.number, p.head_ref.as_str(), p.base_ref.as_str()),
            (7, "feat/a", "main")
        );
        assert!(p.draft && p.mine && !p.cross_repo);
        assert_eq!(p.author, "me@x.com");
        assert_eq!(p.body, "why");
        assert!(
            !open_pr(&repo, &pr, "").mine,
            "an unknown viewer flags nothing"
        );
        let fork = json!({ "pullRequestId": 8, "sourceRefName": "refs/heads/patch",
            "forkSource": { "repository": { "name": "fork" } },
            "createdBy": identity("o", "o@x.com") });
        let p = open_pr(&repo, &fork, ME);
        assert!(p.cross_repo && !p.mine && !p.draft);
    }

    #[test]
    fn connection_data_names_me() {
        let v = json!({ "authenticatedUser": { "id": ME, "providerDisplayName": "Me",
            "properties": { "Account": { "$type": "System.String", "$value": "Me@X.com" } } } });
        assert_eq!(
            connection_identity(&v),
            Some((ME.to_string(), "me@x.com".to_string()))
        );
        assert_eq!(connection_identity(&json!({})), None);
    }

    #[test]
    fn blocking_policies_are_named() {
        let v = json!({ "value": [
            evaluation(BUILD_POLICY, "Build", "approved", json!({}), json!({})),
            evaluation("x", "Minimum number of reviewers", "rejected", json!({}), json!({})),
            evaluation("y", "Work item linking", "queued", json!({}), json!({})),
        ]});
        assert_eq!(
            blocking_policies(&v),
            vec!["Minimum number of reviewers", "Work item linking"]
        );
    }

    #[test]
    fn error_messages_prefer_the_payload_message() {
        assert_eq!(
            error_message(
                r#"{"message":"TF401027: You need the Git 'PullRequestContribute' permission"}"#
            ),
            "TF401027: You need the Git 'PullRequestContribute' permission"
        );
        assert_eq!(error_message("plain"), "plain");
    }
}
