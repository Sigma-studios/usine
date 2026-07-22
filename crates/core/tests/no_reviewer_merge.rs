//! A project with no assigned reviewer opens PRs nobody is asked to review: no
//! approval will ever land, so the card must not strand at the PR gate. Creating
//! such a PR carries the card straight to `ReadyToMerge`, and the background
//! poll recovers cards that were already parked at the gate — unless a reviewer
//! is actually configured, in which case the gate waits for their verdict.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, DraftComment, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, Forge, MergeStatus, PrInfo, PrReviewSub, PrSummary, Project,
    ProjectConfig, ReviewComment, ReviewEvent, ReviewSub, ReviewSummary, ReviewThread, SimFactory,
    SimForge, SimGit, Store,
};

/// A forge where nothing has happened on the PR yet: no comments, no submitted
/// reviews, no threads. `SimForge` cans three comments and a CHANGES_REQUESTED
/// review, which would (correctly) block the no-reviewer advance — this is the
/// quiet PR a reviewer-less project actually produces. Everything else defers
/// to the simulator.
struct QuietForge;

#[async_trait]
impl Forge for QuietForge {
    async fn fetch_comments(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewComment>> {
        Ok(vec![])
    }
    async fn list_submitted_reviews(
        &self,
        _: &Path,
        _: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        Ok(vec![])
    }
    async fn list_threads(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewThread>> {
        Ok(vec![])
    }
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
    async fn list_review_prs(&self, r: &Path, a: &[String]) -> usine_core::Result<Vec<PrSummary>> {
        SimForge.list_review_prs(r, a).await
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
    async fn merge_status(&self, r: &Path, n: u64) -> usine_core::Result<MergeStatus> {
        SimForge.merge_status(r, n).await
    }
    async fn delete_remote_branch(&self, r: &Path, b: &str) -> usine_core::Result<()> {
        SimForge.delete_remote_branch(r, b).await
    }
    async fn resolve_threads(&self, r: &Path, n: u64, c: &[u64]) -> usine_core::Result<usize> {
        SimForge.resolve_threads(r, n, c).await
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

fn seeded_project(reviewer: Option<&str>) -> (Store, Project) {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        reviewer: reviewer.map(str::to_string),
        ..ProjectConfig::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/usine-no-reviewer-merge"), config);
    store.upsert_project(&project).unwrap();
    (store, project)
}

fn executor(store: &Store) -> (usine_core::ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(QuietForge),
        git: Arc::new(SimGit),
    })
}

/// Opening a PR with no reviewer requested (and none configured on the project)
/// lands the card at the merge gate immediately — no waiting on the poll.
#[tokio::test]
async fn creating_a_pr_without_a_reviewer_goes_straight_to_the_merge_gate() {
    let (store, project) = seeded_project(None);
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForPr);
    card.branch = Some("usine/card-x".into());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::CreatePr {
        card_id,
        branch: "feat/thing".into(),
        title: "t".into(),
        body: "b".into(),
        reviewer: None,
        draft: false,
    });

    // The PR-gate state is still applied (and emitted) first; the advance rides
    // on top rather than replacing the transition.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::PrReview(PrReviewSub::Idle)).then_some(())
        }
        _ => None,
    })
    .await;
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::ReadyToMerge).then_some(())
        }
        _ => None,
    })
    .await;
}

fn idle_pr_card(store: &Store, project_id: uuid::Uuid) -> uuid::Uuid {
    let mut card = Card::new(project_id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::PrReview(PrReviewSub::Idle);
    card.branch = Some("feat/thing".into());
    card.pr = Some(PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: None,
    });
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    card_id
}

/// A card already stranded at the PR gate (opened before this advance existed)
/// is recovered by the background poll's immediate first tick.
#[tokio::test]
async fn the_poll_unsticks_a_stranded_card_when_no_reviewer_is_assigned() {
    let (store, project) = seeded_project(None);
    let card_id = idle_pr_card(&store, project.id);
    let (_handle, mut rx) = executor(&store);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::ReadyToMerge).then_some(())
        }
        _ => None,
    })
    .await;
}

/// With a reviewer configured on the project, a quiet PR means "still waiting
/// for their verdict" — the card must stay parked at the PR gate.
#[tokio::test]
async fn a_configured_reviewer_keeps_the_card_waiting_at_the_pr_gate() {
    let (store, project) = seeded_project(Some("octocat"));
    let card_id = idle_pr_card(&store, project.id);
    let (_handle, _rx) = executor(&store);

    // The poll's first tick fires immediately; give it (and any wrongly-fired
    // advance) time to land before checking the card hasn't moved.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        matches!(
            store.get_card(card_id).unwrap().state,
            CardState::PrReview(PrReviewSub::Idle)
        ),
        "an assigned reviewer's PR must wait for their review"
    );
}
