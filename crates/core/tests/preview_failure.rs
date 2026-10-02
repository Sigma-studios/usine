//! A failing preview explains itself: the setup script's (or the app's) exit
//! is reported with the command and the tail of what it printed — in the
//! `Failed` status, the toast, and the persisted transcript — instead of a
//! bare exit status.
//!
//! The preview is a real `sh` process in a real temp worktree — only the
//! agents, git, and the forge are simulated.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, PreviewStatus, Project, ProjectConfig, ReviewSub, Severity, SimFactory,
    SimForge, SimGit, Store,
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

/// Wait for the card's preview to report `Failed`, returning its reason.
async fn wait_for_failure(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
) -> String {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated {
            status: PreviewStatus::Failed(reason),
            ..
        } if e.card_id == card_id => Some(reason.clone()),
        _ => None,
    })
    .await
}

/// A store with one project and one parked card whose REAL worktree dir the
/// preview runs in (`SimGit` is a no-op, so the pre-seeded dir is what keeps
/// the launch on a real path).
fn seed(config: ProjectConfig, worktree: &std::path::Path) -> (Store, uuid::Uuid) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/usine-preview-failure-p"), config);
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

fn persisted(store: &Store, card_id: uuid::Uuid) -> String {
    store.load_transcript(card_id).unwrap().join("\n")
}

#[tokio::test]
async fn failing_setup_quotes_its_output() {
    let wt = tempfile::tempdir().unwrap();
    let setup = "echo step one; echo 'db: connection refused' >&2; exit 3";
    let (store, card_id) = seed(
        ProjectConfig {
            worktree_setup_script: Some(setup.into()),
            run_script: Some("while true; do sleep 1; done".into()),
            ..ProjectConfig::default()
        },
        wt.path(),
    );
    let (handle, mut rx) = executor(&store);
    handle.send(ExecutorCommand::StartPreview { card_id });

    let reason = wait_for_failure(&mut rx, card_id).await;
    for needle in [
        setup,
        "exit status: 3",
        "step one",
        "db: connection refused",
    ] {
        assert!(
            reason.contains(needle),
            "reason should contain {needle:?}: {reason}"
        );
    }
    let toast = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } if e.card_id == card_id => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(toast.contains("db: connection refused"), "toast: {toast}");
    // The live setup stream isn't persisted; the failure line is.
    let saved = persisted(&store, card_id);
    assert!(saved.contains("✕ Preview failed"), "transcript: {saved}");
    assert!(
        saved.contains("db: connection refused"),
        "transcript: {saved}"
    );
}

#[tokio::test]
async fn silent_setup_failure_suggests_tracing() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        ProjectConfig {
            worktree_setup_script: Some("exit 1".into()),
            run_script: Some("while true; do sleep 1; done".into()),
            ..ProjectConfig::default()
        },
        wt.path(),
    );
    let (handle, mut rx) = executor(&store);
    handle.send(ExecutorCommand::StartPreview { card_id });

    let reason = wait_for_failure(&mut rx, card_id).await;
    assert!(reason.contains("printed nothing"), "{reason}");
    assert!(reason.contains("set -x"), "{reason}");
}

#[tokio::test]
async fn crashing_app_quotes_its_output() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        ProjectConfig {
            run_script: Some("echo 'EADDRINUSE :3000' >&2; exit 1".into()),
            ..ProjectConfig::default()
        },
        wt.path(),
    );
    let (handle, mut rx) = executor(&store);
    handle.send(ExecutorCommand::StartPreview { card_id });

    // `Running` goes out before the exit watcher starts, so it can't land
    // after — and overwrite — the `Failed`.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated {
            status: PreviewStatus::Running,
            ..
        } if e.card_id == card_id => Some(()),
        ExecutorEventKind::PreviewUpdated {
            status: PreviewStatus::Failed(r),
            ..
        } if e.card_id == card_id => panic!("Failed arrived before Running: {r}"),
        _ => None,
    })
    .await;
    let reason = wait_for_failure(&mut rx, card_id).await;
    assert!(reason.starts_with("The app `"), "{reason}");
    assert!(reason.contains("EADDRINUSE :3000"), "{reason}");
    assert!(
        persisted(&store, card_id).contains("EADDRINUSE :3000"),
        "the app's failure should be persisted"
    );
}
