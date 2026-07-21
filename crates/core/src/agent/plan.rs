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

/// Split a plan into (prose with the questions block removed, parsed questions).
/// If there's no valid block, returns the plan unchanged with no questions.
pub fn parse_plan(plan: &str) -> (String, Vec<PlanQuestion>) {
    const TAG: &str = "```usine-questions";

    let Some(start) = plan.find(TAG) else {
        return (plan.to_string(), Vec::new());
    };
    let after_tag = &plan[start + TAG.len()..];
    let Some(close_rel) = after_tag.find("```") else {
        return (plan.to_string(), Vec::new());
    };

    let json = after_tag[..close_rel].trim();
    let questions: Vec<PlanQuestion> = serde_json::from_str(json).unwrap_or_default();
    if questions.is_empty() {
        return (plan.to_string(), Vec::new());
    }

    let block_end = start + TAG.len() + close_rel + 3; // include the closing ```
    let mut cleaned = String::new();
    cleaned.push_str(&plan[..start]);
    cleaned.push_str(&plan[block_end..]);
    (cleaned.trim().to_string(), questions)
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
        // …but only Claude is warned off its own ExitPlanMode tool.
        assert!(claude.contains("ExitPlanMode"));
        assert!(!codex.contains("ExitPlanMode"));
    }

    #[test]
    fn malformed_block_is_ignored() {
        let plan = "Plan.\n```usine-questions\nnot json\n```";
        let (clean, qs) = parse_plan(plan);
        assert_eq!(clean, plan);
        assert!(qs.is_empty());
    }
}
