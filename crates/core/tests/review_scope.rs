//! Who can be reviewed. Two things the collaborator-only picker got wrong:
//! a contributor whose PR comes from a fork is never a collaborator, so the
//! suggestions have to come from the repo's open PRs as well; and tracking
//! *everyone* has to work with the pinned contributor list left empty.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, Forge,
    Project, ProjectConfig, SimFactory, SimForge, SimGit, Store,
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

fn spawn(
    config: ProjectConfig,
) -> (
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
    uuid::Uuid,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/review-scope"), config);
    store.upsert_project(&project).unwrap();
    let project_id = project.id;
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store,
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    (handle, rx, project_id)
}

/// The whole point of the picker change: the suggestions must include someone
/// the collaborator list can't reach.
#[tokio::test]
async fn pr_authors_offer_a_non_collaborator() {
    let (handle, mut rx, project_id) = spawn(ProjectConfig::default());

    handle.send(ExecutorCommand::ListPrAuthors { project_id });
    let logins = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::PrAuthors {
            project_id: p,
            logins,
        } if *p == project_id => Some(logins.clone()),
        _ => None,
    })
    .await;

    let collaborators = SimForge
        .list_reviewers(&PathBuf::from("/tmp/review-scope"))
        .await
        .unwrap();
    assert!(
        logins.iter().any(|l| !collaborators.contains(l)),
        "PR authors {logins:?} added nothing over collaborators {collaborators:?}"
    );
}

/// Tracking everyone has to poll with an empty `review_contributors` — the old
/// `is_empty()` gate would have skipped the scan entirely.
#[tokio::test]
async fn all_contributors_scans_without_a_pinned_list() {
    let (handle, mut rx, project_id) = spawn(ProjectConfig {
        review_all_contributors: true,
        review_contributors: Vec::new(),
        ..Default::default()
    });

    handle.send(ExecutorCommand::ScanReviews { project_id });
    let authors = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { tasks, .. } if !tasks.is_empty() => {
            Some(tasks.iter().map(|t| t.author.clone()).collect::<Vec<_>>())
        }
        _ => None,
    })
    .await;

    assert!(
        authors.iter().any(|a| a == "outside-contributor"),
        "everyone mode missed the fork contributor: {authors:?}"
    );
}

/// Two scans fired back to back — what happens when the "scan now" of a freshly
/// added contributor lands on top of the 5-minute poll. Each decides what to
/// create from a task listing read before its own forge round trip, so without
/// serialization both see the same PR as untracked and create a task apiece.
#[tokio::test]
async fn overlapping_scans_create_one_task_per_pr() {
    let (handle, mut rx, project_id) = spawn(ProjectConfig {
        review_all_contributors: true,
        ..Default::default()
    });

    handle.send(ExecutorCommand::ScanReviews { project_id });
    handle.send(ExecutorCommand::ScanReviews { project_id });

    // Both scans emit the project's full task list; the second one's is the
    // settled state, so wait until two updates have gone by.
    let mut seen = 0;
    let tasks = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { tasks, .. } => {
            seen += 1;
            (seen == 2).then(|| tasks.clone())
        }
        _ => None,
    })
    .await;

    let mut numbers: Vec<u64> = tasks.iter().map(|t| t.pr_number).collect();
    let total = numbers.len();
    numbers.sort_unstable();
    numbers.dedup();
    assert_eq!(numbers.len(), total, "duplicate review tasks: {tasks:?}");
    assert!(!numbers.is_empty(), "the scan found no PRs to dedupe");
}
