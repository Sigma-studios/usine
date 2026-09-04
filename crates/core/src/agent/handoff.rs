//! The implementation hand-off: what the agent tells the human who reviews it.
//!
//! An implement run ends with the work committed in the card's worktree and the
//! card parked at the awaiting-review gate. The user arrives there with no idea
//! how the run went — whether the agent hit something surprising, made a call
//! that could have gone the other way, or left part of the task undone.
//!
//! So the run is asked to append a fenced ` ```usine-handoff ` JSON block: a
//! recap of the work done, the files it touched, any open questions, the risks
//! it took, and the things worth testing by hand.
//! [`parse_handoff`] pulls it out; the executor stores it on the card and the
//! detail panel renders it beside the self-review and PR buttons. Same
//! prompt-and-parse shape as [`crate::agent::commit`] and [`crate::agent::review`],
//! and just as tolerant: a run that emits no block simply has no hand-off.
//!
//! # Schema v2
//!
//! v1 was `{summary, questions[], tests[]}` with `tests` a list of strings, the
//! per-change lines buried in `summary`'s prose and a `[verified]` string prefix
//! standing in for a field. v2 promotes each of those to real data —
//! `changes[]`, `risks[]`, `tests[].verified` — so the panel can tab them apart
//! instead of printing one blob.
//!
//! v1 stays readable forever: every field is `#[serde(default)]` and
//! [`TestItem`] deserializes from a bare string, which matters twice over —
//! for an agent that emits the old shape, and for the v1 hand-offs already
//! serialized into `CardReviewRecord`.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::agent::block;

const TAG: &str = "usine-handoff";

/// The schema version this build emits and assumes when a payload omits `v`.
const VERSION: u8 = 2;

/// Appended to implement runs (not to fix runs, which report through their own
/// recap — see [`crate::agent::fixes`]). Kept provider-agnostic and
/// self-contained.
pub const HANDOFF_INSTRUCTION: &str = "\
When you have finished making changes, hand the work off to the human who will review it. Emit a \
fenced code block tagged `usine-handoff` containing a JSON object shaped like \
{\"v\": 2, \"summary\": \"<what was done>\", \"changes\": [{\"path\": \"<file>\", \"what\": \
\"<what changed there>\", \"kind\": \"feat|fix|test|docs|chore\"}], \"tests\": [{\"scenario\": \
\"<what to do>\", \"expect\": \"<what should happen>\", \"verified\": false}], \"risks\": \
[\"<what could bite>\"], \"questions\": [\"<open question>\"]}.\n\
- `summary`: a `TL;DR:` line followed by 1-3 bullet points (`- ` lines) giving the headline \
outcome, for someone who did not watch you work. Keep it short — the per-file detail belongs in \
`changes`.\n\
- `changes`: one entry per file you meaningfully touched, covering everything in the diff \
(features, refactors, tests, docs, config). Say plainly in `what` anything you had to work \
around or deliberately left out; if part of the task is unfinished, or you are unsure a change is \
right, say so there.\n\
- `tests`: what is worth checking by hand in the running app, most important first. `scenario` is \
what to do, `expect` is what should happen. Favour what your automated tests do not already cover: \
the risky paths, the edge cases you touched, the flows a regression would hide in. Set \
`verified: true` only for a scenario you actually exercised yourself in the running app — it still \
deserves a human eye, but the reviewer should know it has been run once. Use an empty array if \
the change has no observable behaviour.\n\
- `risks`: what could bite the reviewer or a user — a migration, a behaviour change, something \
you could not test. Empty array if none.\n\
- `questions`: what you genuinely want the author to weigh in on — a judgement call that could \
reasonably have gone the other way, an ambiguity in the task, an assumption you had to invent. Use \
an empty array if you have none; do NOT manufacture questions to fill it.\n\
Be honest and specific — this is the note a careful engineer leaves a colleague, not a sales pitch.";

/// One file the run touched, as the agent describes it. The path is a claim,
/// not a fact: the panel cross-checks it against the computed diff and marks
/// anything that isn't actually there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Repo-relative path, as the agent wrote it.
    #[serde(default)]
    pub path: String,
    /// What changed in that file.
    #[serde(default)]
    pub what: String,
    /// `feat` / `fix` / `test` / `docs` / `chore`. Free-form: an agent that
    /// invents a word gets it rendered as-is rather than dropped.
    #[serde(default)]
    pub kind: String,
}

/// One thing worth exercising by hand.
///
/// Deserializes from either the v2 object or a bare v1 string — in which case
/// the whole string is the scenario, and a leading `[verified]` marker (the
/// convention v2 replaces with a field) is consumed rather than displayed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TestItem {
    /// What to do.
    #[serde(default)]
    pub scenario: String,
    /// What should happen. Empty for a v1 entry, which said both at once.
    #[serde(default)]
    pub expect: String,
    /// The agent claims to have exercised this itself in the running app.
    #[serde(default)]
    pub verified: bool,
}

impl<'de> Deserialize<'de> for TestItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The object form, spelled out so the string arm can't recurse into it.
        #[derive(Deserialize)]
        struct Object {
            #[serde(default)]
            scenario: String,
            #[serde(default)]
            expect: String,
            #[serde(default)]
            verified: bool,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Either {
            Text(String),
            Object(Object),
        }

        match Either::deserialize(deserializer).map_err(de::Error::custom)? {
            Either::Text(text) => Ok(TestItem::from_v1(&text)),
            Either::Object(o) => Ok(TestItem {
                scenario: o.scenario.trim().to_string(),
                expect: o.expect.trim().to_string(),
                verified: o.verified,
            }),
        }
    }
}

/// A plain checklist line, in the v1 spelling — the shape both a legacy
/// payload and a hand-written caller reach for.
impl From<&str> for TestItem {
    fn from(text: &str) -> Self {
        TestItem::from_v1(text)
    }
}

impl TestItem {
    /// A v1 checklist line, with the `[verified]` prefix migrated to the field.
    fn from_v1(text: &str) -> Self {
        let text = text.trim();
        match text.strip_prefix("[verified]") {
            Some(rest) => TestItem {
                scenario: rest.trim().to_string(),
                expect: String::new(),
                verified: true,
            },
            None => TestItem {
                scenario: text.to_string(),
                expect: String::new(),
                verified: false,
            },
        }
    }

    fn is_empty(&self) -> bool {
        self.scenario.is_empty() && self.expect.is_empty()
    }
}

/// An implement run's hand-off to its reviewer. Every field is optional: an agent
/// with nothing to ask emits `questions: []`, and a change with no observable
/// behaviour emits `tests: []`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Handoff {
    /// Payload schema version. Absent (v1 payloads, and v1 records already on
    /// disk) reads as 1.
    #[serde(default = "v1")]
    pub v: u8,
    /// A short recap of the work done: a `TL;DR:` header with bullet points.
    /// In v1 payloads this also carries the per-change lines.
    #[serde(default)]
    pub summary: String,
    /// The files the run says it touched. Empty for v1.
    #[serde(default)]
    pub changes: Vec<Change>,
    /// Things worth testing by hand, most important first.
    #[serde(default)]
    pub tests: Vec<TestItem>,
    /// What could bite the reviewer or a user. Empty for v1.
    #[serde(default)]
    pub risks: Vec<String>,
    /// Decisions or ambiguities the agent wants the user to weigh in on.
    #[serde(default)]
    pub questions: Vec<String>,
    /// Not part of the payload: set by [`handoff_from_reply`] when the agent
    /// emitted a `usine-handoff` block whose JSON didn't parse. The panel says
    /// so, rather than leaving the reviewer to read "no hand-off" as "the run
    /// had nothing to say".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub malformed: bool,
}

fn v1() -> u8 {
    1
}

impl Default for Handoff {
    fn default() -> Self {
        Handoff {
            v: VERSION,
            summary: String::new(),
            changes: Vec::new(),
            tests: Vec::new(),
            risks: Vec::new(),
            questions: Vec::new(),
            malformed: false,
        }
    }
}

impl Handoff {
    /// Nothing to show. The executor stores this as "no hand-off" rather than an
    /// empty card section.
    pub fn is_empty(&self) -> bool {
        !self.malformed
            && self.summary.trim().is_empty()
            && self.changes.is_empty()
            && self.tests.is_empty()
            && self.risks.is_empty()
            && self.questions.is_empty()
    }
}

/// Extract the agent's hand-off from its `usine-handoff` block. `None` when the
/// block is missing, malformed, or carries nothing — in every case the card ends
/// up with no hand-off rather than a broken one. Use [`handoff_block_malformed`]
/// to tell "the agent said nothing" from "the agent garbled it".
pub fn parse_handoff(text: &str) -> Option<Handoff> {
    let raw = block::parse::<Handoff>(text, TAG).0.ok()?;
    let handoff = Handoff {
        v: raw.v,
        malformed: false,
        summary: raw.summary.trim().to_string(),
        changes: raw
            .changes
            .into_iter()
            .map(|c| Change {
                path: c.path.trim().to_string(),
                what: c.what.trim().to_string(),
                kind: c.kind.trim().to_string(),
            })
            .filter(|c| !(c.path.is_empty() && c.what.is_empty()))
            .collect(),
        tests: raw.tests.into_iter().filter(|t| !t.is_empty()).collect(),
        risks: trimmed(raw.risks),
        questions: trimmed(raw.questions),
    };
    (!handoff.is_empty()).then_some(handoff)
}

/// The hand-off to store for a finished implement run: the parsed one, or a
/// hand-off flagged [`Handoff::malformed`] when the agent emitted a block that
/// didn't parse, or the empty (= "none") one when it emitted no block at all.
pub fn handoff_from_reply(text: &str) -> Handoff {
    match parse_handoff(text) {
        Some(handoff) => handoff,
        None if handoff_block_malformed(text) => Handoff {
            malformed: true,
            ..Handoff::default()
        },
        None => Handoff::default(),
    }
}

/// True when the reply carries a `usine-handoff` block whose JSON doesn't parse
/// — the agent tried to hand off and garbled it. [`parse_handoff`] yields
/// nothing then; the panel uses this to say the note was dropped rather than
/// leaving the reviewer to assume the run had nothing to say.
pub fn handoff_block_malformed(text: &str) -> bool {
    block::parse::<Handoff>(text, TAG).0.is_malformed()
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
    fn parses_a_full_v2_handoff() {
        let text = "Done.\n\n```usine-handoff\n{\
            \"v\": 2,\
            \"summary\": \"TL;DR: added the filter panel. \",\
            \"changes\": [{\"path\":\" src/ui/filter.rs \",\"what\":\"new panel\",\"kind\":\"feat\"}, {\"path\":\"\",\"what\":\"\"}],\
            \"tests\": [{\"scenario\":\"Filter by owner\",\"expect\":\"the list narrows\",\"verified\":true}],\
            \"risks\": [\"The saved filter is not migrated\", \" \"],\
            \"questions\": [\"Should the filter persist across reloads?\"]\
            }\n```";
        let h = parse_handoff(text).unwrap();
        assert_eq!(h.v, 2);
        assert_eq!(
            h.summary, "TL;DR: added the filter panel.",
            "summary is trimmed"
        );
        assert_eq!(h.changes.len(), 1, "the empty change entry is dropped");
        assert_eq!(h.changes[0].path, "src/ui/filter.rs", "paths are trimmed");
        assert_eq!(h.tests[0].expect, "the list narrows");
        assert!(h.tests[0].verified);
        assert_eq!(h.risks, vec!["The saved filter is not migrated"]);
        assert_eq!(h.questions.len(), 1);
    }

    #[test]
    fn a_v1_payload_still_parses_and_migrates_the_verified_prefix() {
        let text = "```usine-handoff\n{\
            \"summary\": \"Renamed a field.\",\
            \"questions\": [],\
            \"tests\": [\"[verified] Open the board — it loads\", \"Reload\", \"  \"]\
            }\n```";
        let h = parse_handoff(text).unwrap();
        assert_eq!(h.v, 1, "a payload without `v` reads as v1");
        assert_eq!(h.tests.len(), 2, "the whitespace-only entry is dropped");
        assert_eq!(h.tests[0].scenario, "Open the board — it loads");
        assert!(h.tests[0].verified, "the [verified] prefix became a field");
        assert!(!h.tests[1].verified);
        assert!(h.changes.is_empty() && h.risks.is_empty());
    }

    #[test]
    fn a_v1_persisted_record_round_trips_through_the_v2_struct() {
        // What `CardReviewRecord.handoff` holds for a card handed off before v2.
        let stored =
            "{\"summary\":\"did the thing\",\"questions\":[\"why?\"],\"tests\":[\"click it\"]}";
        let h: Handoff = serde_json::from_str(stored).unwrap();
        assert_eq!(h.summary, "did the thing");
        assert_eq!(h.tests[0].scenario, "click it");
        // …and re-serializing it yields the v2 shape the panel reads back.
        let again: Handoff = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(again, h);
    }

    #[test]
    fn a_partial_handoff_still_parses() {
        // No questions, no tests: a change with nothing to ask and nothing to click.
        let h = parse_handoff("```usine-handoff\n{\"summary\":\"Renamed a field.\"}\n```").unwrap();
        assert_eq!(h.summary, "Renamed a field.");
        assert!(h.questions.is_empty() && h.tests.is_empty());

        // Only changes: no prose, but still worth showing.
        let h = parse_handoff(
            "```usine-handoff\n{\"changes\":[{\"path\":\"a.rs\",\"what\":\"x\"}]}\n```",
        )
        .unwrap();
        assert!(h.summary.is_empty());
        assert_eq!(h.changes[0].path, "a.rs");
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
        // Only the garbled case is worth telling the user about.
        assert!(handoff_block_malformed("```usine-handoff\nnot json\n```"));
        assert!(!handoff_block_malformed("no block"));
        assert!(!handoff_block_malformed("```usine-handoff\n{}\n```"));
    }

    #[test]
    fn a_summary_quoting_a_code_fence_survives() {
        // The regression Phase 0 fixed: the inner fence used to close the block.
        let text = "Done.\n\n```usine-handoff\n{\"summary\":\"Run ```cargo test``` first.\"}\n```";
        let h = parse_handoff(text).unwrap();
        assert_eq!(h.summary, "Run ```cargo test``` first.");
    }

    #[test]
    fn a_garbled_block_is_stored_as_a_malformed_handoff() {
        let h = handoff_from_reply("```usine-handoff\nnot json\n```");
        assert!(h.malformed);
        assert!(!h.is_empty(), "malformed is worth a panel line of its own");
        // No block at all stays "no hand-off".
        assert!(handoff_from_reply("just prose").is_empty());
        // It survives a store round-trip, so the panel still says so after a
        // restart — but it never clutters an ordinary hand-off's JSON.
        let back: Handoff = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert!(back.malformed);
        let ordinary = parse_handoff("```usine-handoff\n{\"summary\":\"x\"}\n```").unwrap();
        assert!(!serde_json::to_string(&ordinary)
            .unwrap()
            .contains("malformed"));
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

    #[test]
    fn the_instruction_states_the_v2_contract() {
        for field in ["\"v\": 2", "changes", "risks", "verified"] {
            assert!(HANDOFF_INSTRUCTION.contains(field), "missing {field}");
        }
    }
}
