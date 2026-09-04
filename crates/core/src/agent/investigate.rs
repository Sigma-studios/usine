//! The instruction appended to an investigation run's prompt (the read-only
//! "investigate only" cards — see [`crate::domain::config::CardKind`]).
//!
//! Mirrors [`crate::agent::plan`]: the run is a headless single turn whose final
//! response *is* the deliverable — here a conclusion rather than a plan.
//!
//! Two machine-readable blocks ride along with the prose. ` ```usine-findings `
//! carries the verdict and the individual claims with their `file:line`
//! evidence, so the panel can show checkable rows instead of asking the reader
//! to hunt for references in a wall of text. ` ```usine-questions ` is the same
//! block the plan phase uses ([`crate::agent::plan::parse_questions`] is
//! deliberately phase-neutral), so an investigation that could not settle
//! something from the code alone asks it as real options rather than as a
//! sentence buried at the end.
//!
//! Both are views: the prose conclusion stays the payload. It is what a
//! follow-up round quotes back and what "turn into implementation" folds into
//! the description, so [`conclusion_prose`] strips the blocks at exactly those
//! points and a garbled block costs presentation, never work.

use serde::{Deserialize, Serialize};

use crate::agent::block;
use crate::domain::model::Provider;

const TAG: &str = "usine-findings";

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
end your response with a fenced code block tagged `usine-questions` containing a JSON array of \
questions, each shaped like {\"question\": \"...\", \"options\": [\"Option A\", \"Option B\"]} — 2-4 \
short options each (the user can also type their own answer). Omit the block if you have none; do \
NOT manufacture questions to fill it.\n\n\
Also append a fenced code block tagged `usine-findings` summarising what you just wrote, as a JSON \
object shaped like {\"v\": 1, \"verdict\": \"<the one-line answer>\", \"findings\": [{\"claim\": \
\"<what you found, one line>\", \"evidence\": [{\"path\": \"<file>\", \"line\": 12}], \
\"confidence\": \"high|medium|low\"}], \"open_questions\": [\"<what is still unsettled>\"]}. It is a \
view of the conclusion for the panel, not a replacement: say nothing there that is not already \
above, and cite the `file:line` you actually read.";

/// One `file:line` a claim rests on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
}

/// One thing the investigation found, with the places it was read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    #[serde(default)]
    pub claim: String,
    #[serde(default)]
    pub evidence: Vec<Evidence>,
    /// `high` | `medium` | `low`, or empty when unrated. Free-form: an agent
    /// that invents a word gets it shown as-is rather than dropped.
    #[serde(default)]
    pub confidence: String,
}

/// The investigation's own structured view of its conclusion.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Findings {
    #[serde(default)]
    pub v: u8,
    /// The one-line answer to the question that was asked.
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// What is still unsettled. Distinct from the `usine-questions` block: this
    /// is narrative, that one is answerable.
    #[serde(default)]
    pub open_questions: Vec<String>,
}

impl Findings {
    pub fn is_empty(&self) -> bool {
        self.verdict.trim().is_empty() && self.findings.is_empty() && self.open_questions.is_empty()
    }
}

/// The agent's structured findings, when it emitted them. `None` when the block
/// is missing, garbled, or vacuous.
pub fn parse_findings(text: &str) -> Option<Findings> {
    let findings = block::parse::<Findings>(text, TAG).0.ok()?;
    (!findings.is_empty()).then_some(findings)
}

/// True when the conclusion carries a `usine-findings` block whose JSON doesn't
/// parse.
pub fn findings_malformed(text: &str) -> bool {
    block::parse::<Findings>(text, TAG).0.is_malformed()
}

/// The conclusion as prose: both machine-facing blocks removed. This is what a
/// follow-up round quotes back and what "turn into implementation" folds into
/// the task description — neither should carry a payload addressed to the
/// panel.
pub fn conclusion_prose(text: &str) -> String {
    block::strip(&crate::agent::plan::parse_questions(text).0, TAG)
}

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
        // …and both are given both block contracts.
        for text in [&claude, &codex] {
            assert!(text.contains("usine-findings"));
            assert!(text.contains("usine-questions"));
        }
    }

    #[test]
    fn findings_parse_and_never_reach_a_follow_up_prompt() {
        let text = "TL;DR: the cache never expires.\n\n```usine-findings\n{\"v\":1,\
            \"verdict\":\"The cache never expires.\",\
            \"findings\":[{\"claim\":\"No TTL is applied on read\",\
            \"evidence\":[{\"path\":\"src/cache.rs\",\"line\":48}],\"confidence\":\"high\"}],\
            \"open_questions\":[\"How long should entries live?\"]}\n```\n\n\
            ```usine-questions\n[{\"question\":\"TTL?\",\"options\":[\"7 days\",\"1 hour\"]}]\n```";
        let f = parse_findings(text).unwrap();
        assert_eq!(f.verdict, "The cache never expires.");
        assert_eq!(f.findings[0].evidence[0].line, Some(48));
        assert_eq!(f.open_questions.len(), 1);
        // Both blocks are stripped from the prose the prompts quote back.
        let prose = conclusion_prose(text);
        assert_eq!(prose, "TL;DR: the cache never expires.");
        // And the questions block is still readable by the shared parser.
        assert_eq!(crate::agent::plan::parse_questions(text).1.len(), 1);
    }

    #[test]
    fn a_garbled_or_absent_block_leaves_the_conclusion_alone() {
        assert_eq!(parse_findings("just prose"), None);
        assert!(!findings_malformed("just prose"));
        let text = "Conclusion.\n```usine-findings\nnot json\n```";
        assert_eq!(parse_findings(text), None);
        assert!(findings_malformed(text));
        assert_eq!(conclusion_prose(text), "Conclusion.");
        // Present but vacuous: nothing to render.
        assert_eq!(parse_findings("```usine-findings\n{\"v\":1}\n```"), None);
    }
}
