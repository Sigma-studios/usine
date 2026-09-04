//! What a fix run actually did, finding by finding.
//!
//! The fix picker is the app's most structured screen: the user reads each
//! finding, ticks the ones worth fixing, and steers the ones that need it. Then
//! the run comes back with a paragraph of prose, and every bit of that structure
//! is gone at exactly the moment it matters — the merge gate. "Fixed 2 of 3"
//! was something you had to read for, and a finding the agent quietly decided
//! not to touch looked the same as one it fixed.
//!
//! So a fix run is asked to append a fenced ` ```usine-fixes ` block naming, per
//! finding id, what it did. The ids are the picker's own
//! ([`FixVerdict::comment`]'s `id`, real GitHub comment ids on a PR and
//! synthesised-but-stable ones for a self-review), so the outcomes join straight
//! back onto the rows the user ticked.
//!
//! The stashed [`FixItem`]s are the other half: the run's report is only
//! meaningful next to what was asked for, and a finding the agent never
//! mentions has to be visible as *unreported*, not simply absent. Both halves
//! land in a [`FixReport`] when the run's commit is real — the same rule that
//! governs the restart log and the GitHub thread resolves.
//!
//! Nothing here posts anything: the outcomes are usine's own record. In
//! particular a finding reported `skipped` still has its GitHub thread resolved
//! on commit, because that follows the checkboxes; this panel is what makes
//! that visible.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::agent::block;
use crate::domain::model::FixVerdict;

const TAG: &str = "usine-fixes";

/// Appended to fix runs, whose stripped final message becomes the "Fixes recap"
/// the user reads at the merge step. Must NOT mention the hand-off block tag:
/// fix runs report through this recap, not a hand-off.
pub const FIX_RECAP_INSTRUCTION: &str = "\
Your final message is shown to the user as the recap of this fix run. Start it with a `TL;DR:` — \
1-3 short bullet points saying what you changed (and anything you judged not worth changing or \
could not fix) — then any detail worth keeping.\n\
Then, if the task above listed findings with `(#<id>)` markers, append a fenced code block tagged \
`usine-fixes` containing a JSON object shaped like {\"v\": 1, \"outcomes\": [{\"id\": <id>, \
\"outcome\": \"fixed|partial|skipped\", \"note\": \"<one line: what you did, or why not>\"}]} — \
one entry per listed finding, using its exact id. `fixed` means you addressed it in full, \
`partial` that you addressed some of it, `skipped` that you deliberately changed nothing. Report \
every finding you were given, including the ones you left alone: the reviewer sees this as a \
checklist against what they asked for.";

/// What the run did about one finding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Fixed,
    Partial,
    Skipped,
    /// The agent used a word we don't know. Rendered as-is rather than guessed
    /// at — misreading "deferred" as "fixed" is the one mistake this panel
    /// exists to prevent.
    #[default]
    Unclear,
}

impl<'de> Deserialize<'de> for Outcome {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer).map_err(de::Error::custom)?;
        Ok(match raw.trim().to_ascii_lowercase().as_str() {
            "fixed" | "done" => Outcome::Fixed,
            "partial" | "partially" => Outcome::Partial,
            "skipped" | "skip" | "wontfix" | "won't fix" => Outcome::Skipped,
            _ => Outcome::Unclear,
        })
    }
}

impl Outcome {
    /// The word the panel shows on the pill.
    pub fn label(self) -> &'static str {
        match self {
            Outcome::Fixed => "fixed",
            Outcome::Partial => "partial",
            Outcome::Skipped => "skipped",
            Outcome::Unclear => "unclear",
        }
    }
}

/// The run's report on one finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixOutcome {
    /// The finding's id, as the picker showed it to the agent.
    pub id: u64,
    #[serde(default)]
    pub outcome: Outcome,
    /// What was done, or why nothing was.
    #[serde(default)]
    pub note: String,
}

/// One finding the user ticked, as the picker had it — the "what was asked
/// for" half of the report, stashed at launch so the outcomes have something to
/// be checked against.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixItem {
    pub id: u64,
    /// The finding's text, one line, as shown in the picker.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    /// `critical` | `high` | `medium` | `low`, or empty when unrated.
    #[serde(default)]
    pub severity: String,
}

/// The label a synthetic item built from a review's *body* carries in `path`.
/// It names no file, so anything that treats `path` as a repo path (opening the
/// diff at it, say) has to skip it.
pub const REVIEW_BODY_PATH: &str = "PR review summary";

impl FixItem {
    /// The repo path this item points at, when it points at one at all.
    pub fn diff_path(&self) -> Option<&str> {
        (!self.path.is_empty() && self.path != REVIEW_BODY_PATH).then_some(self.path.as_str())
    }
}

/// What a fix run was asked to do and what it reports having done. Stored per
/// card once the run's commit is real.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixReport {
    #[serde(default)]
    pub v: u8,
    /// The findings the user checked, in picker order.
    #[serde(default)]
    pub items: Vec<FixItem>,
    /// What the run said about them. May be shorter than `items` (a run that
    /// under-reported), longer (ids we never asked about), or empty (a run that
    /// emitted no block at all).
    #[serde(default)]
    pub outcomes: Vec<FixOutcome>,
    /// The run emitted a `usine-fixes` block whose JSON didn't parse.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub malformed: bool,
}

/// One line of the merge gate's checklist: what was asked, and what came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixRow {
    /// The finding as the picker had it. `None` for an outcome reporting an id
    /// nobody asked about.
    pub item: Option<FixItem>,
    /// What the run said. `None` for a checked finding the run never mentioned.
    pub outcome: Option<FixOutcome>,
}

impl FixReport {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.outcomes.is_empty() && !self.malformed
    }

    /// The checklist: every finding the user checked in picker order, each with
    /// the run's outcome if it reported one, then any outcome whose id was never
    /// asked about. Nothing is dropped in either direction — an under-report and
    /// an over-report are both things the reviewer needs to see.
    pub fn rows(&self) -> Vec<FixRow> {
        let mut rows: Vec<FixRow> = self
            .items
            .iter()
            .map(|item| FixRow {
                item: Some(item.clone()),
                outcome: self.outcomes.iter().find(|o| o.id == item.id).cloned(),
            })
            .collect();
        for outcome in &self.outcomes {
            if !self.items.iter().any(|i| i.id == outcome.id) {
                rows.push(FixRow {
                    item: None,
                    outcome: Some(outcome.clone()),
                });
            }
        }
        rows
    }

    /// `(reported fixed or partial, findings asked for)` — the "3 of 4" the
    /// merge gate leads with.
    pub fn tally(&self) -> (usize, usize) {
        let addressed = self
            .items
            .iter()
            .filter(|item| {
                self.outcomes.iter().any(|o| {
                    o.id == item.id && matches!(o.outcome, Outcome::Fixed | Outcome::Partial)
                })
            })
            .count();
        (addressed, self.items.len())
    }
}

/// The picker rows a fix run is about to be sent, as the report's "asked for"
/// half. Follows the CHECKBOXES, like every other piece of fix bookkeeping: an
/// edited task that drops a finding must not make its row disappear from the
/// checklist the user gets back.
pub fn fix_items(selected: &[FixVerdict]) -> Vec<FixItem> {
    selected
        .iter()
        .map(|v| FixItem {
            id: v.comment.id,
            label: crate::agent::executor::one_line_capped(&v.comment.body, 160),
            path: if v.comment.review_body_of.is_some() {
                REVIEW_BODY_PATH.to_string()
            } else {
                v.comment.path.clone()
            },
            line: v
                .comment
                .line
                .filter(|_| v.comment.review_body_of.is_none()),
            severity: v.severity.clone(),
        })
        .collect()
}

/// Extract the run's per-finding outcomes from its `usine-fixes` block. `None`
/// when the block is missing or carries nothing; see [`fixes_block_malformed`]
/// for the garbled case.
pub fn parse_fix_outcomes(text: &str) -> Option<Vec<FixOutcome>> {
    /// Just the outcomes; the block's `summary`, when present, duplicates the
    /// prose recap and is ignored.
    #[derive(Deserialize)]
    struct Payload {
        #[serde(default)]
        outcomes: Vec<FixOutcome>,
    }

    let outcomes = block::parse::<Payload>(text, TAG).0.ok()?.outcomes;
    (!outcomes.is_empty()).then_some(outcomes)
}

/// True when the reply carries a `usine-fixes` block whose JSON doesn't parse.
pub fn fixes_block_malformed(text: &str) -> bool {
    block::parse::<serde_json::Value>(text, TAG)
        .0
        .is_malformed()
}

/// The reply with the `usine-fixes` block removed, for the prose recap.
pub fn strip_fixes_block(text: &str) -> String {
    block::strip(text, TAG)
}

/// The report to store for a finished fix run: what it was asked for (stashed
/// at launch) joined to what it says it did.
pub fn fix_report(items: Vec<FixItem>, text: &str) -> FixReport {
    FixReport {
        v: 1,
        items,
        outcomes: parse_fix_outcomes(text).unwrap_or_default(),
        malformed: fixes_block_malformed(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: u64, label: &str) -> FixItem {
        FixItem {
            id,
            label: label.into(),
            path: "a.rs".into(),
            line: Some(3),
            severity: "high".into(),
        }
    }

    #[test]
    fn parses_outcomes_and_normalises_the_verb() {
        let text = "TL;DR: fixed two.\n\n```usine-fixes\n{\"v\":1,\"outcomes\":[\
            {\"id\":7,\"outcome\":\"fixed\",\"note\":\"guarded the index\"},\
            {\"id\":8,\"outcome\":\"Partially\"},\
            {\"id\":9,\"outcome\":\"deferred\",\"note\":\"needs a migration\"}]}\n```";
        let outcomes = parse_fix_outcomes(text).unwrap();
        assert_eq!(outcomes[0].outcome, Outcome::Fixed);
        assert_eq!(outcomes[0].note, "guarded the index");
        assert_eq!(outcomes[1].outcome, Outcome::Partial, "case-insensitive");
        assert_eq!(
            outcomes[2].outcome,
            Outcome::Unclear,
            "an unknown verb is never guessed into fixed"
        );
        assert_eq!(strip_fixes_block(text), "TL;DR: fixed two.");
    }

    #[test]
    fn a_missing_or_garbled_block_is_told_apart() {
        assert_eq!(parse_fix_outcomes("just prose"), None);
        assert!(!fixes_block_malformed("just prose"));
        assert_eq!(parse_fix_outcomes("```usine-fixes\nnope\n```"), None);
        assert!(fixes_block_malformed("```usine-fixes\nnope\n```"));
        // Valid but empty: the run reported nothing, which is not garbled.
        assert_eq!(
            parse_fix_outcomes("```usine-fixes\n{\"outcomes\":[]}\n```"),
            None
        );
        assert!(!fixes_block_malformed(
            "```usine-fixes\n{\"outcomes\":[]}\n```"
        ));
    }

    #[test]
    fn rows_show_under_and_over_reporting() {
        let report = fix_report(
            vec![item(7, "off-by-one"), item(8, "missing test")],
            "```usine-fixes\n{\"outcomes\":[{\"id\":7,\"outcome\":\"fixed\"},\
             {\"id\":99,\"outcome\":\"fixed\",\"note\":\"tidied this too\"}]}\n```",
        );
        let rows = report.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].outcome.as_ref().unwrap().outcome, Outcome::Fixed);
        assert!(
            rows[1].outcome.is_none(),
            "a checked finding the run never mentioned stays visible"
        );
        assert!(
            rows[2].item.is_none(),
            "an id nobody asked about is reported, not dropped"
        );
        assert_eq!(report.tally(), (1, 2));
    }

    #[test]
    fn a_run_that_reported_nothing_still_yields_the_asked_for_checklist() {
        let report = fix_report(vec![item(7, "off-by-one")], "TL;DR: done.");
        assert!(!report.is_empty());
        assert!(!report.malformed);
        assert_eq!(report.tally(), (0, 1));
        assert_eq!(report.rows()[0].outcome, None);
    }

    #[test]
    fn the_instruction_states_the_contract_without_naming_the_handoff() {
        assert!(FIX_RECAP_INSTRUCTION.contains("usine-fixes"));
        assert!(FIX_RECAP_INSTRUCTION.contains("skipped"));
        assert!(!FIX_RECAP_INSTRUCTION.contains("usine-handoff"));
    }
}
