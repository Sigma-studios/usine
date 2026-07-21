//! A write run that changes no files must not sail through to review.
//!
//! `finalize_run` commits the card's worktree, but an implement/fix run can
//! finish having written nothing — the agent answered in prose, decided no
//! change was needed, or reverted its own edits. `git add -A` then stages
//! nothing and `git commit` reports "nothing to commit". Without a guard the
//! card would advance to the review gate on an empty branch: it looks done, but
//! the diff is empty and any PR opened from it would be empty too (exactly how a
//! "ready for PR" card ended up with no changes in the diff viewer).
//!
//! The guard demotes such a run to `Failed` (a running state's escape hatch) so
//! the user sees it needs attention and can Retry, instead of a silent dead-end.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, GitOps, MergeOutcome, Project, ProjectConfig, Result, SimFactory, SimForge,
    SimGit, Store,
};

/// Git that behaves like the simulator, except a commit never lands — the tree
/// is always clean, so `commit_all` reports "nothing committed".
struct NoCommitGit;

#[async_trait]
impl GitOps for NoCommitGit {
    async fn commit_all(&self, _: &Path, _: &str) -> Result<bool> {
        Ok(false)
    }
    // Everything else defers to the simulator's always-succeeds behavior.
    async fn create_worktree(&self, r: &Path, b: &str, p: &Path, base: &str) -> Result<()> {
        SimGit.create_worktree(r, b, p, base).await
    }
    async fn remove_worktree(&self, r: &Path, p: &Path) -> Result<()> {
        SimGit.remove_worktree(r, p).await
    }
    async fn worktree_add_existing(&self, r: &Path, b: &str, p: &Path) -> Result<()> {
        SimGit.worktree_add_existing(r, b, p).await
    }
    async fn worktree_add_detached(&self, r: &Path, p: &Path, c: &str) -> Result<()> {
        SimGit.worktree_add_detached(r, p, c).await
    }
    async fn fetch_pr(&self, r: &Path, n: u64, b: &str) -> Result<()> {
        SimGit.fetch_pr(r, n, b).await
    }
    async fn reset_mixed(&self, d: &Path, g: &str) -> Result<()> {
        SimGit.reset_mixed(d, g).await
    }
    async fn rename_branch(&self, d: &Path, o: &str, n: &str) -> Result<()> {
        SimGit.rename_branch(d, o, n).await
    }
    async fn delete_branch(&self, r: &Path, b: &str) -> Result<()> {
        SimGit.delete_branch(r, b).await
    }
    async fn fetch(&self, d: &Path, remote: &str) -> Result<()> {
        SimGit.fetch(d, remote).await
    }
    async fn merge_ref(&self, d: &Path, g: &str) -> Result<MergeOutcome> {
        SimGit.merge_ref(d, g).await
    }
    async fn push(&self, d: &Path, b: &str) -> Result<()> {
        SimGit.push(d, b).await
    }
}

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
async fn an_implement_run_that_commits_nothing_is_failed_not_advanced() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-no-commit-guard"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(NoCommitGit),
    });

    handle.send(ExecutorCommand::Start { card_id });

    // The run finishes, commits nothing, and lands in Failed — never at the gate.
    let message = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => match &c.state {
            CardState::Failed { message, .. } => Some(message.clone()),
            CardState::AwaitingReview(_) => {
                panic!("an empty run reached the review gate instead of Failed")
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(
        message.contains("without changing any files"),
        "the failure explains why: {message}"
    );

    // The final stored state is Failed, and it preserves the implement run state
    // so Retry relaunches the right phase rather than stranding the card.
    let final_state = store.get_card(card_id).unwrap().state;
    match final_state {
        CardState::Failed { previous, .. } => assert!(
            matches!(*previous, CardState::Implementing(_)),
            "Failed preserves the implement run for Retry, got {previous:?}"
        ),
        other => panic!("card should be Failed, got {other:?}"),
    }
}
