//! A card must not be called "ready to merge" while its CI is still starting.
//!
//! `statusCheckRollup` is empty for the first seconds of a PR's life — GitHub
//! has not registered the push's workflow runs yet — and taking that at face
//! value is what made a freshly-opened PR show a green Merge button and light
//! the dock badge before its build existed. On a project whose PRs get CI, the
//! executor marks the build in flight optimistically and holds it through a
//! registration grace; a dedicated fast poll then settles it the moment a real
//! status lands (or the grace expires on a PR that genuinely has no checks).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    now_millis, spawn_executor, Card, CardConfig, CardState, CheckStatus, DraftComment,
    ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, FailedCheck, Forge,
    Mergeable, PrInfo, PrSummary, Project, ProjectConfig, ReviewComment, ReviewEvent, ReviewScope,
    ReviewSub, ReviewSummary, ReviewThread, SimFactory, SimForge, SimGit, Store,
};

/// A quiet PR (no comments, no reviews, no threads) whose rollup reports
/// `checks`. `CheckStatus::None` stands for the empty rollup a just-pushed PR
/// answers with.
struct CiForge {
    checks: CheckStatus,
}

#[async_trait]
impl Forge for CiForge {
    async fn pr_checks(
        &self,
        _: &Path,
        _: u64,
    ) -> usine_core::Result<(CheckStatus, Vec<FailedCheck>)> {
        Ok((self.checks, Vec::new()))
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

/// A project with no reviewer (so a created PR walks straight to the merge
/// gate) and a known answer to "do this project's PRs get CI?".
fn seeded_project(ci_checks: Option<bool>) -> (Store, Project) {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        reviewer: None,
        ci_checks,
        ..ProjectConfig::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/usine-ci-pending-gate"), config);
    store.upsert_project(&project).unwrap();
    (store, project)
}

fn executor(
    store: &Store,
    checks: CheckStatus,
) -> (usine_core::ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(CiForge { checks }),
        git: Arc::new(SimGit),
    })
}

fn pr_ready_card(store: &Store, project_id: uuid::Uuid) -> uuid::Uuid {
    let mut card = Card::new(project_id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForPr);
    card.branch = Some("usine/card-x".into());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    card_id
}

fn merge_gate_card(store: &Store, project_id: uuid::Uuid, awaited_since: i64) -> uuid::Uuid {
    let mut card = Card::new(project_id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::ReadyToMerge;
    card.branch = Some("feat/thing".into());
    card.checks = CheckStatus::Pending;
    card.ci_awaited_since = Some(awaited_since);
    card.pr = Some(PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: None,
        reviewer_recorded: true,
    });
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    card_id
}

/// The regression: opening a PR on a project whose PRs get CI lands the card at
/// the merge gate *waiting on the build*, not wearing a green Merge button.
#[tokio::test]
async fn a_pr_opened_on_a_ci_project_waits_for_its_build() {
    let (store, project) = seeded_project(Some(true));
    let card_id = pr_ready_card(&store, project.id);
    // The empty rollup a PR created seconds ago really does answer.
    let (handle, mut rx) = executor(&store, CheckStatus::None);

    handle.send(ExecutorCommand::CreatePr {
        card_id,
        branch: "feat/thing".into(),
        title: "t".into(),
        body: "b".into(),
        reviewer: None,
        draft: false,
    });

    let card = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::ReadyToMerge).then(|| c.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(
        card.checks,
        CheckStatus::Pending,
        "the build is starting — the card must not claim green"
    );
    assert!(
        card.ci_awaited_since.is_some(),
        "the registration grace must be stamped so an empty rollup keeps reading as pending"
    );
    assert!(
        !card.needs_attention(),
        "a card waiting on CI must not light the dock badge"
    );
}

/// …and a project that has no CI is untouched: its cards still go green (and
/// badge) the instant the PR opens, exactly as before.
#[tokio::test]
async fn a_pr_opened_on_a_project_without_ci_is_ready_immediately() {
    let (store, project) = seeded_project(Some(false));
    let card_id = pr_ready_card(&store, project.id);
    let (handle, mut rx) = executor(&store, CheckStatus::None);

    handle.send(ExecutorCommand::CreatePr {
        card_id,
        branch: "feat/thing".into(),
        title: "t".into(),
        body: "b".into(),
        reviewer: None,
        draft: false,
    });

    let card = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            matches!(c.state, CardState::ReadyToMerge).then(|| c.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(card.checks, CheckStatus::None);
    assert_eq!(card.ci_awaited_since, None);
    assert!(
        card.needs_attention(),
        "nothing is coming — the card is ready and should say so"
    );
}

/// The build reports: the fast CI poll picks it up (its first tick fires on
/// spawn) and the project learns that its PRs do get checks.
#[tokio::test]
async fn a_reported_status_settles_the_gate_and_teaches_the_project() {
    let (store, project) = seeded_project(None);
    let card_id = merge_gate_card(&store, project.id, now_millis());
    let (_handle, mut rx) = executor(&store, CheckStatus::Passing);

    let card = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            (c.checks == CheckStatus::Passing).then(|| c.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(card.ci_awaited_since, None, "nothing is awaited any more");
    assert!(card.needs_attention(), "a green build is the user's turn");
    assert_eq!(
        store.get_project(project.id).unwrap().config.ci_checks,
        Some(true),
        "a reported status proves this project's PRs get CI"
    );
}

/// A PR that really has no checks: once the grace expires the empty rollup is
/// taken at face value, the gate opens, and the project learns it has no CI.
#[tokio::test]
async fn an_empty_rollup_past_the_grace_opens_the_gate() {
    let (store, project) = seeded_project(None);
    let card_id = merge_gate_card(&store, project.id, now_millis() - 120_000);
    let (_handle, mut rx) = executor(&store, CheckStatus::None);

    let card = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => {
            (c.checks == CheckStatus::None).then(|| c.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(card.ci_awaited_since, None);
    assert!(card.needs_attention());
    assert_eq!(
        store.get_project(project.id).unwrap().config.ci_checks,
        Some(false),
        "a whole grace window with an empty rollup proves there is no CI here"
    );
}
