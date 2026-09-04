//! Structured questions a plan can surface for the user to answer.
//!
//! Headless `claude` has no live question tool, so instead we ask the agent to
//! append a machine-readable `usine-questions` block to its plan; [`parse_plan`]
//! splits that out so the UI can render real multiple-choice + free-form inputs,
//! and the answers are fed back to refine the plan.

use serde::{Deserialize, Serialize};

use crate::agent::block;
use crate::domain::model::Provider;

/// Appended to every plan-phase prompt so the agent surfaces decisions in a
/// parseable form. Provider-neutral — [`plan_instruction`] adds the
/// Claude-specific addendum.
pub const PLAN_QUESTIONS_INSTRUCTION: &str = "\
Write your complete implementation plan out in full as your final response. \
Start it with a `TL;DR:` — 2-5 short bullet points summarizing what will be done and the key \
decisions — then the full plan. \
This runs headlessly with no follow-up turn: you cannot pause to wait for \
background work, sub-agents, or tool results and resume later. If you launch any exploration, \
finish it and fold the findings into your plan before you respond — never end your turn by \
saying you will wait for results.\n\n\
If — and only if — you need the user to make decisions about implementation details or product \
choices before this can be implemented, end your plan with a fenced code block tagged \
`usine-questions` containing a JSON array of questions, each shaped like \
{\"question\": \"...\", \"options\": [\"Option A\", \"Option B\"]}. Give 2-4 short options per \
question (the user can also type their own answer). If you have no such questions, omit the block.";

/// Claude-only addendum: a headless `claude` plan run has no ExitPlanMode
/// approver (see `PlanGate` in the provider), so steer the agent away from the
/// tool. Codex has no such tool, so this would only be noise there.
const CLAUDE_NO_EXIT_PLAN_MODE: &str = "\
Do not try to call ExitPlanMode: it is not available in headless runs (it is absent from the tool \
list and from ToolSearch's deferred registry), and your final response is what gets captured \
either way.";

/// The full plan-phase instruction for `provider`: the provider-neutral
/// [`PLAN_QUESTIONS_INSTRUCTION`] plus, for Claude, the ExitPlanMode warning.
pub fn plan_instruction(provider: Provider) -> String {
    match provider {
        Provider::Claude => format!("{PLAN_QUESTIONS_INSTRUCTION}\n\n{CLAUDE_NO_EXIT_PLAN_MODE}"),
        Provider::Codex => PLAN_QUESTIONS_INSTRUCTION.to_string(),
    }
}

/// One step of the plan, as the agent's own outline of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub files: Vec<String>,
}

/// A structured view of the plan the agent wrote out in prose: the same
/// content, split so the approval screen can show the steps, the blast radius
/// and the verification plan side by side instead of as one 200-line scroller.
///
/// Strictly a *view*. The prose plan remains the payload — it is what gets fed
/// verbatim into the implement prompt and into every read-only run's background
/// block — so a missing or garbled outline costs presentation, never work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanOutline {
    #[serde(default)]
    pub v: u8,
    /// The headline bullets, matching the plan's own `TL;DR:`.
    #[serde(default)]
    pub tldr: Vec<String>,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    /// Every file the plan expects to touch — the blast radius at a glance.
    #[serde(default)]
    pub files: Vec<String>,
    /// How the result will be checked.
    #[serde(default)]
    pub verification: Vec<String>,
    #[serde(default)]
    pub risks: Vec<String>,
}

impl PlanOutline {
    pub fn is_empty(&self) -> bool {
        self.tldr.is_empty()
            && self.steps.is_empty()
            && self.files.is_empty()
            && self.verification.is_empty()
            && self.risks.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
}

const TAG: &str = "usine-questions";
const OUTLINE_TAG: &str = "usine-plan";

/// Split a plan into (prose with the questions block removed, parsed questions).
/// With no complete block, returns the plan unchanged and no questions. A
/// complete block is always stripped — even when its JSON is malformed (no
/// questions then; see [`plan_block_malformed`]), so the garbage never badges
/// the plan as questioned nor leaks into the implement prompt.
pub fn parse_plan(plan: &str) -> (String, Vec<PlanQuestion>) {
    let (prose, questions) = parse_questions(plan);
    // The outline block is a view of this same prose, addressed to the approval
    // screen. Stripping it here keeps it out of the implement prompt and out of
    // every background block that quotes the plan.
    (block::strip(&prose, OUTLINE_TAG), questions)
}

/// The agent's structured view of its own plan, when it emitted one. `None`
/// when the block is missing, garbled, or vacuous — the prose plan is the
/// payload either way.
pub fn parse_plan_outline(plan: &str) -> Option<PlanOutline> {
    let outline = block::parse::<PlanOutline>(plan, OUTLINE_TAG).0.ok()?;
    (!outline.is_empty()).then_some(outline)
}

/// True when the plan carries a `usine-plan` block whose JSON doesn't parse.
pub fn plan_outline_malformed(plan: &str) -> bool {
    block::parse::<PlanOutline>(plan, OUTLINE_TAG)
        .0
        .is_malformed()
}

/// [`parse_plan`] under the name the non-plan callers use: the same
/// `usine-questions` block is how a conflict-resolution run asks for a
/// decision, and there is nothing plan-specific about splitting it out.
pub fn parse_questions(text: &str) -> (String, Vec<PlanQuestion>) {
    let (parsed, prose) = block::parse::<Vec<PlanQuestion>>(text, TAG);
    (prose, parsed.ok().unwrap_or_default())
}

/// True when the plan carries a questions block whose JSON doesn't parse — the
/// agent tried to ask something but garbled it. [`parse_plan`] strips the block
/// silently; the plan panel uses this to tell the user something was dropped.
/// A valid-but-empty `[]` is not malformed.
pub fn plan_block_malformed(plan: &str) -> bool {
    block::parse::<Vec<PlanQuestion>>(plan, TAG)
        .0
        .is_malformed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_strips_questions_block() {
        let plan = "Here is the plan.\n\n```usine-questions\n[{\"question\":\"DB?\",\"options\":[\"SQLite\",\"Postgres\"]}]\n```\n";
        let (clean, qs) = parse_plan(plan);
        assert!(!clean.contains("usine-questions"));
        assert_eq!(clean, "Here is the plan.");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "DB?");
        assert_eq!(qs[0].options, vec!["SQLite", "Postgres"]);
    }

    #[test]
    fn the_outline_is_a_view_and_never_reaches_the_implement_prompt() {
        let plan = "Here is the plan.\n\n```usine-plan\n{\"v\":1,\"tldr\":[\"Do it\"],\
            \"steps\":[{\"title\":\"Extract\",\"detail\":\"Move it out\",\"files\":[\"a.rs\"]}],\
            \"files\":[\"a.rs\"],\"verification\":[\"cargo test\"],\"risks\":[]}\n```\n";
        let (clean, qs) = parse_plan(plan);
        assert_eq!(
            clean, "Here is the plan.",
            "the block is stripped from the prose"
        );
        assert!(qs.is_empty());
        let outline = parse_plan_outline(plan).unwrap();
        assert_eq!(outline.tldr, vec!["Do it"]);
        assert_eq!(outline.steps[0].files, vec!["a.rs"]);
        assert_eq!(outline.verification, vec!["cargo test"]);
        assert!(!plan_outline_malformed(plan));
    }

    #[test]
    fn a_garbled_or_absent_outline_leaves_the_plan_alone() {
        assert_eq!(parse_plan_outline("just a plan"), None);
        assert!(!plan_outline_malformed("just a plan"));
        let plan = "Plan.\n```usine-plan\nnot json\n```";
        assert_eq!(parse_plan_outline(plan), None);
        assert!(plan_outline_malformed(plan));
        assert_eq!(
            parse_plan(plan).0,
            "Plan.",
            "garbage never reaches the prompt"
        );
        // A block with nothing in it is not worth a tab.
        assert_eq!(parse_plan_outline("```usine-plan\n{\"v\":1}\n```"), None);
    }

    #[test]
    fn both_blocks_can_ride_along_together() {
        let plan = "Plan.\n\n```usine-questions\n[{\"question\":\"DB?\",\"options\":[]}]\n```\n\n\
            ```usine-plan\n{\"files\":[\"a.rs\"]}\n```";
        let (clean, qs) = parse_plan(plan);
        assert_eq!(clean, "Plan.");
        assert_eq!(qs.len(), 1);
        assert_eq!(parse_plan_outline(plan).unwrap().files, vec!["a.rs"]);
    }

    #[test]
    fn no_block_returns_plan_unchanged() {
        let (clean, qs) = parse_plan("just a plan");
        assert_eq!(clean, "just a plan");
        assert!(qs.is_empty());
    }

    #[test]
    fn plan_instruction_gates_the_exit_plan_mode_note_by_provider() {
        let claude = plan_instruction(Provider::Claude);
        let codex = plan_instruction(Provider::Codex);
        // Both providers get the questions-block contract…
        assert!(claude.contains("usine-questions"));
        assert!(codex.contains("usine-questions"));
        // …both are asked to open the plan with a TL;DR…
        assert!(claude.contains("TL;DR"));
        assert!(codex.contains("TL;DR"));
        // …but only Claude is warned off its own ExitPlanMode tool.
        assert!(claude.contains("ExitPlanMode"));
        assert!(!codex.contains("ExitPlanMode"));
    }

    #[test]
    fn malformed_block_is_stripped_and_flagged() {
        let plan = "Plan.\n```usine-questions\nnot json\n```";
        let (clean, qs) = parse_plan(plan);
        // The garbage block must not survive into the badge logic or the
        // implement prompt — stripped, with no questions.
        assert_eq!(clean, "Plan.");
        assert!(qs.is_empty());
        assert!(plan_block_malformed(plan));
    }

    #[test]
    fn valid_or_absent_blocks_are_not_malformed() {
        // A valid empty array strips silently: nothing was lost.
        let plan = "Plan.\n```usine-questions\n[]\n```";
        let (clean, qs) = parse_plan(plan);
        assert_eq!(clean, "Plan.");
        assert!(qs.is_empty());
        assert!(!plan_block_malformed(plan));

        assert!(!plan_block_malformed("just a plan"));
        // An unterminated fence isn't a complete block — left untouched.
        let dangling = "Plan.\n```usine-questions\nnot json";
        assert_eq!(parse_plan(dangling).0, dangling);
        assert!(!plan_block_malformed(dangling));
    }
}
