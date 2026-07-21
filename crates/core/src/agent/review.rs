//! Structured self-review and PR-comment triage.
//!
//! Both flows run a headless agent that emits its verdicts as a fenced
//! ` ```usine-review ` JSON block, then parse that block into [`FixVerdict`]s.
//! This mirrors the `usine-questions` pattern in [`crate::agent::plan`]:
//! - **Self-review** ([`parse_self_review`]) — the agent invents findings from
//!   the committed diff; each becomes a verdict over a *synthetic* comment.
//! - **PR triage** ([`parse_triage`]) — the agent judges *existing* PR comments
//!   (matched by id), adding a worth-fixing verdict and a short reply.

use std::path::Path;

use serde::Deserialize;

use crate::agent::block;
use crate::domain::model::{DraftComment, FixVerdict, ReviewComment, ReviewEvent};

const TAG: &str = "usine-review";

/// Appended to a self-review run: review the committed diff and report issues.
pub const SELF_REVIEW_INSTRUCTION: &str = "\
You are reviewing the committed changes on this branch (compare against the base branch, e.g. \
`git diff` the base). This runs headlessly with no follow-up turn, and you must NOT modify any \
files — this is review only. Report the issues you find as a fenced code block tagged \
`usine-review` containing a JSON array, each item shaped like \
{\"path\": \"src/foo.rs\", \"line\": 42, \"issue\": \"<what's wrong>\", \"severity\": \"high\", \
\"worth_fixing\": true, \"opinion\": \"<why / how important>\"}. `line` is a line number in the \
file's NEW version (its state on this branch); omit it for a whole-file issue. `severity` is your honest \
assessment of how serious the issue is — one of \"critical\", \"high\", \"medium\", or \"low\" — \
independent of whether it's worth fixing now. Use worth_fixing=true for genuine problems and false \
for optional nits. Keep `issue` and `opinion` to one or two sentences each. If you find nothing \
worth raising, emit an empty array `[]`.";

/// Appended to a PR-comment triage run: judge each numbered comment.
pub const REVIEW_TRIAGE_INSTRUCTION: &str = "\
You are triaging pull-request review comments. Each comment above is listed with a numeric id. \
Consider each against the actual diff on this branch. This runs headlessly and you must NOT modify \
any files. Respond with a fenced code block tagged `usine-review` containing a JSON array, one item \
per comment, shaped like {\"id\": 123, \"severity\": \"high\", \"worth_fixing\": true, \
\"opinion\": \"<your reasoning>\", \"reply\": \"<short reply>\"}. `severity` is your honest \
assessment of how serious the comment's underlying issue is — one of \"critical\", \"high\", \
\"medium\", or \"low\" — independent of whether it's worth fixing. `opinion` is shown to the user \
as your recommendation; `reply` is the message posted on the comment when the user chooses NOT to \
fix it — a polite, one- or two-sentence explanation. Include exactly one item per comment id listed.";

/// Built-in review guidance used when the project has no `review.md`.
pub const DEFAULT_REVIEW_PROMPT: &str = "\
Review these changes for correctness bugs, missing edge cases, security issues, and clear \
simplifications or reuse opportunities. Prefer a few high-confidence findings over many style nits.";

/// Appended to a PR-review run (reviewing *another* contributor's PR): review the
/// branch's diff vs its base and draft inline comments to post as a GitHub review.
pub const PR_REVIEW_INSTRUCTION: &str = "\
You are reviewing a pull request authored by someone else. The PR's changes are the commits on this \
branch beyond its base — run `git diff <base>...HEAD` (the base branch is stated above) to see exactly \
what changed, and review ONLY those changes (you may read the surrounding code for context). This runs \
headlessly with no follow-up turn, and you must NOT modify any files — this is review only. \
Write your review as a fenced code block tagged `usine-review` containing a JSON object shaped like \
{\"comments\": [{\"path\": \"src/foo.rs\", \"line\": 42, \"body\": \"<short, specific comment>\", \
\"severity\": \"high\"}], \"summary\": \"<1-3 sentence overall summary>\", \"event\": \"comment\"}. \
Each comment's `line` is a line number in the file's NEW version that the changes touch; omit `line` \
only for a whole-file comment. Keep each `body` to one or two sentences — short and actionable. \
`severity` is one of \"critical\", \"high\", \"medium\", or \"low\". `event` is your overall verdict: \
\"comment\" (neutral feedback), \"request_changes\" (problems that should block), or \"approve\" (looks \
good). If you have no inline comments, use an empty `comments` array.";

/// Prefaces the background block below, framing it as context rather than spec.
const BACKGROUND_PREAMBLE: &str = "\
Background — how this task evolved after the description above was written. Treat this as CONTEXT, \
not as a specification to check the code against. Where the implementation diverges from the \
original description but follows the decisions below, that divergence is deliberate: do NOT report \
it as a deviation, a missing requirement, or an issue. Review the code on its own merits.";

/// The context a card accumulated *after* its description was written: the plan
/// the user approved, and the answers and change requests from its runs.
///
/// Read-only runs (self-review, PR-comment triage) start a fresh conversation
/// whose only statement of intent is `card.description`. Without this, a mid-flight
/// adjustment the user asked for ("use a channel, not a mutex") is indistinguishable
/// from the agent quietly ignoring the spec, and gets reported as a finding.
/// Write runs don't need it — they're handed the plan and the feedback directly.
///
/// Returns an empty string when there's nothing to say, so callers can skip the
/// block entirely.
pub fn background_prompt(plan: Option<&str>, qa_log: &[String]) -> String {
    let plan = plan.map(str::trim).filter(|p| !p.is_empty());
    let entries: Vec<&str> = qa_log
        .iter()
        .map(|e| e.trim())
        .filter(|e| !e.is_empty())
        .collect();
    if plan.is_none() && entries.is_empty() {
        return String::new();
    }

    let mut s = String::from(BACKGROUND_PREAMBLE);
    if let Some(plan) = plan {
        s.push_str("\n\nThe plan the user approved:\n");
        s.push_str(plan);
        s.push('\n');
    }
    if !entries.is_empty() {
        s.push_str("\n\nDecisions and change requests the user made during the work:\n");
        for e in entries {
            // Indent continuation lines so a multi-line entry stays one bullet.
            s.push_str("- ");
            s.push_str(&e.replace('\n', "\n  "));
            s.push('\n');
        }
    }
    s
}

/// The project's review guidance: the contents of a `review.md` / `REVIEW.md` at
/// the repo root if present, else [`DEFAULT_REVIEW_PROMPT`].
pub fn find_review_prompt(repo: &Path) -> String {
    for name in ["review.md", "REVIEW.md", "Review.md"] {
        if let Ok(content) = std::fs::read_to_string(repo.join(name)) {
            if !content.trim().is_empty() {
                return content;
            }
        }
    }
    DEFAULT_REVIEW_PROMPT.to_string()
}

/// One raw verdict item as emitted by the agent. Fields are optional so a partial
/// or slightly-off object still parses (missing bits fall back to sensible
/// defaults rather than dropping the whole array).
#[derive(Debug, Default, Deserialize)]
struct RawVerdict {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    issue: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    worth_fixing: bool,
    #[serde(default)]
    opinion: String,
    #[serde(default)]
    reply: String,
}

/// Clamp the agent's severity string to one of the known levels (lower-cased),
/// or empty if it returned something unexpected.
fn normalize_severity(s: &str) -> String {
    match s.trim().to_lowercase().as_str() {
        "critical" => "critical".into(),
        "high" => "high".into(),
        "medium" => "medium".into(),
        "low" => "low".into(),
        _ => String::new(),
    }
}

/// The JSON payload inside the fenced `usine-review` block, if present.
fn find_block(text: &str) -> Option<&str> {
    block::find(text, TAG)
}

/// Extract and parse the `usine-review` JSON array. Tolerant of a missing or
/// malformed block (returns an empty vec), like [`crate::agent::plan::parse_plan`].
fn parse_raw(text: &str) -> Vec<RawVerdict> {
    find_block(text)
        .and_then(|b| serde_json::from_str(b).ok())
        .unwrap_or_default()
}

/// Self-review: turn the agent's invented findings into verdicts over synthetic
/// comments (there is no GitHub comment yet). Findings default to *selected* when
/// worth fixing so the common case is one click.
pub fn parse_self_review(text: &str) -> Vec<FixVerdict> {
    parse_raw(text)
        .into_iter()
        .enumerate()
        .map(|(i, r)| FixVerdict {
            comment: ReviewComment {
                id: r.id.unwrap_or(i as u64),
                author: "self-review".into(),
                path: r.path,
                line: r.line,
                body: r.issue,
            },
            selected: r.worth_fixing,
            worth_fixing: r.worth_fixing,
            severity: normalize_severity(&r.severity),
            rationale: r.opinion,
            reply: String::new(),
        })
        .collect()
}

/// PR triage: join each verdict to its comment by id. A comment the agent didn't
/// return a verdict for defaults to worth-fixing/selected so it's never silently
/// dropped from the user's picker.
pub fn parse_triage(text: &str, comments: &[ReviewComment]) -> Vec<FixVerdict> {
    let raw = parse_raw(text);
    comments
        .iter()
        .map(|c| match raw.iter().find(|r| r.id == Some(c.id)) {
            Some(r) => FixVerdict {
                comment: c.clone(),
                selected: r.worth_fixing,
                worth_fixing: r.worth_fixing,
                severity: normalize_severity(&r.severity),
                rationale: r.opinion.clone(),
                reply: r.reply.clone(),
            },
            None => FixVerdict {
                comment: c.clone(),
                selected: true,
                worth_fixing: true,
                severity: String::new(),
                rationale: "No verdict returned — review this one manually.".into(),
                reply: String::new(),
            },
        })
        .collect()
}

// --- PR review (reviewing another contributor's PR) ------------------------

/// One drafted comment as emitted by the PR-review agent.
#[derive(Debug, Default, Deserialize)]
struct RawDraft {
    #[serde(default)]
    path: String,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    severity: String,
}

/// The structured object a PR-review run emits: inline comments + an overall
/// summary and verdict.
#[derive(Debug, Default, Deserialize)]
struct RawPrReview {
    #[serde(default)]
    comments: Vec<RawDraft>,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    event: String,
}

fn parse_event(s: &str) -> ReviewEvent {
    match s.trim().to_lowercase().as_str() {
        "approve" | "approved" => ReviewEvent::Approve,
        "request_changes" | "request changes" | "changes_requested" => ReviewEvent::RequestChanges,
        _ => ReviewEvent::Comment,
    }
}

/// Parse a PR-review run's output into drafted comments (all pre-selected), an
/// overall summary, and a verdict. Tolerant of a missing/malformed block and of
/// the agent emitting a bare comments array instead of the full object.
pub fn parse_pr_review(text: &str) -> (Vec<DraftComment>, String, ReviewEvent) {
    let Some(block) = find_block(text) else {
        return (Vec::new(), String::new(), ReviewEvent::Comment);
    };
    let raw: RawPrReview = serde_json::from_str::<RawPrReview>(block).unwrap_or_else(|_| {
        // Fall back to a bare array of comments.
        RawPrReview {
            comments: serde_json::from_str(block).unwrap_or_default(),
            summary: String::new(),
            event: String::new(),
        }
    });
    let drafts = raw
        .comments
        .into_iter()
        .filter(|d| !d.path.is_empty() && !d.body.is_empty())
        .map(|d| DraftComment {
            path: d.path,
            line: d.line,
            body: d.body,
            severity: normalize_severity(&d.severity),
            selected: true,
        })
        .collect();
    (drafts, raw.summary, parse_event(&raw.event))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_is_empty_without_a_plan_or_a_qa_log() {
        assert_eq!(background_prompt(None, &[]), "");
        // Blank-ish inputs contribute nothing either.
        assert_eq!(
            background_prompt(Some("  \n "), &["".into(), "  ".into()]),
            ""
        );
    }

    #[test]
    fn background_carries_the_plan_and_the_decisions_as_context() {
        let qa = vec![
            "Q: Mutex or channel?\nA: Channel".to_string(),
            "Requested change: rename `x` to `count`".to_string(),
        ];
        let s = background_prompt(Some("Step 1: do the thing"), &qa);

        // Framed as context, and explicit that divergence from the description
        // is not a finding — this is the whole point of the block.
        assert!(s.contains("CONTEXT"));
        assert!(s.contains("do NOT report"));

        assert!(s.contains("Step 1: do the thing"));
        assert!(s.contains("- Q: Mutex or channel?"));
        assert!(s.contains("- Requested change: rename `x` to `count`"));
        // A multi-line entry stays a single indented bullet.
        assert!(s.contains("\n  A: Channel"));
    }

    #[test]
    fn background_omits_the_section_it_has_no_content_for() {
        let plan_only = background_prompt(Some("the plan"), &[]);
        assert!(plan_only.contains("the plan"));
        assert!(!plan_only.contains("Decisions and change requests"));

        let qa_only = background_prompt(None, &["Requested change: x".into()]);
        assert!(!qa_only.contains("The plan the user approved"));
        assert!(qa_only.contains("Requested change: x"));
    }

    #[test]
    fn self_review_parses_findings() {
        let text = "Here's my review.\n\n```usine-review\n[\
            {\"path\":\"a.rs\",\"line\":3,\"issue\":\"bug\",\"severity\":\"HIGH\",\"worth_fixing\":true,\"opinion\":\"real\"},\
            {\"path\":\"b.rs\",\"issue\":\"nit\",\"severity\":\"bogus\",\"worth_fixing\":false,\"opinion\":\"minor\"}\
            ]\n```";
        let v = parse_self_review(text);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].comment.path, "a.rs");
        assert_eq!(v[0].comment.line, Some(3));
        assert!(v[0].worth_fixing && v[0].selected);
        assert_eq!(v[0].severity, "high", "severity is normalized/lower-cased");
        assert!(!v[1].worth_fixing && !v[1].selected);
        assert_eq!(v[1].comment.line, None);
        assert_eq!(v[1].severity, "", "unknown severity falls back to empty");
    }

    #[test]
    fn triage_joins_by_id_and_backfills_missing() {
        let comments = vec![
            ReviewComment {
                id: 1,
                author: "r".into(),
                path: "a.rs".into(),
                line: Some(1),
                body: "c1".into(),
            },
            ReviewComment {
                id: 2,
                author: "r".into(),
                path: "b.rs".into(),
                line: None,
                body: "c2".into(),
            },
        ];
        // Only a verdict for id 1; id 2 must be backfilled as worth-fixing.
        let text = "```usine-review\n[{\"id\":1,\"worth_fixing\":false,\"opinion\":\"nit\",\"reply\":\"leaving as-is\"}]\n```";
        let v = parse_triage(text, &comments);
        assert_eq!(v.len(), 2);
        assert!(!v[0].worth_fixing);
        assert_eq!(v[0].reply, "leaving as-is");
        assert!(
            v[1].worth_fixing,
            "missing verdict backfills to worth-fixing"
        );
    }

    #[test]
    fn missing_block_yields_empty() {
        assert!(parse_self_review("no block here").is_empty());
    }

    #[test]
    fn pr_review_parses_object_with_comments_and_verdict() {
        let text = "My review:\n\n```usine-review\n{\
            \"comments\":[\
              {\"path\":\"a.rs\",\"line\":3,\"body\":\"off by one\",\"severity\":\"HIGH\"},\
              {\"path\":\"b.rs\",\"body\":\"whole-file note\",\"severity\":\"low\"}\
            ],\"summary\":\"mostly good\",\"event\":\"request_changes\"}\n```";
        let (drafts, summary, event) = parse_pr_review(text);
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].path, "a.rs");
        assert_eq!(drafts[0].line, Some(3));
        assert_eq!(drafts[0].severity, "high");
        assert!(drafts[0].selected, "drafts start selected");
        assert_eq!(drafts[1].line, None);
        assert_eq!(summary, "mostly good");
        assert_eq!(event, ReviewEvent::RequestChanges);
    }

    #[test]
    fn pr_review_falls_back_to_bare_array_and_drops_incomplete() {
        // No object wrapper, and one item missing a body → dropped.
        let text = "```usine-review\n[\
            {\"path\":\"a.rs\",\"line\":1,\"body\":\"ok\"},\
            {\"path\":\"b.rs\"}\
            ]\n```";
        let (drafts, summary, event) = parse_pr_review(text);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].path, "a.rs");
        assert_eq!(summary, "");
        assert_eq!(event, ReviewEvent::Comment, "defaults to Comment");
    }

    #[test]
    fn pr_review_missing_block_is_empty() {
        let (drafts, _, event) = parse_pr_review("no block");
        assert!(drafts.is_empty());
        assert_eq!(event, ReviewEvent::Comment);
    }
}
