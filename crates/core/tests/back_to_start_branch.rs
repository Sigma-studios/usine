//! Back to Start must delete the discarded attempt's git branch, not just its
//! worktree.
//!
//! The branch name is derived deterministically from the card (title + id), so a
//! leftover branch collides with the next start's `git worktree add -b
//! <same-name>` — failing with "a branch named … already exists" right at plan
//! approval. Back to Start clears the card's branch *pointer*; it must also tear
//! down the actual branch so the next start can recreate it cleanly.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, GitOps, MergeOutcome, Project, ProjectConfig, Result, ReviewSub, SimFactory,
    SimForge, SimGit, Store,
};

/// Simulator git that records the branches it was asked to delete.
struct SpyGit {
    deleted: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl GitOps for SpyGit {
    async fn delete_branch(&self, r: &Path, branch: &str) -> Result<()> {
        self.deleted.lock().unwrap().push(branch.to_string());
        SimGit.delete_branch(r, branch).await
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
    async fn fetch(&self, d: &Path, remote: &str) -> Result<()> {
        SimGit.fetch(d, remote).await
    }
    async fn merge_ref(&self, d: &Path, g: &str) -> Result<MergeOutcome> {
        SimGit.merge_ref(d, g).await
    }
    async fn commit_all(&self, d: &Path, m: &str) -> Result<bool> {
        SimGit.commit_all(d, m).await
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
async fn back_to_start_deletes_the_discarded_branch() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-back-to-start-branch"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let deleted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SpyGit {
            deleted: deleted.clone(),
        }),
    });

    // Run to the gate — this is where the card has acquired its branch.
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
    let branch = store
        .get_card(card_id)
        .unwrap()
        .branch
        .expect("the card acquired a branch on its way to the gate");

    // Back to Start must tear that branch down, not just null the pointer.
    handle.send(ExecutorCommand::BackToStart { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if matches!(c.state, CardState::StartingBlock) => {
            Some(())
        }
        _ => None,
    })
    .await;

    assert!(
        deleted.lock().unwrap().contains(&branch),
        "Back to Start should delete branch {branch:?}; deleted: {:?}",
        deleted.lock().unwrap()
    );
    // And the card's pointer is cleared, so the next start recreates it fresh.
    assert!(
        store.get_card(card_id).unwrap().branch.is_none(),
        "the card's branch pointer is cleared"
    );
}
