//! The "no work needed" escape hatch for plan and implement runs.
//!
//! Some cards need no code change: the ask is already implemented on the base
//! branch, it's obsolete, or it doesn't make sense for this codebase. Rather
//! than inventing work (or ending in a generic "no files changed" failure), the
//! agent says so and ends its reply with a fenced ` ```usine-no-work ` block:
//!
//! ```text
//! {"v":1,"kind":"already_done"|"not_actionable","explanation":"…"}
//! ```
//!
//! The executor parks the card in
//! [`CardState::NothingToDo`](crate::domain::model::CardState::NothingToDo)
//! with that explanation, for the user to mark done or overrule.

use serde::Deserialize;

use crate::agent::block;
use crate::domain::model::NoWorkKind;

/// The fence tag the agent emits.
pub const NO_WORK_TAG: &str = "usine-no-work";

const SHARED: &str = "\
If, after investigating, you are confident this task needs NO code change — it is already \
implemented on the base branch, it is obsolete, or the request does not make sense for this \
codebase — do not invent work and do not edit any files. Explain why in your reply (cite the \
files and lines that already do it, or why the request cannot apply), and end your reply with a \
fenced code block tagged `usine-no-work` containing a JSON object shaped like \
{\"v\": 1, \"kind\": \"already_done\" | \"not_actionable\", \"explanation\": \"<1-3 sentences>\"}. \
Use it only when you are sure; if the request is merely ambiguous, ask questions instead.";

/// Appended to plan runs.
pub fn plan_instruction() -> String {
    format!("{SHARED} Emit it instead of a plan.")
}

/// Appended to (non-change) implement runs.
pub fn implement_instruction() -> String {
    format!("{SHARED} Emit it INSTEAD of the commit-message and hand-off blocks.")
}

/// The agent's verdict that the card needs no code change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoWork {
    pub kind: NoWorkKind,
    pub explanation: String,
}

#[derive(Deserialize)]
struct Payload {
    kind: NoWorkKind,
    #[serde(default)]
    explanation: String,
}

/// The agent's no-work verdict, if its reply *ends* with a `usine-no-work`
/// block.
///
/// Only a trailing block counts — the instruction tells the agent to end its
/// reply with it, and a plan that merely quotes or illustrates the protocol
/// (this repo's own plans discuss it) must not turn into a verdict. "Trailing"
/// tolerates other `usine-*` blocks after it (a stray commit block, say), but
/// no prose. The last `usine-no-work` opener is the one considered.
///
/// A block whose JSON doesn't parse still counts — the agent clearly meant to
/// say "nothing to do" — and degrades to `NotActionable` with the reply's prose
/// as the explanation. Likewise an empty `explanation` falls back to the prose.
pub fn parse_no_work(text: &str) -> Option<NoWork> {
    let start = last_opener(text)?;
    let tail = &text[start..];
    let found = block::candidates(tail, NO_WORK_TAG);
    // The block as parsed: the first close whose payload is valid JSON, else
    // the nearest close (malformed).
    let chosen = found
        .iter()
        .find(|f| serde_json::from_str::<Payload>(f.payload).is_ok())
        .or(found.first())?;
    if !only_usine_blocks(&tail[chosen.end..]) {
        return None;
    }
    let (kind, explanation) = match serde_json::from_str::<Payload>(chosen.payload) {
        Ok(p) => (p.kind, p.explanation.trim().to_string()),
        Err(_) => (NoWorkKind::NotActionable, String::new()),
    };
    let explanation = if explanation.is_empty() {
        let prose = block::strip(&text[..start], NO_WORK_TAG);
        crate::domain::session::strip_status_lines(&prose)
            .trim()
            .to_string()
    } else {
        explanation
    };
    Some(NoWork { kind, explanation })
}

/// Byte offset of the last line opening a `usine-no-work` fence.
fn last_opener(text: &str) -> Option<usize> {
    let mut offset = 0;
    let mut last = None;
    for line in text.split_inclusive('\n') {
        if opener_tag(line).is_some_and(|tag| tag == NO_WORK_TAG) {
            last = Some(offset);
        }
        offset += line.len();
    }
    last
}

/// The info string of a fence-opening line (≤3 spaces indent, ≥3 backticks or
/// tildes), trimmed.
fn opener_tag(line: &str) -> Option<&str> {
    let bare = line.trim_end_matches(['\n', '\r']);
    let indent = bare.len() - bare.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &bare[indent..];
    let fence = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let run = rest.chars().take_while(|c| *c == fence).count();
    (run >= 3).then(|| rest[run..].trim())
}

/// `rest` holds nothing but whitespace and complete `usine-*` blocks.
fn only_usine_blocks(mut rest: &str) -> bool {
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return true;
        }
        let first_line = rest.split_inclusive('\n').next().unwrap_or(rest);
        let Some(tag) = opener_tag(first_line).filter(|t| t.starts_with("usine-")) else {
            return false;
        };
        let Some(found) = block::candidates(rest, tag).into_iter().next() else {
            return false;
        };
        rest = &rest[found.end..];
    }
}

/// The reply with the `usine-no-work` block removed, for display.
pub fn strip_no_work_block(text: &str) -> String {
    block::strip(text, NO_WORK_TAG)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_verdict() {
        let text = "It's already there in foo.rs:12.\n\n```usine-no-work\n\
                    {\"v\":1,\"kind\":\"already_done\",\"explanation\":\"foo.rs:12 does it.\"}\n```";
        assert_eq!(
            parse_no_work(text),
            Some(NoWork {
                kind: NoWorkKind::AlreadyDone,
                explanation: "foo.rs:12 does it.".into(),
            })
        );
    }

    #[test]
    fn not_actionable_and_empty_explanation_fall_back_to_prose() {
        let text = "This repo has no web UI.\n\n```usine-no-work\n\
                    {\"kind\":\"not_actionable\",\"explanation\":\"\"}\n```";
        assert_eq!(
            parse_no_work(text),
            Some(NoWork {
                kind: NoWorkKind::NotActionable,
                explanation: "This repo has no web UI.".into(),
            })
        );
    }

    #[test]
    fn malformed_block_still_counts() {
        let text = "Nothing to change here.\n\n```usine-no-work\n{kind: already}\n```";
        assert_eq!(
            parse_no_work(text),
            Some(NoWork {
                kind: NoWorkKind::NotActionable,
                explanation: "Nothing to change here.".into(),
            })
        );
    }

    #[test]
    fn a_quoted_block_mid_reply_is_not_a_verdict() {
        // A plan that illustrates the protocol, then carries on with prose.
        let text = "## Plan\n\nThe agent emits:\n\n```usine-no-work\n\
                    {\"v\":1,\"kind\":\"already_done\"|\"not_actionable\"}\n```\n\n\
                    Then the executor parks the card. Step 2: add tests.";
        assert_eq!(parse_no_work(text), None);
        // Same with a well-formed example.
        let text = "Example:\n```usine-no-work\n{\"kind\":\"already_done\"}\n```\nMore plan.";
        assert_eq!(parse_no_work(text), None);
    }

    #[test]
    fn the_last_block_wins_and_trailing_usine_blocks_are_tolerated() {
        let text = "Example:\n```usine-no-work\n{bad}\n```\nIt's in foo.rs.\n\n\
                    ```usine-no-work\n{\"kind\":\"already_done\",\"explanation\":\"\"}\n```\n\n\
                    ```usine-commit\nchore: nothing\n```\n";
        let verdict = parse_no_work(text).unwrap();
        assert_eq!(verdict.kind, NoWorkKind::AlreadyDone);
        assert_eq!(verdict.explanation, "Example:\n\nIt's in foo.rs.");
    }

    #[test]
    fn absent_block_is_none() {
        assert_eq!(parse_no_work("Implemented the thing."), None);
    }

    #[test]
    fn strips_the_block() {
        let text = "Prose.\n\n```usine-no-work\n{\"kind\":\"already_done\"}\n```";
        assert_eq!(strip_no_work_block(text), "Prose.");
        assert_eq!(strip_no_work_block("just prose"), "just prose");
    }

    #[test]
    fn instructions_name_the_tag() {
        assert!(plan_instruction().contains(NO_WORK_TAG));
        assert!(implement_instruction().contains("INSTEAD"));
    }
}
