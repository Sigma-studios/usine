//! The canned sim payloads parse into the exact structures the panels render.
//!
//! Demo mode (`USINE_SIM=1`) is how the structured-artifact UI is exercised
//! without burning real runs, so the sim provider and the parsers have to stay
//! in lockstep. A sim payload that silently stopped parsing would leave the
//! panels blank in the one mode built to show them off — and nothing else in
//! the suite would notice, because every other test supplies its own fixtures.
//!
//! So this drives the *real* provider and feeds its *real* output through the
//! *real* parsers, asserting each tab has something to draw.

use futures::StreamExt;
use std::path::PathBuf;
use usine_core::{
    AgentEvent, Effort, ModelSpec, Provider, ProviderFactory, RunConfig, RunControl, RunMode,
    SimFactory,
};
use uuid::Uuid;

/// Run the sim provider in `mode` and return its final result text.
async fn sim_result(mode: RunMode) -> String {
    let provider = SimFactory.make(Provider::Claude);
    let mut handle = provider
        .start(RunConfig {
            provider: Provider::Claude,
            project_dir: PathBuf::from("/tmp/sim-artifacts"),
            spec: ModelSpec::new("opus", Effort::Medium),
            mode,
            session_id: Uuid::new_v4(),
            prompt: "Do the thing.".into(),
            extra_prompt: None,
            resume_session: None,
            attachments: Vec::new(),
        })
        .await
        .expect("the sim provider always starts");

    let mut result = String::new();
    while let Some(evt) = handle.events.next().await {
        match evt {
            AgentEvent::Done { result: r, .. } => {
                result = r;
                break;
            }
            // A plan run reports through PlanReady rather than Done.
            AgentEvent::PlanReady { plan } => {
                result = plan;
                break;
            }
            // The sim is interactive: its plan run blocks on a question until
            // something answers it, exactly as the executor does.
            AgentEvent::NeedsInput { .. } => {
                handle
                    .control
                    .unbounded_send(RunControl::Answer {
                        text: "Simplicity".into(),
                    })
                    .expect("the sim's control channel is open");
            }
            _ => {}
        }
    }
    assert!(
        !result.is_empty(),
        "the sim produced no result for {mode:?}"
    );
    result
}

#[tokio::test]
async fn the_sim_hand_off_fills_every_tab() {
    let text = sim_result(RunMode::Implement).await;
    let h = usine_core::parse_handoff(&text).expect("the sim hand-off parses");

    assert_eq!(h.v, 2, "the sim emits the current schema");
    assert!(h.summary.starts_with("TL;DR:"), "Summary tab");
    assert!(h.changes.len() >= 2, "Changes tab: {:?}", h.changes);
    assert!(
        h.changes
            .iter()
            .all(|c| !c.path.is_empty() && !c.what.is_empty()),
        "every change row has a path and a description"
    );
    assert!(h.tests.len() >= 2, "Test it tab");
    assert!(
        h.tests.iter().any(|t| t.verified),
        "one scenario is marked verified, so the badge is exercised"
    );
    assert!(
        h.tests
            .iter()
            .all(|t| !t.scenario.is_empty() && !t.expect.is_empty()),
        "every checklist row has both halves"
    );
    assert!(!h.risks.is_empty(), "Risks tab");
    assert!(!h.questions.is_empty(), "Risks tab's open-questions list");
    assert!(!h.malformed);

    // The block is machine-facing: the transcript must not show it.
    let stripped = usine_core::strip_handoff_block(&text);
    assert!(!stripped.contains("usine-handoff"));
    assert!(!stripped.is_empty(), "prose survives the strip");
}

#[tokio::test]
async fn the_sim_plan_fills_every_tab_and_keeps_its_prose() {
    let text = sim_result(RunMode::Plan).await;

    // Prose (the implement payload) survives with both blocks removed.
    let (prose, questions) = usine_core::parse_plan(&text);
    assert!(prose.contains("Plan"), "the Plan tab's prose: {prose}");
    assert!(!prose.contains("usine-plan"), "outline block stripped");
    assert!(
        !prose.contains("usine-questions"),
        "questions block stripped"
    );
    assert_eq!(questions.len(), 2, "the sim asks two questions");
    assert!(!usine_core::plan_block_malformed(&text));
    assert!(!usine_core::plan_outline_malformed(&text));

    let outline = usine_core::parse_plan_outline(&text).expect("the sim outline parses");
    assert!(!outline.tldr.is_empty(), "Plan tab's TL;DR bullets");
    assert!(outline.steps.len() >= 3, "Steps tab");
    assert!(
        outline.steps.iter().all(|s| !s.title.is_empty()),
        "every step has a title"
    );
    assert!(outline.files.len() >= 3, "Files tab");
    assert!(!outline.verification.is_empty(), "Verify tab");
    assert!(!outline.risks.is_empty(), "Verify tab's risks");
}

#[tokio::test]
async fn the_sim_conclusion_fills_its_findings_and_asks_real_questions() {
    let text = sim_result(RunMode::Investigate).await;

    let prose = usine_core::conclusion_prose(&text);
    assert!(prose.contains("Verdict"), "the Conclusion tab: {prose}");
    assert!(!prose.contains("usine-findings"));
    assert!(!prose.contains("usine-questions"));
    assert!(!usine_core::findings_malformed(&text));

    let f = usine_core::parse_findings(&text).expect("the sim findings parse");
    assert!(!f.verdict.is_empty(), "the Findings tab's verdict line");
    assert!(f.findings.len() >= 2, "Findings tab rows");
    assert!(
        f.findings
            .iter()
            .all(|x| !x.claim.is_empty() && !x.evidence.is_empty()),
        "every finding carries a claim and at least one file:line chip"
    );
    assert!(
        f.findings
            .iter()
            .flat_map(|x| &x.evidence)
            .all(|e| !e.path.is_empty() && e.line.is_some()),
        "evidence chips are clickable file:line pairs"
    );
    assert!(!f.open_questions.is_empty(), "the still-open list");

    // An investigation can now ask structured questions, not just prose.
    let (_, questions) = usine_core::parse_questions(&text);
    assert_eq!(questions.len(), 1, "the sim conclusion asks one question");
    assert!(!questions[0].options.is_empty(), "with pickable options");
}

#[tokio::test]
async fn the_sim_fix_run_reports_outcomes_against_the_picker() {
    let text = sim_result(RunMode::ApplyFixes).await;

    let outcomes = usine_core::parse_fix_outcomes(&text).expect("the sim fix outcomes parse");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(
        outcomes[0].id, 0,
        "id 0 is the first sim self-review finding, checked by default"
    );
    assert!(!outcomes[0].note.is_empty(), "the note explains 'partial'");
    assert!(!usine_core::fixes_block_malformed(&text));

    // The prose recap survives with the block removed.
    let recap = usine_core::strip_fixes_block(&text);
    assert!(recap.starts_with("TL;DR:"), "recap: {recap}");
    assert!(!recap.contains("usine-fixes"));
}
