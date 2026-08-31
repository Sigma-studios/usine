//! Structured questions a plan can surface for the user to answer.
//!
//! Headless `claude` has no live question tool, so instead we ask the agent to
//! append a machine-readable `usine-questions` block to its plan; [`parse_plan`]
//! splits that out so the UI can render real multiple-choice + free-form inputs,
//! and the answers are fed back to refine the plan.

use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
}

const TAG: &str = "```usine-questions";

/// Locate a complete fenced questions block: (block start, end past the
/// closing fence, the JSON between them).
fn find_block(plan: &str) -> Option<(usize, usize, &str)> {
    let start = plan.find(TAG)?;
    let after_tag = &plan[start + TAG.len()..];
    let close_rel = after_tag.find("```")?;
    let block_end = start + TAG.len() + close_rel + 3; // include the closing ```
    Some((start, block_end, after_tag[..close_rel].trim()))
}

/// Split a plan into (prose with the questions block removed, parsed questions).
/// With no complete block, returns the plan unchanged and no questions. A
/// complete block is always stripped — even when its JSON is malformed (no
/// questions then; see [`plan_block_malformed`]), so the garbage never badges
/// the plan as questioned nor leaks into the implement prompt.
pub fn parse_plan(plan: &str) -> (String, Vec<PlanQuestion>) {
    parse_questions(plan)
}

/// [`parse_plan`] under the name the non-plan callers use: the same
/// `usine-questions` block is how a conflict-resolution run asks for a
/// decision, and there is nothing plan-specific about splitting it out.
pub fn parse_questions(text: &str) -> (String, Vec<PlanQuestion>) {
    let plan = text;
    let Some((start, block_end, json)) = find_block(plan) else {
        return (plan.to_string(), Vec::new());
    };
    let questions: Vec<PlanQuestion> = serde_json::from_str(json).unwrap_or_default();
    let mut cleaned = String::new();
    cleaned.push_str(&plan[..start]);
    cleaned.push_str(&plan[block_end..]);
    (cleaned.trim().to_string(), questions)
}

/// True when the plan carries a questions block whose JSON doesn't parse — the
/// agent tried to ask something but garbled it. [`parse_plan`] strips the block
/// silently; the plan panel uses this to tell the user something was dropped.
/// A valid-but-empty `[]` is not malformed.
pub fn plan_block_malformed(plan: &str) -> bool {
    match find_block(plan) {
        Some((_, _, json)) => serde_json::from_str::<Vec<PlanQuestion>>(json).is_err(),
        None => false,
    }
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
