//! End-to-end smoke of the Agent Chat "Ask questions" flow over the simulated
//! backends: a question rides the existing send-back transitions, runs a
//! read-only turn, and lands the card back exactly where it was asked from —
//! never tripping the no-commit guard, never touching the fixes recap.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, DesignSub, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, ExecutorHandle, PrReviewSub, Project, ProjectConfig,
    ReviewSub, RunSub, SimFactory, SimForge, SimGit, Store,
};
use uuid::Uuid;

/// Await events until `f` returns `Some`, failing the test on timeout.
async fn wait_for<F, T>(rx: &mut UnboundedReceiver<ExecutorEvent>, mut f: F) -> T
where
    F: FnMut(&ExecutorEvent) -> Option<T>,
{
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(15), rx.next())
            .await
            .expect("timed out waiting for an executor event")
            .expect("event stream closed unexpectedly");
        if let Some(v) = f(&evt) {
            return v;
        }
    }
}

fn spawn_with(store: &Store) -> (ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

fn seed_card(store: &Store, dir: &str) -> Uuid {
    let project = Project::new("p", PathBuf::from(dir), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "d", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    card_id
}

#[tokio::test]
async fn a_question_from_awaiting_review_round_trips_with_an_answer() {
    let store = Store::open_in_memory().unwrap();
    let card_id = seed_card(&store, "/tmp/question-review");
    // Straight to implementing — the plan phase would block on a sim question.
    store.set_skip_plan(card_id, true).unwrap();
    // Park at the manual gate: with the auto self-review on, its in-flight
    // claim races the `AskQuestion` below and can silently drop it.
    store.set_auto_review(card_id, false).unwrap();
    let (handle, mut rx) = spawn_with(&store);

    handle.send(ExecutorCommand::Start { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(
                c.state,
                CardState::AwaitingReview(ReviewSub::ReadyForReview)
            ) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    handle.send(ExecutorCommand::AskQuestion {
        card_id,
        question: "  why did you adapt at the boundary?  ".into(),
    });
    // The card passes through Implementing while the agent answers.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::Implementing(RunSub::Running)) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    // The answer arrives, then the card lands back on ReadyForReview — a run
    // that changed no files must NOT be demoted by the no-commit guard.
    let (question, answer) = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswerUpdated { question, answer } => {
            Some((question.clone(), answer.clone()))
        }
        _ => None,
    })
    .await;
    assert_eq!(
        question, "why did you adapt at the boundary?",
        "the exchange carries the (trimmed) question for the panel"
    );
    assert!(!answer.is_empty(), "the question run must yield an answer");
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::ReadyForReview) => Some(()),
            CardState::Failed { message, .. } => {
                panic!("question run must not fail (no-commit guard?): {message}")
            }
            _ => None,
        },
        _ => None,
    })
    .await;

    // The *answered* exchange is on the restart log (never a bare question,
    // which a later prompt would read as a standing directive), and the answer
    // is persisted.
    let qa = store.get_card(card_id).unwrap().qa_log;
    assert_eq!(
        qa,
        vec![format!(
            "Q: why did you adapt at the boundary?\nA: {answer}"
        )]
    );
    assert_eq!(store.get_answer(card_id).unwrap(), Some(answer));

    // A later change request supersedes the exchange: the write run's launch
    // clears it so the panel can't resurface an answer about replaced work.
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "tighten the boundary".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswerUpdated { answer, .. } if answer.is_empty() => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(store.get_answer(card_id).unwrap(), None);
}

#[tokio::test]
async fn a_plan_stage_question_restores_the_identical_plan() {
    let store = Store::open_in_memory().unwrap();
    let card_id = seed_card(&store, "/tmp/question-plan");
    let (handle, mut rx) = spawn_with(&store);

    // Drive the sim plan run to its awaiting-approval park (it asks one
    // question mid-plan that must be answered first).
    handle.send(ExecutorCommand::Start { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    handle.send(ExecutorCommand::Answer {
        card_id,
        text: "Simplicity".into(),
    });
    let plan = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::Designing(DesignSub::AwaitingApproval { plan }) => Some(plan.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;

    handle.send(ExecutorCommand::AskQuestion {
        card_id,
        question: "does step 2 cover the migration?".into(),
    });
    // While answering, the card shows as designing again; then it returns to
    // AwaitingApproval carrying the untouched plan text (questions block and all).
    let restored = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::Designing(DesignSub::AwaitingApproval { plan }) => Some(plan.clone()),
            CardState::Failed { message, .. } => panic!("plan question failed: {message}"),
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(restored, plan, "the plan must round-trip verbatim");
    assert!(
        store.get_answer(card_id).unwrap().is_some(),
        "the answer is persisted"
    );
}

#[tokio::test]
async fn a_ready_to_merge_question_returns_without_touching_the_recap() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/question-merge"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    // Seed a card already parked at the merge gate. The worktree points at an
    // existing directory so `ensure_branch_worktree` is a no-op.
    let mut card = Card::new(project.id, "c", "d", CardConfig::default());
    card.state = CardState::ReadyToMerge;
    card.branch = Some("usine/c".into());
    card.worktree_path = Some(std::env::temp_dir());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store
        .set_review_recap(card_id, "fixed the two nits")
        .unwrap();
    let (handle, mut rx) = spawn_with(&store);

    handle.send(ExecutorCommand::AskQuestion {
        card_id,
        question: "is the migration reversible?".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::PrReview(PrReviewSub::ApplyingFixes)) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::ReadyToMerge => Some(()),
            CardState::Failed { message, .. } => panic!("merge-gate question failed: {message}"),
            _ => None,
        },
        _ => None,
    })
    .await;

    // The answer never masquerades as a fixes recap.
    assert_eq!(
        store.get_review_recap(card_id).unwrap().as_deref(),
        Some("fixed the two nits")
    );
    assert!(store.get_answer(card_id).unwrap().is_some());
}

#[tokio::test]
async fn a_question_from_an_illegal_state_leaves_no_trace() {
    let store = Store::open_in_memory().unwrap();
    let card_id = seed_card(&store, "/tmp/question-illegal");
    let (handle, mut rx) = spawn_with(&store);

    // Questions can't be asked from the starting block — the command must be
    // refused without recording anything (the refusal surfaces as a toast).
    handle.send(ExecutorCommand::AskQuestion {
        card_id,
        question: "too early?".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast { .. } => Some(()),
        _ => None,
    })
    .await;

    assert!(store.get_card(card_id).unwrap().qa_log.is_empty());
    assert_eq!(store.get_question(card_id).unwrap(), None);
}
