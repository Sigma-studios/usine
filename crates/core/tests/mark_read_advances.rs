//! "Mark as read" is the way out of a PR gate whose only outstanding feedback
//! is a review *body* — a bot's pass report, say — that needs no agent run. It
//! records the bodies locally, which makes the merge-clear predicates true; the
//! card must therefore reach the merge gate right then, not on the next
//! five-minute poll tick. A card stranded at the gate with no triage offer and
//! no badge for minutes is the "it fixed itself after a while" bug.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, DraftComment, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, Forge, Mergeable, PrInfo, PrReviewSub, PrSummary, Project,
    ProjectConfig, ReviewComment, ReviewEvent, ReviewScope, ReviewSummary, ReviewThread,
    SimFactory, SimForge, SimGit, Store,
};

/// A PR the configured reviewer has approved, with one bot review whose body is
/// the only thing left outstanding: no inline comments, no threads. Everything
/// the merge gate waits on is that body.
struct ApprovedWithBotBodyForge;

fn reviews() -> Vec<ReviewSummary> {
    let mut approval = ReviewSummary::new("octocat", "APPROVED");
    approval.submitted_at = "2026-08-21T13:00:00Z".into();
    let mut bot = ReviewSummary::new("ci-bot", "COMMENTED");
    bot.body = "All checks green. Nothing to do here.".into();
    bot.submitted_at = "2026-08-21T13:05:00Z".into();
    vec![approval, bot]
}

#[async_trait]
impl Forge for ApprovedWithBotBodyForge {
    async fn fetch_comments(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewComment>> {
        Ok(vec![])
    }
    async fn list_submitted_reviews(
        &self,
        _: &Path,
        _: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        Ok(reviews())
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
    async fn merge_status(&self, r: &Path, n: u64) -> usine_core::Result<Mergeable> {
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

#[tokio::test]
async fn marking_the_last_review_body_read_carries_the_card_to_the_merge_gate() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-mark-read-advances"),
        ProjectConfig {
            reviewer: Some("octocat".into()),
            ..ProjectConfig::default()
        },
    );
    store.upsert_project(&project).unwrap();

    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::PrReview(PrReviewSub::Idle);
    card.branch = Some("feat/thing".into());
    card.pr = Some(PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: Some("octocat".into()),
        reviewer_recorded: true,
    });
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(ApprovedWithBotBodyForge),
        git: Arc::new(SimGit),
    });

    // The poll's first tick syncs the reviews onto the card. It must NOT advance
    // it: the bot's body is still pending, so the approval doesn't clear merge.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            (c.reviews.len() == 2).then_some(())
        }
        _ => None,
    })
    .await;
    let card = store.get_card(card_id).unwrap();
    assert!(
        matches!(card.state, CardState::PrReview(PrReviewSub::Idle)),
        "a pending review body holds the card at the PR gate: {:?}",
        card.state
    );
    assert_eq!(card.pending_review_bodies().len(), 1);

    handle.send(ExecutorCommand::MarkReviewBodiesRead { card_id });

    // No poll tick in between (the interval is minutes): the command itself
    // re-runs the merge-clear predicates.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::ReadyToMerge).then_some(())
        }
        _ => None,
    })
    .await;
}
