//! End-to-end smoke of the PR-review workflow over the simulated backends,
//! exercising the real executor wiring: scan → start → (agent) → awaiting
//! validation → publish → reviewed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, Project,
    ProjectConfig, ReviewStatus, SimFactory, SimForge, SimGit, Store,
};

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

#[tokio::test]
async fn review_lifecycle_scan_start_publish() {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        review_contributors: vec!["octocat".into()],
        ..Default::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/review-flow"), config);
    store.upsert_project(&project).unwrap();
    let project_id = project.id;

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store,
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    // A scan discovers the two canned PRs from SimForge and creates tasks.
    handle.send(ExecutorCommand::ScanReviews { project_id });
    let review_id = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { tasks, .. } if !tasks.is_empty() => {
            Some(tasks[0].id)
        }
        _ => None,
    })
    .await;

    // Starting the review runs the (sim) agent, which drafts comments and parks
    // the task for validation.
    handle.send(ExecutorCommand::StartReview {
        review_id,
        guidance: String::new(),
    });
    let (drafts, event) = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTaskUpdated(t) if t.id == review_id => match &t.status {
            ReviewStatus::AwaitingValidation { drafts, event, .. } => {
                Some((drafts.clone(), *event))
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(drafts.len(), 2, "the sim review drafts two comments");

    // Publishing submits the review and moves the task to Reviewed.
    handle.send(ExecutorCommand::PublishReview {
        review_id,
        drafts,
        event,
        body: "LGTM".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTaskUpdated(t)
            if t.id == review_id && matches!(t.status, ReviewStatus::Reviewed) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
}
