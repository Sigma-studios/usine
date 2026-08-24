//! The instruction appended to an investigation run's prompt (the read-only
//! "investigate only" cards — see [`crate::domain::config::CardKind`]).
//!
//! Mirrors [`crate::agent::plan`]: the run is a headless single turn whose final
//! response *is* the deliverable — here a conclusion rather than a plan. There
//! is no structured-questions block in v1; the agent is told to state open
//! decisions plainly in its conclusion, which the user answers via a follow-up
//! round.

use crate::domain::model::Provider;

/// Appended to every investigation prompt. Provider-neutral —
/// [`investigate_instruction`] adds the Claude-specific addendum.
pub const INVESTIGATE_INSTRUCTION: &str = "\
This is a READ-ONLY investigation: examine and audit the codebase, but do not create, modify, or \
delete any files, and do not run commands that change state. You are answering a question, not \
implementing anything.\n\n\
Write your complete findings out in full as your final response: what you examined, what you \
found — with `file:line` references so claims can be checked — and a clear verdict or answer to \
the question asked. Start the response with a `TL;DR:` — 2-5 short bullet points giving the \
verdict/answer first and the load-bearing findings — then the full detail. \
This runs headlessly with no follow-up turn: you cannot pause to wait for \
background work, sub-agents, or tool results and resume later. If you launch any exploration, \
finish it and fold the findings into your conclusion before you respond — never end your turn by \
saying you will wait for results.\n\n\
If something needs the user's decision, or you could not settle a question from the code alone, \
say so plainly at the end of your conclusion — the user can reply with a follow-up round.";

/// Claude-only addendum: same reason as the plan phase — a headless run has no
/// ExitPlanMode approver, and the final response is what gets captured.
const CLAUDE_NO_EXIT_PLAN_MODE: &str = "\
Do not try to call ExitPlanMode: it is not available in headless runs (it is absent from the tool \
list and from ToolSearch's deferred registry), and your final response is what gets captured \
either way.";

/// The full investigation instruction for `provider`.
pub fn investigate_instruction(provider: Provider) -> String {
    match provider {
        Provider::Claude => format!("{INVESTIGATE_INSTRUCTION}\n\n{CLAUDE_NO_EXIT_PLAN_MODE}"),
        Provider::Codex => INVESTIGATE_INSTRUCTION.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_gates_the_exit_plan_mode_note_by_provider() {
        let claude = investigate_instruction(Provider::Claude);
        let codex = investigate_instruction(Provider::Codex);
        // Both providers get the read-only + full-conclusion contract…
        assert!(claude.contains("READ-ONLY"));
        assert!(codex.contains("READ-ONLY"));
        // …both are asked to open with a TL;DR…
        assert!(claude.contains("TL;DR"));
        assert!(codex.contains("TL;DR"));
        // …but only Claude is warned off its own ExitPlanMode tool.
        assert!(claude.contains("ExitPlanMode"));
        assert!(!codex.contains("ExitPlanMode"));
    }
}
