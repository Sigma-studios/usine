//! The implementation hand-off: what the agent tells the human who reviews it.
//!
//! An implement run ends with the work committed in the card's worktree and the
//! card parked at the awaiting-review gate. The user arrives there with no idea
//! how the run went — whether the agent hit something surprising, made a call
//! that could have gone the other way, or left part of the task undone.
//!
//! So the run is asked to append a fenced ` ```usine-handoff ` JSON block: a
//! short recap, any open questions, and the things worth testing by hand.
//! [`parse_handoff`] pulls it out; the executor stores it on the card and the
//! detail panel renders it beside the self-review and PR buttons. Same
//! prompt-and-parse shape as [`crate::agent::commit`] and [`crate::agent::review`],
//! and just as tolerant: a run that emits no block simply has no hand-off.

use serde::{Deserialize, Serialize};

use crate::agent::block;

const TAG: &str = "usine-handoff";

/// Appended to implement runs (not to fix runs, which report through their own
/// recap). Kept provider-agnostic and self-contained.
pub const HANDOFF_INSTRUCTION: &str = "\
When you have finished making changes, hand the work off to the human who will review it. Emit a \
fenced code block tagged `usine-handoff` containing a JSON object shaped like \
{\"summary\": \"<how it went>\", \"questions\": [\"<open question>\"], \"tests\": [\"<what to check>\"]}.\n\
- `summary`: two to five sentences for someone who did not watch you work — what you built, the \
approach you took, and anything you had to work around, decide, or deliberately leave out. If part \
of the task is unfinished, or you are unsure a change is right, say so plainly.\n\
- `questions`: what you genuinely want the author to weigh in on — a judgement call that could \
reasonably have gone the other way, an ambiguity in the task, an assumption you had to invent. Use \
an empty array if you have none; do NOT manufacture questions to fill it.\n\
- `tests`: what is worth checking by hand in the running app, most important first, one short line \
each naming the scenario and what should happen. Favour what your automated tests do not already \
cover: the risky paths, the edge cases you touched, the flows a regression would hide in. Prefix a \
scenario you already verified yourself in the running app with `[verified]` — it still deserves a \
human eye, but the reviewer should know it has been exercised once. Use an \
empty array if the change has no observable behaviour.\n\
Be honest and specific — this is the note a careful engineer leaves a colleague, not a sales pitch. \
Do not restate the commit message.";

/// An implement run's hand-off to its reviewer. Every field is optional: an agent
/// with nothing to ask emits `questions: []`, and a change with no observable
/// behaviour emits `tests: []`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// A few sentences on how the implementation went.
    #[serde(default)]
    pub summary: String,
    /// Decisions or ambiguities the agent wants the user to weigh in on.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Things worth testing by hand, most important first.
    #[serde(default)]
    pub tests: Vec<String>,
}

impl Handoff {
    /// Nothing to show. The executor stores this as "no hand-off" rather than an
    /// empty card section.
    pub fn is_empty(&self) -> bool {
        self.summary.trim().is_empty() && self.questions.is_empty() && self.tests.is_empty()
    }
}

/// Extract the agent's hand-off from its `usine-handoff` block. `None` when the
/// block is missing, malformed, or carries nothing — in every case the card ends
/// up with no hand-off rather than a broken one.
pub fn parse_handoff(text: &str) -> Option<Handoff> {
    let raw: Handoff = serde_json::from_str(block::find(text, TAG)?).ok()?;
    let handoff = Handoff {
        summary: raw.summary.trim().to_string(),
        questions: trimmed(raw.questions),
        tests: trimmed(raw.tests),
    };
    (!handoff.is_empty()).then_some(handoff)
}

/// The reply with the `usine-handoff` block removed, for the transcript (the
/// block is rendered as its own panel section, not as agent prose).
pub fn strip_handoff_block(text: &str) -> String {
    block::strip(text, TAG)
}

/// Trim each entry and drop the ones that were only whitespace.
fn trimmed(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_handoff() {
        let text = "Done.\n\n```usine-handoff\n{\
            \"summary\": \"Added the filter panel. \",\
            \"questions\": [\"Should the filter persist across reloads?\"],\
            \"tests\": [\"Filter by owner, then reload — the list should reset\", \"  \"]\
            }\n```";
        let h = parse_handoff(text).unwrap();
        assert_eq!(h.summary, "Added the filter panel.", "summary is trimmed");
        assert_eq!(h.questions.len(), 1);
        assert_eq!(
            h.tests.len(),
            1,
            "the whitespace-only test entry is dropped"
        );
    }

    #[test]
    fn a_partial_handoff_still_parses() {
        // No questions, no tests: a change with nothing to ask and nothing to click.
        let h = parse_handoff("```usine-handoff\n{\"summary\":\"Renamed a field.\"}\n```").unwrap();
        assert_eq!(h.summary, "Renamed a field.");
        assert!(h.questions.is_empty() && h.tests.is_empty());

        // Only a checklist: no prose, but still worth showing.
        let h = parse_handoff("```usine-handoff\n{\"tests\":[\"Open the board\"]}\n```").unwrap();
        assert!(h.summary.is_empty());
        assert_eq!(h.tests, vec!["Open the board"]);
    }

    #[test]
    fn missing_malformed_or_empty_yields_none() {
        assert_eq!(parse_handoff("no block"), None);
        assert_eq!(parse_handoff("```usine-handoff\nnot json\n```"), None);
        assert_eq!(parse_handoff("```usine-handoff\n{}\n```"), None);
        // Present but vacuous — nothing to render.
        assert_eq!(
            parse_handoff("```usine-handoff\n{\"summary\":\"  \",\"tests\":[\" \"]}\n```"),
            None
        );
    }

    #[test]
    fn strips_the_block_but_leaves_the_commit_one() {
        let text = "All done.\n\n```usine-handoff\n{\"summary\":\"x\"}\n```\n\n```usine-commit\nfeat: x\n```";
        let stripped = strip_handoff_block(text);
        assert!(!stripped.contains("usine-handoff"));
        assert!(
            stripped.contains("```usine-commit"),
            "the commit block is another parser's to remove"
        );
    }
}
