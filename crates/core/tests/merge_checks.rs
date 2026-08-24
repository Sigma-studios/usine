//! The merge gate on a PR's CI checks. A merge asked for while the checks are
//! red or still running must not reach the forge: failing checks come back as
//! an offer to fix them with an agent (mirroring the merge-conflict flow), and
//! pending ones as a "wait" toast — in both cases the card stays in
//! `ReadyToMerge`. The user's explicit "merge anyway" (`force`) skips exactly
//! that pre-check and nothing else: a forced merge that hits a conflict still
//! surfaces it as a conflict.

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
    Mergeable, PrInfo, PrSummary, Project, ProjectConfig, ReviewComment, ReviewEvent,
    ReviewSummary, Severity, SimFactory, SimForge, SimGit, Store,
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

fn ready_to_merge_card(store: &Store, project_id: uuid::Uuid) -> Card {
    let mut card = Card::new(project_id, "t", "d", CardConfig::default());
    card.state = CardState::ReadyToMerge;
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
    force: bool,
) -> (Store, Card, UnboundedReceiver<ExecutorEvent>) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-checks"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = ready_to_merge_card(&store, project.id);

    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge,
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::Merge {
        card_id: card.id,
        delete_branch: true,
        force,
    });
    (store, card, rx)
}

/// Failing checks block the merge before it reaches the forge, surfacing as the
/// fix offer (naming the checks) rather than an error — and the card stays at
/// the gate.
#[tokio::test]
async fn failing_checks_block_the_merge_and_offer_the_fix() {
    let forge = Arc::new(CheckedForge::new(CheckStatus::Failing));
    let (store, card, mut rx) = merging(forge.clone(), false);

    let (pr, failed) = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ChecksFailed { pr_number, failed } if e.card_id == card.id => {
            Some((*pr_number, failed.clone()))
        }
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => panic!("failing checks must not surface as an error: {message}"),
        _ => None,
    })
    .await;

    assert_eq!(pr, 7);
    assert_eq!(failed, vec!["test".to_string()], "the dialog names them");
    assert!(
        !forge.merge_called.load(Ordering::SeqCst),
        "a red PR must never reach the forge's merge"
    );
    let card = store.get_card(card.id).unwrap();
    assert!(matches!(card.state, CardState::ReadyToMerge));
    assert_eq!(
        card.checks,
        CheckStatus::Failing,
        "the freshly-read status lands on the card for the badge"
    );
}

/// Still-running checks also block — merging under them would be merging on
/// hope — but as a warning to wait (or force), not a fix offer.
#[tokio::test]
async fn pending_checks_block_the_merge_with_a_warning() {
    let forge = Arc::new(CheckedForge::new(CheckStatus::Pending));
    let (store, card, mut rx) = merging(forge.clone(), false);

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Warning,
            message,
        } => Some(message.clone()),
        ExecutorEventKind::ChecksFailed { .. } => {
            panic!("pending is not failing — no fix to offer")
        }
        _ => None,
    })
    .await;

    assert!(msg.contains("still running"), "got: {msg}");
    assert!(!forge.merge_called.load(Ordering::SeqCst));
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::ReadyToMerge
    ));
}

/// Green checks — and a repo with no checks at all — merge exactly as before.
#[tokio::test]
async fn passing_or_absent_checks_merge_as_before() {
    for status in [CheckStatus::Passing, CheckStatus::None] {
        let forge = Arc::new(CheckedForge::new(status));
        let (store, card, mut rx) = merging(forge.clone(), false);

        wait_for(&mut rx, |e| match &e.kind {
            ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
                matches!(c.state, CardState::Done).then_some(())
            }
            _ => None,
        })
        .await;

        assert!(forge.merge_called.load(Ordering::SeqCst));
        assert!(matches!(
            store.get_card(card.id).unwrap().state,
            CardState::Done
        ));
    }
}

/// `force` is the user's explicit override for flaky CI: the checks pre-check
/// is skipped and the red PR merges.
#[tokio::test]
async fn a_forced_merge_skips_the_checks_gate() {
    let forge = Arc::new(CheckedForge::new(CheckStatus::Failing));
    let (store, card, mut rx) = merging(forge.clone(), true);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            matches!(c.state, CardState::Done).then_some(())
        }
        ExecutorEventKind::ChecksFailed { .. } => {
            panic!("force must skip the checks gate entirely")
        }
        _ => None,
    })
    .await;

    assert!(forge.merge_called.load(Ordering::SeqCst));
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::Done
    ));
}

/// The override bypasses only the checks: a forced merge that fails on a
/// conflict still comes back as the conflict offer, never as a silent success
/// or a raw error.
#[tokio::test]
async fn a_forced_merge_still_surfaces_conflicts() {
    let forge = Arc::new(CheckedForge {
        checks: CheckStatus::Failing,
        merge_fails: true,
        merged: false,
        merge_called: AtomicBool::new(false),
    });
    let (store, card, mut rx) = merging(forge.clone(), true);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::MergeConflict { pr_number, .. } if e.card_id == card.id => {
            assert_eq!(*pr_number, 7);
            Some(())
        }
        _ => None,
    })
    .await;

    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::ReadyToMerge
    ));
}

/// A PR merged on GitHub directly while its checks were red (or pending) must
/// still reach `Done` through the plain Merge button: the checks gate only
/// guards a PR that still needs merging. Gating here would offer a fix run
/// against a merged (possibly deleted) branch — or a wait for a green that
/// will never come — and the card could never shed its worktree.
#[tokio::test]
async fn an_already_merged_pr_is_not_gated_on_its_red_checks() {
    for status in [CheckStatus::Failing, CheckStatus::Pending] {
        let forge = Arc::new(CheckedForge {
            checks: status,
            merge_fails: false,
            merged: true,
            merge_called: AtomicBool::new(false),
        });
        let (store, card, mut rx) = merging(forge.clone(), false);

        wait_for(&mut rx, |e| match &e.kind {
            ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
                matches!(c.state, CardState::Done).then_some(())
            }
            ExecutorEventKind::ChecksFailed { .. } => {
                panic!("an already-merged PR must not be gated on its checks")
            }
            _ => None,
        })
        .await;

        assert!(matches!(
            store.get_card(card.id).unwrap().state,
            CardState::Done
        ));
    }
}

// --- fixing: an agent runs only while the checks are actually red ------------

/// Seed a `ReadyToMerge` card with an isolated worktree on disk (the launch
/// tripwire refuses to run a write agent without one) and ask to fix checks.
fn fixing(
    forge: Arc<CheckedForge>,
    worktree: &Path,
) -> (Store, Card, UnboundedReceiver<ExecutorEvent>) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-checks"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let mut card = ready_to_merge_card(&store, project.id);
    card.worktree_path = Some(worktree.to_path_buf());
    store.upsert_card(&card).unwrap();

    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge,
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::FixChecks { card_id: card.id });
    (store, card, rx)
}

/// The fix run goes through the same applying-fixes loop a post-PR change uses
/// — the run's completion commits + pushes (re-running CI) and lands the card
/// back on `ReadyToMerge`.
#[tokio::test]
async fn fixing_checks_hands_the_failures_to_an_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let forge = Arc::new(CheckedForge::new(CheckStatus::Failing));
    let (store, card, mut rx) = fixing(forge, tmp.path());

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => matches!(
            c.state,
            CardState::PrReview(usine_core::PrReviewSub::ApplyingFixes)
        )
        .then_some(()),
        _ => None,
    })
    .await;

    // The sim run completes: its push re-triggers CI, so the card must come
    // back to `ReadyToMerge` showing `Pending` — not the stale `Failing` the
    // fix set out to cure, which would re-offer the fix while CI re-runs.
    let checks = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == card.id && matches!(c.state, CardState::ReadyToMerge) =>
        {
            Some(c.checks)
        }
        _ => None,
    })
    .await;
    assert_eq!(checks, CheckStatus::Pending);
    assert_eq!(
        store.get_card(card.id).unwrap().checks,
        CheckStatus::Pending
    );
}

/// If the checks went green between the dialog and the click — a re-run, a
/// teammate's push — there is nothing to fix, and a run must not be spent.
#[tokio::test]
async fn checks_that_turned_green_cost_no_agent_run() {
    let tmp = tempfile::tempdir().unwrap();
    let forge = Arc::new(CheckedForge::new(CheckStatus::Passing));
    let (store, card, mut rx) = fixing(forge, tmp.path());

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } => Some(message.clone()),
        ExecutorEventKind::CardUpdated(c)
            if c.id == card.id && !matches!(c.state, CardState::ReadyToMerge) =>
        {
            panic!("green checks must not move the card: {:?}", c.state)
        }
        _ => None,
    })
    .await;

    assert!(msg.contains("No failing checks left"), "got: {msg}");
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::ReadyToMerge
    ));
}
