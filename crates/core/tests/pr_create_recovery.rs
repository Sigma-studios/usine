//! `gh pr create` can exit non-zero after GitHub has already opened the PR — a
//! refused `--reviewer` request, or a timeout — and every retry then fails with
//! "a pull request for branch … already exists". Creating the PR must recover
//! the open PR on the card's branch instead of stranding the card at
//! `ReadyForPr`, taking the reviewer from what GitHub actually requested.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, CoreError, DraftComment, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Forge, Mergeable, PrInfo, PrReviewSub,
    PrSummary, Project, ProjectConfig, ReviewComment, ReviewEvent, ReviewScope, ReviewSub,
    ReviewSummary, ReviewThread, Severity, SimFactory, SimForge, SimGit, Store,
};

/// A quiet PR (no comments, reviews or threads) whose `create_pr` always fails
/// as gh does on a retry, while `pr_for_head` reports `existing`.
struct FailingCreateForge {
    existing: Option<PrInfo>,
}

#[async_trait]
impl Forge for FailingCreateForge {
    async fn create_pr(
        &self,
        _: &Path,
        _: &str,
        _: &str,
        _: &str,
        h: &str,
        _: Option<&str>,
        _: bool,
    ) -> usine_core::Result<PrInfo> {
        Err(CoreError::other(format!(
            "gh pr create failed: a pull request for branch \"{h}\" already exists"
        )))
    }
    async fn pr_for_head(&self, _: &Path, _: &str) -> usine_core::Result<Option<PrInfo>> {
        Ok(self.existing.clone())
    }
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

fn open_pr(reviewer: Option<&str>) -> PrInfo {
    PrInfo {
        number: 42,
        url: "https://github.com/example/repo/pull/42".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: reviewer.map(str::to_string),
        reviewer_recorded: false,
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

/// Seed a `ReadyForPr` card, spawn the executor over `forge`, and ask it to
/// create a PR requesting `me` as reviewer.
fn create_pr_with(
    existing: Option<PrInfo>,
) -> (
    Store,
    uuid::Uuid,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-pr-create-recovery"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForPr);
    card.branch = Some("feat/thing".into());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(FailingCreateForge { existing }),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::CreatePr {
        card_id,
        branch: "feat/thing".into(),
        title: "t".into(),
        body: "b".into(),
        reviewer: Some("me".into()),
        draft: false,
    });
    (store, card_id, handle, rx)
}

/// The reviewer request was refused, so GitHub lists nobody: the card records
/// the existing PR, warns, and — with no reviewer to wait on — reaches the
/// merge gate.
#[tokio::test]
async fn a_failed_create_recovers_the_open_pr_and_advances_without_a_reviewer() {
    let (store, card_id, _handle, mut rx) = create_pr_with(Some(open_pr(None)));

    let mut warned = false;
    wait_for(&mut rx, |e| {
        if e.card_id != card_id {
            return None;
        }
        match &e.kind {
            ExecutorEventKind::Toast { severity, message }
                if *severity == Severity::Warning && message.contains("PR #42") =>
            {
                warned = true;
                None
            }
            ExecutorEventKind::CardUpdated(c) => {
                matches!(c.state, CardState::ReadyToMerge).then_some(())
            }
            _ => None,
        }
    })
    .await;
    assert!(warned, "the gh error must still be surfaced as a warning");

    let card = store.get_card(card_id).unwrap();
    let pr = card.pr.expect("the recovered PR is recorded on the card");
    assert_eq!(pr.number, 42);
    assert_eq!(pr.reviewer, None);
    assert!(pr.reviewer_recorded);
}

/// When GitHub reports a requested reviewer, the recovered PR carries it and
/// the card waits at the PR gate for their verdict.
#[tokio::test]
async fn a_recovered_pr_keeps_the_reviewer_github_reports() {
    let (store, card_id, _handle, mut rx) = create_pr_with(Some(open_pr(Some("octocat"))));

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::PrReview(PrReviewSub::Idle)).then_some(())
        }
        _ => None,
    })
    .await;
    // Give a wrongly-fired no-reviewer advance time to land.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let card = store.get_card(card_id).unwrap();
    assert!(matches!(card.state, CardState::PrReview(PrReviewSub::Idle)));
    assert_eq!(card.pr.unwrap().reviewer.as_deref(), Some("octocat"));
}

/// No PR on the branch: the failure is genuine, so the card stays put with no
/// PR and the error still reaches the user.
#[tokio::test]
async fn a_failed_create_with_no_open_pr_still_fails() {
    let (store, card_id, _handle, mut rx) = create_pr_with(None);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast { severity, message }
            if *severity == Severity::Error && message.contains("already exists") =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    let card = store.get_card(card_id).unwrap();
    assert!(matches!(
        card.state,
        CardState::AwaitingReview(ReviewSub::ReadyForPr)
    ));
    assert!(card.pr.is_none());
}
