//! The pre-PR validation gate, driven through the real executor: the project's
//! validate command runs in the card's worktree on the way to `ReadyForPr`, a
//! failure loops through an (simulated) agent fix run and re-runs the check,
//! exhaustion parks the card with the output, and cancel/skip/retry all behave.
//!
//! The check is a real `sh` process in a real temp worktree — only the agents,
//! git, and the forge are simulated.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    reconcile_interrupted_runs, spawn_executor, Card, CardConfig, CardState, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Project, ProjectConfig, ReviewSub,
    SimFactory, SimForge, SimGit, Store, MAX_VALIDATION_ATTEMPTS,
};

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

/// Wait for the card to reach a specific `ReviewSub`, panicking on `Failed`.
async fn wait_for_review_sub<F>(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
    mut want: F,
) -> ReviewSub
where
    F: FnMut(&ReviewSub) -> bool,
{
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => match &c.state {
            CardState::AwaitingReview(sub) if want(sub) => Some(sub.clone()),
            CardState::Failed { message, .. } => {
                panic!("card failed instead of advancing: {message}")
            }
            _ => None,
        },
        _ => None,
    })
    .await
}

/// A store with one project (carrying `validate_script`) and one card parked at
/// `ReadyForReview` with a branch and a REAL worktree dir the check runs in.
fn seed(validate_script: Option<&str>, worktree: &std::path::Path) -> (Store, uuid::Uuid) {
    seed_with(
        ProjectConfig {
            validate_script: validate_script.map(str::to_string),
            ..ProjectConfig::default()
        },
        worktree,
    )
}

/// [`seed`] over a fully-specified project config, for the cases that also need
/// a setup command or a custom gate timeout.
fn seed_with(config: ProjectConfig, worktree: &std::path::Path) -> (Store, uuid::Uuid) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/usine-validation-gate-p"), config);
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
    card.branch = Some("usine/card-x".into());
    card.worktree_path = Some(worktree.to_path_buf());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    (store, card_id)
}

fn executor(store: &Store) -> (usine_core::ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

#[tokio::test]
async fn failing_check_loops_through_a_fix_run_then_passes() {
    let wt = tempfile::tempdir().unwrap();
    // Deterministically fails the first run, passes the second — the marker
    // file stands in for "the fix run repaired the code".
    let script = "if [ -f ran ]; then exit 0; else touch ran; echo FAIL-MARKER; exit 1; fi";
    let (store, card_id) = seed(Some(script), wt.path());
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::SkipReview { card_id });

    wait_for_review_sub(&mut rx, card_id, |s| {
        matches!(s, ReviewSub::Validating { attempt: 1 })
    })
    .await;
    // The failure carries the captured output into the fix state.
    let sub = wait_for_review_sub(&mut rx, card_id, |s| {
        matches!(s, ReviewSub::FixingValidation { attempt: 1, .. })
    })
    .await;
    if let ReviewSub::FixingValidation { output, .. } = sub {
        assert!(output.contains("FAIL-MARKER"), "captured output: {output}");
    }
    // The (simulated) fix run commits and the check re-runs…
    wait_for_review_sub(&mut rx, card_id, |s| {
        matches!(s, ReviewSub::Validating { attempt: 2 })
    })
    .await;
    // …and passes this time.
    wait_for_review_sub(&mut rx, card_id, |s| matches!(s, ReviewSub::ReadyForPr)).await;
}

#[tokio::test]
async fn exhausted_budget_parks_then_skip_opens_the_pr() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(Some("echo NOPE; exit 1"), wt.path());
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::SkipReview { card_id });

    // Three failing attempts (with a fix run between each) exhaust the budget.
    let diagnose = |store: &Store, stage: &str| -> ! {
        let card = store.get_card(card_id).unwrap();
        let transcript = store.load_transcript(card_id).unwrap_or_default();
        panic!(
            "stalled at {stage}; card state: {:?}; transcript: {:#?}",
            card.state, transcript
        );
    };
    let park = tokio::time::timeout(
        Duration::from_secs(12),
        wait_for_review_sub(&mut rx, card_id, |s| {
            matches!(s, ReviewSub::ValidationFailed { .. })
        }),
    )
    .await;
    let Ok(sub) = park else {
        diagnose(&store, "park")
    };
    if let ReviewSub::ValidationFailed { attempt, output } = sub {
        assert_eq!(attempt, MAX_VALIDATION_ATTEMPTS);
        assert!(output.contains("NOPE"), "parked output: {output}");
    }

    // "Create PR anyway" clears the gate; the PR then opens normally.
    handle.send(ExecutorCommand::SkipValidation { card_id });
    if tokio::time::timeout(
        Duration::from_secs(12),
        wait_for_review_sub(&mut rx, card_id, |s| matches!(s, ReviewSub::ReadyForPr)),
    )
    .await
    .is_err()
    {
        diagnose(&store, "skip-validation");
    }
    // Wait for SkipValidation's in-flight claim to release (its CardBusy=false)
    // before the next exclusive command: a human can't click faster than the
    // handler's tail, but this test can, and a claimed card drops the command.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardBusy { busy: false } if e.card_id == card_id => Some(()),
        _ => None,
    })
    .await;
    handle.send(ExecutorCommand::CreatePr {
        card_id,
        branch: "feat/thing".into(),
        title: "t".into(),
        body: "b".into(),
        reviewer: None,
        draft: false,
    });
    if tokio::time::timeout(
        Duration::from_secs(12),
        wait_for(&mut rx, |e| match &e.kind {
            ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
                matches!(c.state, CardState::PrReview(_)).then_some(())
            }
            _ => None,
        }),
    )
    .await
    .is_err()
    {
        diagnose(&store, "create-pr");
    }
}

#[tokio::test]
async fn no_validate_command_lands_ready_for_pr_directly() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(None, wt.path());
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::SkipReview { card_id });

    // The very first state after ReadyForReview is ReadyForPr — today's
    // behavior, byte for byte, when no validate command is configured.
    let sub = wait_for_review_sub(&mut rx, card_id, |s| {
        !matches!(s, ReviewSub::ReadyForReview)
    })
    .await;
    assert!(matches!(sub, ReviewSub::ReadyForPr), "got {sub:?}");
}

#[tokio::test]
async fn cancelling_the_check_skips_the_gate() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(Some("sleep 60"), wt.path());
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::SkipReview { card_id });
    wait_for_review_sub(&mut rx, card_id, |s| {
        matches!(s, ReviewSub::Validating { .. })
    })
    .await;

    handle.send(ExecutorCommand::Cancel { card_id });
    wait_for_review_sub(&mut rx, card_id, |s| matches!(s, ReviewSub::ReadyForPr)).await;
}

#[tokio::test]
async fn interrupted_check_resumes_at_the_same_attempt() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(Some("exit 0"), wt.path());
    // The app crashed mid-check at attempt 2…
    store
        .mutate_card(card_id, |c| {
            c.state = CardState::AwaitingReview(ReviewSub::Validating { attempt: 2 });
            Ok(())
        })
        .unwrap();
    // …so startup reconciliation parks it as interrupted (Failed).
    assert_eq!(
        reconcile_interrupted_runs(&store, "Interrupted").unwrap(),
        1
    );

    let (handle, mut rx) = executor(&store);
    handle.send(ExecutorCommand::Retry { card_id });

    // Retry restores Validating{2} and re-runs the check, which passes.
    wait_for_review_sub(&mut rx, card_id, |s| {
        matches!(s, ReviewSub::Validating { attempt: 2 })
    })
    .await;
    wait_for_review_sub(&mut rx, card_id, |s| matches!(s, ReviewSub::ReadyForPr)).await;
}

/// The gate prepares the worktree before it judges it: the setup command runs
/// first, so a check that depends on what setup installs passes instead of
/// failing on a missing environment.
#[tokio::test]
async fn setup_command_runs_before_the_check() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed_with(
        ProjectConfig {
            worktree_setup_script: Some("echo ready > deps".into()),
            // Fails unless setup already put `deps` in the worktree.
            validate_script: Some("test -f deps".into()),
            ..ProjectConfig::default()
        },
        wt.path(),
    );
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::SkipReview { card_id });

    wait_for_review_sub(&mut rx, card_id, |s| {
        matches!(s, ReviewSub::Validating { attempt: 1 })
    })
    .await;
    // Passing on the first attempt is the whole assertion: the check saw the
    // setup command's output, and no fix run was needed to get there.
    wait_for_review_sub(&mut rx, card_id, |s| matches!(s, ReviewSub::ReadyForPr)).await;
    assert!(wt.path().join("deps").exists(), "setup didn't run");
}

/// A setup command that fails is a worktree that couldn't be prepared, not a
/// verdict on the work: the card parks at `Failed` (retryable) with the output,
/// and no fix attempt is spent on something the code can't repair.
#[tokio::test]
async fn failing_setup_parks_the_card_without_running_the_check() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed_with(
        ProjectConfig {
            worktree_setup_script: Some("echo SETUP-BROKE; exit 1".into()),
            validate_script: Some("touch check-ran".into()),
            ..ProjectConfig::default()
        },
        wt.path(),
    );
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::SkipReview { card_id });

    let message = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => match &c.state {
            CardState::Failed { message, .. } => Some(message.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(
        message.contains("SETUP-BROKE"),
        "failure message: {message}"
    );
    assert!(
        !wt.path().join("check-ran").exists(),
        "the check ran despite setup failing"
    );
}
