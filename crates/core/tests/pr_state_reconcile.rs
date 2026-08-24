//! Reconciling local PR state with GitHub. A PR merged or closed on GitHub
//! directly used to leave its card showing "PR open" forever (and its review
//! task queued forever): nothing ever re-read the PR's lifecycle state after
//! creation. The poll (and the panel's ↻, which shares `fetch_review_status`)
//! now reads it and retires the card — to "Merged w/o review" when the review
//! hadn't finished, to `Done` when it had — and the review scan confirms each
//! tracked PR that vanished from the open listing before moving it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, CheckStatus, CoreError, DraftComment,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Forge, LivePrState, Mergeable, PrInfo,
    PrSummary, Project, ProjectConfig, ReviewComment, ReviewEvent, ReviewStatus, ReviewSummary,
    ReviewTask, SimFactory, SimForge, SimGit, Store,
};

/// A forge that answers `pr_live_state` from a per-PR table (`fail_live` makes
/// it error instead) and lists `open_prs` for the review scan. Everything else
/// defers to the simulator — whose submitted reviews include a
/// CHANGES_REQUESTED, so an `Idle` card never auto-advances underneath a test.
struct LiveForge {
    live: HashMap<u64, LivePrState>,
    fail_live: bool,
    open_prs: Vec<PrSummary>,
}

impl LiveForge {
    fn new(live: impl IntoIterator<Item = (u64, LivePrState)>) -> Self {
        LiveForge {
            live: live.into_iter().collect(),
            fail_live: false,
            open_prs: Vec::new(),
        }
    }
}

#[async_trait]
impl Forge for LiveForge {
    async fn pr_live_state(&self, _: &Path, n: u64) -> usine_core::Result<Option<LivePrState>> {
        if self.fail_live {
            return Err(CoreError::forge("gh pr view timed out"));
        }
        Ok(self.live.get(&n).copied())
    }
    async fn list_review_prs(&self, _: &Path, _: &[String]) -> usine_core::Result<Vec<PrSummary>> {
        Ok(self.open_prs.clone())
    }
    // Unreached by the reconciliation; defer to the simulator.
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
    async fn merge(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.merge(r, n).await
    }
    async fn is_merged(&self, r: &Path, n: u64) -> usine_core::Result<bool> {
        SimForge.is_merged(r, n).await
    }
    async fn merge_status(&self, r: &Path, n: u64) -> usine_core::Result<usine_core::Mergeable> {
        SimForge.merge_status(r, n).await
    }
    async fn delete_remote_branch(&self, r: &Path, b: &str) -> usine_core::Result<()> {
        SimForge.delete_remote_branch(r, b).await
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

fn pr_card(store: &Store, project_id: uuid::Uuid, state: CardState, pr_state: &str) -> Card {
    let mut card = Card::new(project_id, "t", "d", CardConfig::default());
    card.state = state;
    card.branch = Some("usine/thing".into());
    card.worktree_path = Some(PathBuf::from("/tmp/usine-reconcile-worktree"));
    card.pr = Some(PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: pr_state.into(),
        reviewer: Some("octocat".into()),
        reviewer_recorded: true,
    });
    store.upsert_card(&card).unwrap();
    card
}

/// Spawn the executor over a store — the poll's first tick fires immediately.
fn polling(store: Store, forge: Arc<LiveForge>) -> UnboundedReceiver<ExecutorEvent> {
    let (_handle, rx) = spawn_executor(ExecutorConfig {
        store,
        providers: Arc::new(SimFactory),
        forge,
        git: Arc::new(SimGit),
    });
    rx
}

fn project_with(store: &Store, config: ProjectConfig) -> Project {
    let project = Project::new("p", PathBuf::from("/tmp/usine-reconcile"), config);
    store.upsert_project(&project).unwrap();
    project
}

// --- cards -------------------------------------------------------------------

/// A PR merged on GitHub while its review comments were still pending parks the
/// card on "Merged w/o review", records the merged PR state, and sheds the
/// worktree.
#[tokio::test]
async fn merged_pr_with_pending_review_parks_on_the_new_column() {
    let store = Store::open_in_memory().unwrap();
    let project = project_with(&store, ProjectConfig::default());
    let card = pr_card(
        &store,
        project.id,
        CardState::PrReview(usine_core::PrReviewSub::Idle),
        "open",
    );
    let forge = Arc::new(LiveForge::new([(7, LivePrState::Merged)]));
    let mut rx = polling(store.clone(), forge);

    // The transition and the worktree cleanup land as separate updates; wait
    // for the second so the teardown is finished before asserting.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            (matches!(c.state, CardState::MergedWithoutReview { merged: true })
                && c.worktree_path.is_none())
            .then_some(())
        }
        _ => None,
    })
    .await;

    let card = store.get_card(card.id).unwrap();
    assert_eq!(card.state, CardState::MergedWithoutReview { merged: true });
    assert_eq!(card.pr.as_ref().unwrap().state, "merged");
    assert!(
        card.worktree_path.is_none(),
        "the worktree is cleaned up like a normal merge's"
    );
    assert!(
        card.branch.is_some(),
        "the branch is never deleted by reconciliation"
    );
}

/// A `ReadyToMerge` card — its review completed — whose PR was merged on GitHub
/// directly reaches `Done`, mirroring `merge()`'s already-merged recovery.
#[tokio::test]
async fn merged_pr_with_review_passed_reaches_done() {
    let store = Store::open_in_memory().unwrap();
    let project = project_with(&store, ProjectConfig::default());
    let card = pr_card(&store, project.id, CardState::ReadyToMerge, "open");
    let forge = Arc::new(LiveForge::new([(7, LivePrState::Merged)]));
    let mut rx = polling(store.clone(), forge);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            (matches!(c.state, CardState::Done) && c.worktree_path.is_none()).then_some(())
        }
        _ => None,
    })
    .await;

    let card = store.get_card(card.id).unwrap();
    assert_eq!(card.pr.as_ref().unwrap().state, "merged");
    assert!(card.worktree_path.is_none());
}

/// A PR closed without merging parks the card on the same column, flagged
/// closed — from either gate.
#[tokio::test]
async fn closed_pr_parks_flagged_closed() {
    for state in [
        CardState::PrReview(usine_core::PrReviewSub::Idle),
        CardState::ReadyToMerge,
    ] {
        let store = Store::open_in_memory().unwrap();
        let project = project_with(&store, ProjectConfig::default());
        let card = pr_card(&store, project.id, state.clone(), "open");
        let forge = Arc::new(LiveForge::new([(7, LivePrState::Closed)]));
        let mut rx = polling(store.clone(), forge);

        wait_for(&mut rx, |e| match &e.kind {
            ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
                (matches!(c.state, CardState::MergedWithoutReview { merged: false })
                    && c.worktree_path.is_none())
                .then_some(())
            }
            _ => None,
        })
        .await;

        let card = store.get_card(card.id).unwrap();
        assert_eq!(card.pr.as_ref().unwrap().state, "closed", "{state:?}");
        assert!(card.worktree_path.is_none(), "{state:?}");
        assert!(card.branch.is_some(), "{state:?}");
    }
}

/// A failed live-state read must leave the card exactly where it is — a network
/// hiccup is not a closed PR. The tick's normal count refresh still lands,
/// which is what the test waits on.
#[tokio::test]
async fn a_failed_live_read_moves_nothing() {
    let store = Store::open_in_memory().unwrap();
    let project = project_with(&store, ProjectConfig::default());
    let card = pr_card(
        &store,
        project.id,
        CardState::PrReview(usine_core::PrReviewSub::Idle),
        "open",
    );
    let forge = Arc::new(LiveForge {
        live: HashMap::new(),
        fail_live: true,
        open_prs: Vec::new(),
    });
    let mut rx = polling(store.clone(), forge);

    wait_for(&mut rx, |e| match &e.kind {
        // The sim comments landing proves the tick processed this card.
        ExecutorEventKind::CardUpdated(c) if c.id == card.id && c.comment_count > 0 => Some(()),
        _ => None,
    })
    .await;

    let card = store.get_card(card.id).unwrap();
    assert!(matches!(
        card.state,
        CardState::PrReview(usine_core::PrReviewSub::Idle)
    ));
    assert_eq!(card.pr.as_ref().unwrap().state, "open");
    assert!(card.worktree_path.is_some());
}

/// A draft marked ready on GitHub (outside the app) syncs the card's recorded
/// PR state without moving the card.
#[tokio::test]
async fn a_draft_flipped_ready_on_github_syncs_the_badge() {
    let store = Store::open_in_memory().unwrap();
    let project = project_with(&store, ProjectConfig::default());
    let card = pr_card(&store, project.id, CardState::ReadyToMerge, "draft");
    let forge = Arc::new(LiveForge::new([(7, LivePrState::Open { draft: false })]));
    let mut rx = polling(store.clone(), forge);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            (c.pr.as_ref().map(|p| p.state.as_str()) == Some("open")).then_some(())
        }
        _ => None,
    })
    .await;

    let card = store.get_card(card.id).unwrap();
    assert!(matches!(card.state, CardState::ReadyToMerge));
    assert!(card.worktree_path.is_some());
}

// --- review tasks -------------------------------------------------------------

fn seed_task(store: &Store, project_id: uuid::Uuid, pr: u64, status: ReviewStatus) -> ReviewTask {
    let mut task = ReviewTask::new(project_id, pr, "t", "octocat", "u", "feat/x", "main");
    task.status = status;
    store.upsert_review_task(&task).unwrap();
    task
}

fn reviewing_project(store: &Store) -> Project {
    project_with(
        store,
        ProjectConfig {
            review_contributors: vec!["octocat".into()],
            ..Default::default()
        },
    )
}

/// The scan's end-of-pass task list — the signal that a whole scan (including
/// its reconciliation) finished.
fn scan_done(e: &ExecutorEvent, project_id: uuid::Uuid) -> Option<()> {
    match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { project_id: p, .. } if *p == project_id => Some(()),
        _ => None,
    }
}

/// A tracked PR absent from the open listing is confirmed individually: merged
/// → the task moves to "merged w/o review" (kept, not deleted, and NOT on the
/// permanent dismissed list, so a reopen can heal it).
#[tokio::test]
async fn a_vanished_pr_confirmed_merged_moves_to_the_column() {
    let store = Store::open_in_memory().unwrap();
    let project = reviewing_project(&store);
    let task = seed_task(&store, project.id, 101, ReviewStatus::ToReview);
    let forge = Arc::new(LiveForge::new([(101, LivePrState::Merged)]));
    let mut rx = polling(store.clone(), forge);

    wait_for(&mut rx, |e| scan_done(e, project.id)).await;

    let task = store.get_review_task(task.id).unwrap();
    assert_eq!(
        task.status,
        ReviewStatus::MergedWithoutReview { merged: true }
    );
    assert!(
        !store
            .dismissed_reviews(project.id)
            .unwrap_or_default()
            .contains(&101),
        "retiring is not dismissing — the PR can come back"
    );
}

/// A PR closed without merging mid-review moves the task the same way, flagged
/// closed, and drops its worktree pointer.
#[tokio::test]
async fn a_vanished_pr_confirmed_closed_moves_mid_review() {
    let store = Store::open_in_memory().unwrap();
    let project = reviewing_project(&store);
    let mut task = seed_task(&store, project.id, 101, ReviewStatus::Reviewing);
    task.worktree_path = Some(PathBuf::from("/tmp/usine-reconcile-review-wt"));
    store.upsert_review_task(&task).unwrap();
    let forge = Arc::new(LiveForge::new([(101, LivePrState::Closed)]));
    let mut rx = polling(store.clone(), forge);

    wait_for(&mut rx, |e| scan_done(e, project.id)).await;

    let task = store.get_review_task(task.id).unwrap();
    assert_eq!(
        task.status,
        ReviewStatus::MergedWithoutReview { merged: false }
    );
    assert!(task.worktree_path.is_none());
}

/// Absence from the listing alone proves nothing (drafts and already-reviewed
/// PRs are filtered out of it too) — a task whose confirmation fails, or whose
/// PR is actually still open, stays put.
#[tokio::test]
async fn an_unconfirmed_vanished_pr_stays_put() {
    // Confirmation errors out.
    let store = Store::open_in_memory().unwrap();
    let project = reviewing_project(&store);
    let task = seed_task(&store, project.id, 101, ReviewStatus::ToReview);
    let forge = Arc::new(LiveForge {
        live: HashMap::new(),
        fail_live: true,
        open_prs: Vec::new(),
    });
    let mut rx = polling(store.clone(), forge);
    wait_for(&mut rx, |e| scan_done(e, project.id)).await;
    assert_eq!(
        store.get_review_task(task.id).unwrap().status,
        ReviewStatus::ToReview
    );

    // The PR is still open — it just left the search (e.g. flipped to draft).
    let store = Store::open_in_memory().unwrap();
    let project = reviewing_project(&store);
    let task = seed_task(&store, project.id, 101, ReviewStatus::ToReview);
    let forge = Arc::new(LiveForge::new([(101, LivePrState::Open { draft: true })]));
    let mut rx = polling(store.clone(), forge);
    wait_for(&mut rx, |e| scan_done(e, project.id)).await;
    assert_eq!(
        store.get_review_task(task.id).unwrap().status,
        ReviewStatus::ToReview
    );
}

/// A retired task whose PR shows up open again was reopened on GitHub — it
/// heals back to the review queue.
#[tokio::test]
async fn a_reopened_pr_heals_back_to_the_queue() {
    let store = Store::open_in_memory().unwrap();
    let project = reviewing_project(&store);
    let task = seed_task(
        &store,
        project.id,
        101,
        ReviewStatus::MergedWithoutReview { merged: false },
    );
    let forge = Arc::new(LiveForge {
        live: HashMap::new(),
        fail_live: false,
        open_prs: vec![PrSummary {
            number: 101,
            title: "Add caching layer".into(),
            author: "octocat".into(),
            head_ref: "feat/x".into(),
            base_ref: "main".into(),
            url: "u".into(),
            body: String::new(),
            checks: CheckStatus::None,
            mergeable: Mergeable::Unknown,
        }],
    });
    let mut rx = polling(store.clone(), forge);

    wait_for(&mut rx, |e| scan_done(e, project.id)).await;

    assert_eq!(
        store.get_review_task(task.id).unwrap().status,
        ReviewStatus::ToReview
    );
}
