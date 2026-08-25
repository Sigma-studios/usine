//! The merge gate on a PR's mergeability. The board hides the merge button
//! behind a "Resolve conflicts" offer when the card's cached `mergeable` says
//! the PR conflicts — so the cache must land via the background poll, be
//! persisted the moment a merge is refused on a conflict (not a poll tick
//! later), and be reset once a resolve run pushes (the merge commit stales it).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, CoreError, DraftComment, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Forge, GitOps, MergeOutcome, Mergeable,
    PrInfo, PrSummary, Project, ProjectConfig, ReviewComment, ReviewEvent, ReviewScope,
    ReviewSummary, Severity, SimFactory, SimForge, SimGit, Store,
};

/// A forge reporting the given mergeability, whose merge (optionally) fails —
/// standing in for GitHub refusing to merge a conflicting PR. Everything else
/// defers to the simulator.
struct GateForge {
    mergeable: Mergeable,
    merge_fails: bool,
}

#[async_trait]
impl Forge for GateForge {
    async fn merge(&self, _: &Path, _: u64) -> usine_core::Result<()> {
        if self.merge_fails {
            Err(CoreError::forge("gh pr merge 7 --squash failed"))
        } else {
            Ok(())
        }
    }
    async fn is_merged(&self, _: &Path, _: u64) -> usine_core::Result<bool> {
        Ok(false)
    }
    async fn merge_status(&self, _: &Path, _: u64) -> usine_core::Result<Mergeable> {
        Ok(self.mergeable)
    }
    async fn delete_remote_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    // Unreached by the gate; defer to the simulator.
    async fn create_pr(
        &self,
        r: &Path,
        t: &str,
        b: &str,
        base: &str,
        h: &str,
        rev: Option<&str>,
        d: bool,
    ) -> usine_core::Result<PrInfo> {
        SimForge.create_pr(r, t, b, base, h, rev, d).await
    }
    async fn fetch_comments(&self, r: &Path, n: u64) -> usine_core::Result<Vec<ReviewComment>> {
        SimForge.fetch_comments(r, n).await
    }
    async fn list_review_prs(
        &self,
        r: &Path,
        s: ReviewScope,
    ) -> usine_core::Result<Vec<PrSummary>> {
        SimForge.list_review_prs(r, s).await
    }
    async fn submit_review(
        &self,
        r: &Path,
        n: u64,
        e: ReviewEvent,
        b: &str,
        c: &[DraftComment],
    ) -> usine_core::Result<()> {
        SimForge.submit_review(r, n, e, b, c).await
    }
    async fn list_reviewers(&self, r: &Path) -> usine_core::Result<Vec<String>> {
        SimForge.list_reviewers(r).await
    }
    async fn list_submitted_reviews(
        &self,
        r: &Path,
        n: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        SimForge.list_submitted_reviews(r, n).await
    }
    async fn reply_to_comment(&self, r: &Path, n: u64, c: u64, b: &str) -> usine_core::Result<()> {
        SimForge.reply_to_comment(r, n, c, b).await
    }
    async fn mark_ready(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.mark_ready(r, n).await
    }
    async fn resolve_threads(&self, r: &Path, n: u64, c: &[u64]) -> usine_core::Result<usize> {
        SimForge.resolve_threads(r, n, c).await
    }
    async fn list_threads(
        &self,
        r: &Path,
        n: u64,
    ) -> usine_core::Result<Vec<usine_core::ReviewThread>> {
        SimForge.list_threads(r, n).await
    }
}

/// Git whose merge always stops on a conflict (the base has moved).
struct ConflictingGit;

#[async_trait]
impl GitOps for ConflictingGit {
    async fn merge_ref(&self, _: &Path, _: &str) -> usine_core::Result<MergeOutcome> {
        Ok(MergeOutcome::Conflicted(vec!["src/lib.rs".into()]))
    }
    async fn fetch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn create_worktree(
        &self,
        _: &Path,
        _: &str,
        _: &Path,
        _: &str,
    ) -> usine_core::Result<()> {
        Ok(())
    }
    async fn remove_worktree(&self, _: &Path, _: &Path) -> usine_core::Result<()> {
        Ok(())
    }
    async fn worktree_add_existing(&self, _: &Path, _: &str, _: &Path) -> usine_core::Result<()> {
        Ok(())
    }
    async fn worktree_add_detached(&self, _: &Path, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn fetch_pr(&self, _: &Path, _: u64, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn reset_mixed(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn rename_branch(&self, _: &Path, _: &str, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn delete_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn commit_all(&self, _: &Path, _: &str) -> usine_core::Result<bool> {
        Ok(true)
    }
    async fn push(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
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

fn ready_to_merge_card(store: &Store, project_id: uuid::Uuid, mergeable: Mergeable) -> Card {
    let mut card = Card::new(project_id, "t", "d", CardConfig::default());
    card.state = CardState::ReadyToMerge;
    card.branch = Some("feat/thing".into());
    card.mergeable = mergeable;
    card.pr = Some(PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: None,
        reviewer_recorded: false,
    });
    store.upsert_card(&card).unwrap();
    card
}

fn seeded(
    forge: Arc<GateForge>,
    git: Arc<dyn GitOps>,
    mergeable: Mergeable,
    worktree: Option<&Path>,
) -> (
    Store,
    Card,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-gate"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let mut card = ready_to_merge_card(&store, project.id, mergeable);
    if let Some(dir) = worktree {
        card.worktree_path = Some(dir.to_path_buf());
        store.upsert_card(&card).unwrap();
    }

    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge,
        git,
    });
    (store, card, handle, rx)
}

/// The background poll lands the forge's mergeability on a `ReadyToMerge` card,
/// which is what lets the board swap Merge for "Resolve conflicts" without any
/// user action. (The poll's first tick fires right after spawn.)
#[tokio::test]
async fn the_poll_lands_a_conflict_on_a_ready_to_merge_card() {
    let forge = Arc::new(GateForge {
        mergeable: Mergeable::Conflicting,
        merge_fails: false,
    });
    let (store, card, _handle, mut rx) = seeded(forge, Arc::new(SimGit), Mergeable::Unknown, None);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id && c.mergeable.is_conflicting() => {
            Some(())
        }
        _ => None,
    })
    .await;

    assert!(store.get_card(card.id).unwrap().mergeable.is_conflicting());
}

/// A merge refused on a conflict must gate the board on the spot: the conflict
/// is persisted alongside the `MergeConflict` event, not left for the next
/// five-minute poll tick to discover.
#[tokio::test]
async fn a_refused_merge_persists_the_conflict_immediately() {
    let forge = Arc::new(GateForge {
        mergeable: Mergeable::Conflicting,
        merge_fails: true,
    });
    let (store, card, handle, mut rx) = seeded(forge, Arc::new(SimGit), Mergeable::Unknown, None);
    handle.send(ExecutorCommand::Merge {
        card_id: card.id,
        delete_branch: true,
        force: false,
    });

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::MergeConflict { pr_number, .. } if e.card_id == card.id => {
            assert_eq!(*pr_number, 7);
            Some(())
        }
        _ => None,
    })
    .await;

    let card = store.get_card(card.id).unwrap();
    assert!(matches!(card.state, CardState::ReadyToMerge));
    assert!(
        card.mergeable.is_conflicting(),
        "the refused merge must persist the conflict for the board's gate"
    );
}

/// A resolve run's completing push (the merge commit) stales the cached
/// conflict: the card must come back to `ReadyToMerge` with `Unknown`, not the
/// `Conflicting` the run just cured — which would keep offering a resolve for a
/// conflict that no longer exists.
#[tokio::test]
async fn a_resolve_runs_push_resets_the_cached_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    // The forge keeps answering `Conflicting` (matching the seeded cache), so
    // the poll's immediate first tick can't paper over a missing reset — only
    // the resolve path itself writes `Unknown` here.
    let forge = Arc::new(GateForge {
        mergeable: Mergeable::Conflicting,
        merge_fails: false,
    });
    let (store, card, handle, mut rx) = seeded(
        forge,
        Arc::new(ConflictingGit),
        Mergeable::Conflicting,
        Some(tmp.path()),
    );
    handle.send(ExecutorCommand::ResolveConflicts { card_id: card.id });

    // Wait for the resolve run to actually start (the card leaves the gate)
    // before watching for the landing: the background poll's first tick also
    // emits a `CardUpdated` for the still-`ReadyToMerge` card (the sim forge's
    // comment counts differ from the seeded zeros), and matching that pre-run
    // echo would read the seeded `Conflicting` instead of the reset the run
    // performs.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == card.id && !matches!(c.state, CardState::ReadyToMerge) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // The run goes through applying-fixes and lands back at the gate.
    let mergeable = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == card.id && matches!(c.state, CardState::ReadyToMerge) =>
        {
            Some(c.mergeable)
        }
        _ => None,
    })
    .await;

    assert_eq!(mergeable, Mergeable::Unknown);
    assert_eq!(
        store.get_card(card.id).unwrap().mergeable,
        Mergeable::Unknown
    );
}

/// The resolve's clean-merge shortcut pushes without a run (and without
/// `finalize_run`), so it must reset the cached conflict itself.
#[tokio::test]
async fn a_conflict_that_resolved_itself_still_resets_the_cache() {
    let tmp = tempfile::tempdir().unwrap();
    // Same stance as above: the forge still answers `Conflicting`, so the reset
    // observed must be the clean-merge path's own.
    let forge = Arc::new(GateForge {
        mergeable: Mergeable::Conflicting,
        merge_fails: false,
    });
    let (store, card, handle, mut rx) = seeded(
        forge,
        Arc::new(SimGit),
        Mergeable::Conflicting,
        Some(tmp.path()),
    );
    handle.send(ExecutorCommand::ResolveConflicts { card_id: card.id });

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } => Some(message.clone()),
        _ => None,
    })
    .await;

    assert!(msg.contains("No conflicts left"), "got: {msg}");
    assert_eq!(
        store.get_card(card.id).unwrap().mergeable,
        Mergeable::Unknown
    );
}
