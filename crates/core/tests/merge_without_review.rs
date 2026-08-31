//! The last-resort "Merge without review": the merge the PR-review panel offers
//! from `PrReview(Idle)` when no reviewer is coming. It skips the *review* and
//! nothing else — the CI gate still refuses a red build — it lands the card in
//! `Done` (not the `MergedWithoutReview` park, which stays reserved for merges
//! the poll *discovers* on GitHub), and it is refused outright from any state
//! that isn't a merge gate, so a stale panel can't merge behind a running fix.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, CheckStatus, CoreError, DraftComment,
    ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, FailedCheck, Forge,
    Mergeable, PrInfo, PrReviewSub, PrSummary, Project, ProjectConfig, ReviewComment, ReviewEvent,
    ReviewScope, ReviewSummary, Severity, SimFactory, SimForge, SimGit, Store,
};

/// A forge whose PR reports `checks`, recording whether `merge` was reached.
/// `merge_fails` makes the merge itself fail as a conflict (base = "dev"), for
/// proving a forced merge still goes through conflict detection.
struct CheckedForge {
    checks: CheckStatus,
    merge_fails: bool,
    /// The PR was already merged (on GitHub directly): `merge` refuses like gh
    /// does, and `is_merged` answers true.
    merged: bool,
    merge_called: AtomicBool,
}

impl CheckedForge {
    fn new(checks: CheckStatus) -> Self {
        CheckedForge {
            checks,
            merge_fails: false,
            merged: false,
            merge_called: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Forge for CheckedForge {
    async fn pr_checks(
        &self,
        _: &Path,
        _: u64,
    ) -> usine_core::Result<(CheckStatus, Vec<FailedCheck>)> {
        let failed = if self.checks == CheckStatus::Failing {
            vec![FailedCheck {
                name: "test".into(),
                workflow: "CI".into(),
                url: "https://github.com/example/repo/actions/runs/123/job/9".into(),
            }]
        } else {
            Vec::new()
        };
        Ok((self.checks, failed))
    }
    async fn merge(&self, _: &Path, _: u64) -> usine_core::Result<()> {
        self.merge_called.store(true, Ordering::SeqCst);
        if self.merge_fails {
            Err(CoreError::forge("gh pr merge 7 --squash failed"))
        } else if self.merged {
            Err(CoreError::forge("pull request #7 is already merged"))
        } else {
            Ok(())
        }
    }
    async fn is_merged(&self, _: &Path, _: u64) -> usine_core::Result<bool> {
        Ok(self.merged)
    }
    async fn merge_status(&self, _: &Path, _: u64) -> usine_core::Result<Mergeable> {
        Ok(if self.merge_fails {
            Mergeable::Conflicting
        } else {
            Mergeable::Clean
        })
    }
    async fn delete_remote_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    // Unreached by a merge; defer to the simulator.
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

/// A card parked at the PR gate with an open PR — what the panel's
/// "Merge without review" button acts on.
fn pr_review_card(store: &Store, project_id: uuid::Uuid, sub: PrReviewSub) -> Card {
    let mut card = Card::new(project_id, "t", "d", CardConfig::default());
    card.state = CardState::PrReview(sub);
    card.branch = Some("feat/thing".into());
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

fn merging(
    forge: Arc<CheckedForge>,
    sub: PrReviewSub,
) -> (Store, Card, UnboundedReceiver<ExecutorEvent>) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-without-review"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = pr_review_card(&store, project.id, sub);

    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge,
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::Merge {
        card_id: card.id,
        delete_branch: true,
        force: false,
    });
    (store, card, rx)
}

/// The happy path: green checks, the PR merges, and the card lands in `Done`
/// with the usual success toast — the same landing the merge gate gives, since
/// the user asked for this deliberately.
#[tokio::test]
async fn merging_without_review_lands_the_card_in_done() {
    let forge = Arc::new(CheckedForge::new(CheckStatus::Passing));
    let (store, card, mut rx) = merging(forge.clone(), PrReviewSub::Idle);

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } if e.card_id == card.id => Some(message.clone()),
        ExecutorEventKind::CardUpdated(c)
            if c.id == card.id && matches!(c.state, CardState::MergedWithoutReview { .. }) =>
        {
            panic!("a merge we performed lands in Done, not the external-merge park")
        }
        _ => None,
    })
    .await;

    assert!(msg.contains("Merged"), "got: {msg}");
    assert!(forge.merge_called.load(Ordering::SeqCst));
    let card = store.get_card(card.id).unwrap();
    assert!(matches!(card.state, CardState::Done), "{:?}", card.state);
    assert_eq!(card.pr.unwrap().state, "merged");
}

/// Skipping the review does not skip CI: a red build refuses the merge before
/// it reaches the forge and raises the usual fix offer, leaving the card at the
/// gate it came from.
#[tokio::test]
async fn a_red_build_still_refuses_the_merge_without_review() {
    let forge = Arc::new(CheckedForge::new(CheckStatus::Failing));
    let (store, card, mut rx) = merging(forge.clone(), PrReviewSub::Idle);

    let failed = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ChecksFailed { failed, .. } if e.card_id == card.id => {
            Some(failed.clone())
        }
        _ => None,
    })
    .await;

    assert_eq!(failed, vec!["test".to_string()]);
    assert!(
        !forge.merge_called.load(Ordering::SeqCst),
        "the CI gate is not bypassed by skipping the review"
    );
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::PrReview(PrReviewSub::Idle)
    ));
}

/// A panel left open while the card moved on (triage started under it) must not
/// merge the PR on the forge and only then fail the transition, which would
/// leave the card behind its own merged PR.
#[tokio::test]
async fn a_card_that_left_the_gate_is_refused_before_the_forge() {
    let forge = Arc::new(CheckedForge::new(CheckStatus::Passing));
    let (store, card, mut rx) = merging(forge.clone(), PrReviewSub::FetchingComments);

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } if e.card_id == card.id => Some(message.clone()),
        _ => None,
    })
    .await;

    assert!(msg.contains("merge gate"), "got: {msg}");
    assert!(
        !forge.merge_called.load(Ordering::SeqCst),
        "a card off the gate must never reach the forge's merge"
    );
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::PrReview(PrReviewSub::FetchingComments)
    ));
}
