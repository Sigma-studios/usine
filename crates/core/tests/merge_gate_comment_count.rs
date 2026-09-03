//! A PR-comment fix run answers every comment the picker showed: the checked
//! ones by fixing (and resolving their threads), the declined ones by replying.
//! So the card must arrive at the merge gate with nothing outstanding.
//!
//! The correction that used to carry that news — the `list_reviews` inside
//! `ResolveFixedComments` — runs *after* the transition and only warns on
//! failure, so the gate spent seconds (and, on a failed round trip, up to a
//! poll interval) asserting "N review threads have no answer yet" from the
//! pre-fix count. The forge here fails every call the follow-up makes, which is
//! the worst case: the count must be right without it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, CoreError, DraftComment, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, FixVerdict, Forge, Mergeable, PrInfo,
    PrReviewSub, PrSummary, Project, ProjectConfig, ReviewComment, ReviewEvent, ReviewScope,
    ReviewSummary, ReviewThread, SimFactory, SimForge, SimGit, Store,
};

/// A forge whose every post-fix read fails: resolving the threads errors, and so
/// does the refresh that follows it. Nothing can correct the card's counts after
/// the transition — only what `finalize_run` itself wrote.
struct BrokenAfterFixForge;

#[async_trait]
impl Forge for BrokenAfterFixForge {
    async fn resolve_threads(&self, _: &Path, _: u64, _: &[u64]) -> usine_core::Result<usize> {
        Err(CoreError::other("gh: could not resolve threads"))
    }
    async fn list_threads(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewThread>> {
        Err(CoreError::other("gh: offline"))
    }
    async fn fetch_comments(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewComment>> {
        Err(CoreError::other("gh: offline"))
    }
    async fn list_submitted_reviews(
        &self,
        _: &Path,
        _: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        Err(CoreError::other("gh: offline"))
    }
    async fn reply_to_comment(&self, _: &Path, _: u64, _: u64, _: &str) -> usine_core::Result<()> {
        Ok(())
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

fn verdict(id: u64, body: &str, selected: bool) -> FixVerdict {
    FixVerdict {
        comment: ReviewComment {
            id,
            author: "octocat".into(),
            body: body.into(),
            path: "src/lib.rs".into(),
            line: Some(1),
            review_body_of: None,
        },
        worth_fixing: selected,
        severity: "medium".into(),
        rationale: "because".into(),
        selected,
        reply: if selected {
            String::new()
        } else {
            "Leaving this as is.".into()
        },
        instruction: String::new(),
    }
}

/// Seed a card parked in the PR fix picker with two comments outstanding.
fn picker_card(store: &Store, project_id: uuid::Uuid, dir: &str) -> uuid::Uuid {
    let mut card = Card::new(project_id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::PrReview(PrReviewSub::SelectingFixes {
        verdicts: vec![verdict(1, "fix this", true), verdict(2, "and this", false)],
    });
    card.branch = Some("feat/thing".into());
    card.worktree_path = Some(PathBuf::from(format!("{dir}-wt")));
    card.unanswered_count = 2;
    card.comment_count = 2;
    card.reviewer_comment_count = 2;
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
    card_id
}

fn setup(
    dir: &str,
) -> (
    Store,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
    uuid::Uuid,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from(dir), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::create_dir_all(format!("{dir}-wt")).unwrap();
    let card_id = picker_card(&store, project.id, dir);
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(BrokenAfterFixForge),
        git: Arc::new(SimGit),
    });
    (store, handle, rx, card_id)
}

/// The card reaches the merge gate with no unanswered comments — on the very
/// `CardUpdated` that carries `ReadyToMerge`, not seconds later.
#[tokio::test]
async fn a_finished_fix_run_clears_the_gate_counts_with_the_transition() {
    let (_store, handle, mut rx, card_id) = setup("/tmp/usine-merge-gate-count-fix");

    handle.send(ExecutorCommand::ApplyFixes {
        card_id,
        verdicts: vec![verdict(1, "fix this", true), verdict(2, "and this", false)],
        note: String::new(),
        prompt: None,
    });

    let count = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::ReadyToMerge).then_some(c.unanswered_count)
        }
        _ => None,
    })
    .await;
    assert_eq!(
        count, 0,
        "the merge gate must not assert the pre-fix comment count"
    );
}

/// Same promise on the picker's other exit: nothing checked and no task, so the
/// declined comments are replied to and the card advances with no fix run at
/// all. That path applies its own transition, so it needs its own clearing.
#[tokio::test]
async fn declining_every_comment_also_clears_the_gate_counts() {
    let (_store, handle, mut rx, card_id) = setup("/tmp/usine-merge-gate-count-decline");

    handle.send(ExecutorCommand::ApplyFixes {
        card_id,
        verdicts: vec![verdict(1, "fix this", false), verdict(2, "and this", false)],
        note: String::new(),
        prompt: Some(String::new()),
    });

    let count = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::ReadyToMerge).then_some(c.unanswered_count)
        }
        _ => None,
    })
    .await;
    assert_eq!(count, 0);
}
