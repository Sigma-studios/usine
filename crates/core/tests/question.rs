//! End-to-end smoke of the Agent Chat "Ask questions" flow over the simulated
//! backends: a question wraps the parked state in `Answering`, runs a
//! read-only turn, and lands the card back exactly where it was asked from —
//! never tripping the no-commit guard, never touching the fixes recap, and
//! surviving a crash mid-question as a retryable question (not the parked
//! phase re-run as a write).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, DesignSub, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, ExecutorHandle, Project, ProjectConfig, ReviewSub,
    SimFactory, SimForge, SimGit, Store,
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
    // The card wraps into `Answering` while the agent answers, carrying the
    // asked-from state.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::Answering { previous, .. }
                if matches!(
                    **previous,
                    CardState::AwaitingReview(ReviewSub::ReadyForReview)
                ) =>
            {
                Some(())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    // The answer arrives, then the card lands back on ReadyForReview — a run
    // that changed no files must NOT be demoted by the no-commit guard.
    let (question, answer) = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { answers } => answers
            .exchanges
            .last()
            .map(|x| (x.question.clone(), x.answer.clone())),
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
    assert_eq!(store.get_answer(card_id).unwrap(), Some(answer.clone()));

    // A later change request supersedes the log: the write run's launch marks
    // it so the panel can't resurface, expanded, an answer about replaced work
    // — but the exchange itself is kept, readable behind its collapsed row.
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "tighten the boundary".into(),
    });
    let superseded = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { answers } if answers.superseded => {
            Some(answers.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(
        superseded.exchanges.len(),
        1,
        "a superseding write run keeps the exchange"
    );
    assert!(store.get_answers(card_id).unwrap().superseded);
    assert_eq!(store.get_answer(card_id).unwrap(), Some(answer));
}

/// Asking twice keeps both exchanges, oldest first — the panel renders the
/// whole log, not just the latest turn.
#[tokio::test]
async fn a_second_question_keeps_the_first_exchange() {
    let store = Store::open_in_memory().unwrap();
    let card_id = seed_card(&store, "/tmp/question-history");
    store.set_skip_plan(card_id, true).unwrap();
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

    for q in ["why the boundary?", "and the retry?"] {
        handle.send(ExecutorCommand::AskQuestion {
            card_id,
            question: q.into(),
        });
        wait_for(&mut rx, |e| match &e.kind {
            ExecutorEventKind::AnswersUpdated { answers }
                if answers.exchanges.last().is_some_and(|x| x.question == q) =>
            {
                Some(())
            }
            _ => None,
        })
        .await;
    }

    let log = store.get_answers(card_id).unwrap();
    let questions: Vec<_> = log.exchanges.iter().map(|x| x.question.as_str()).collect();
    assert_eq!(questions, vec!["why the boundary?", "and the retry?"]);
    assert!(
        log.exchanges.iter().all(|x| !x.answer.is_empty()),
        "every kept exchange carries its answer"
    );
    assert!(!log.superseded, "a question run does not supersede the log");

    // "Back to the starting block" is what actually discards the log.
    handle.send(ExecutorCommand::BackToStart { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { answers } if answers.exchanges.is_empty() => Some(()),
        _ => None,
    })
    .await;
    assert!(store.get_answers(card_id).unwrap().exchanges.is_empty());
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
    // While answering, the card rides `Answering` (plan intact inside); then it
    // returns to AwaitingApproval carrying the untouched plan text (questions
    // block and all).
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
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::Answering { previous, .. }
                if matches!(**previous, CardState::ReadyToMerge) =>
            {
                Some(())
            }
            _ => None,
        },
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

/// The position-loss regression the `Answering` state fixes: a question asked
/// from a pre-PR gate position other than `ReadyForReview` must return to that
/// exact position — before, it came back via `AgentImplementDone` and dropped
/// to `ReadyForReview`, losing the gate position and the validation output.
#[tokio::test]
async fn a_question_from_validation_failed_returns_to_that_exact_state() {
    let store = Store::open_in_memory().unwrap();
    let card_id = seed_card(&store, "/tmp/question-valfail");
    let parked = CardState::AwaitingReview(ReviewSub::ValidationFailed {
        attempt: 3,
        output: "test task_x failed".into(),
    });
    store
        .mutate_card(card_id, |c| {
            c.state = parked.clone();
            Ok(())
        })
        .unwrap();
    let (handle, mut rx) = spawn_with(&store);

    handle.send(ExecutorCommand::AskQuestion {
        card_id,
        question: "is the failure environmental?".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if matches!(c.state, CardState::Answering { .. }) => {
            Some(())
        }
        _ => None,
    })
    .await;
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            s if *s == parked => Some(()),
            CardState::AwaitingReview(ReviewSub::ReadyForReview) => {
                panic!("the question dropped the card back to ReadyForReview")
            }
            CardState::Failed { message, .. } => panic!("question failed: {message}"),
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(store.get_answer(card_id).unwrap().is_some());
}

/// A question run that dies mid-flight faults as `Failed { previous:
/// Answering }`, and Retry re-runs the *question* — landing back at the parked
/// state — rather than resuming the borrowed phase as a write run.
#[tokio::test]
async fn an_interrupted_question_retries_as_a_question() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use usine_core::{
        AgentProvider, CoreError, Provider, ProviderFactory, Result, RunConfig, RunHandle, RunMode,
    };

    /// Fails the next `start` when armed (a run that can't launch at all);
    /// records every launched run's `(mode, full prompt)`.
    struct FlakyProvider {
        inner: Arc<dyn AgentProvider>,
        fail_next: Arc<AtomicBool>,
        runs: Arc<Mutex<Vec<(RunMode, String)>>>,
    }
    #[async_trait::async_trait]
    impl AgentProvider for FlakyProvider {
        fn provider(&self) -> Provider {
            self.inner.provider()
        }
        fn interactive(&self) -> bool {
            self.inner.interactive()
        }
        async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Err(CoreError::other("simulated launch failure"));
            }
            self.runs
                .lock()
                .unwrap()
                .push((cfg.mode, cfg.full_prompt()));
            self.inner.start(cfg).await
        }
    }
    struct FlakyFactory {
        fail_next: Arc<AtomicBool>,
        runs: Arc<Mutex<Vec<(RunMode, String)>>>,
    }
    impl ProviderFactory for FlakyFactory {
        fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
            Arc::new(FlakyProvider {
                inner: SimFactory.make(provider),
                fail_next: self.fail_next.clone(),
                runs: self.runs.clone(),
            })
        }
    }

    let store = Store::open_in_memory().unwrap();
    let card_id = seed_card(&store, "/tmp/question-interrupted");
    store
        .mutate_card(card_id, |c| {
            c.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
            Ok(())
        })
        .unwrap();
    let fail_next = Arc::new(AtomicBool::new(true));
    let runs = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(FlakyFactory {
            fail_next: fail_next.clone(),
            runs: runs.clone(),
        }),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    // The question's run dies at launch → Failed wrapping Answering.
    handle.send(ExecutorCommand::AskQuestion {
        card_id,
        question: "why did you adapt at the boundary?".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::Failed { previous, .. } => {
                assert!(
                    matches!(**previous, CardState::Answering { .. }),
                    "the fault must wrap the question run, got {previous:?}"
                );
                Some(())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    // The failing AskQuestion still holds the card's exclusive claim when the
    // Failed echo arrives; wait for the release so Retry isn't dropped.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardBusy { busy: false } => Some(()),
        _ => None,
    })
    .await;

    // Retry re-runs the question (read-only, question in the prompt) and lands
    // the card back where it was asked from.
    handle.send(ExecutorCommand::Retry { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::ReadyForReview) => Some(()),
            CardState::Implementing(_) => panic!("retry must not resume as a write run"),
            _ => None,
        },
        _ => None,
    })
    .await;
    let runs = runs.lock().unwrap();
    assert_eq!(runs.len(), 1, "only the retried launch reached a provider");
    let (mode, prompt) = &runs[0];
    assert_eq!(*mode, RunMode::Question);
    assert!(
        prompt.contains("why did you adapt at the boundary?"),
        "the retried question rides in the rebuilt prompt"
    );
    assert!(store.get_answer(card_id).unwrap().is_some());
}
