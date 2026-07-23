//! Auto-stop of the in-worktree preview when the automated pipeline parks,
//! driven through the real executor: a write run brings the app up
//! (`EnsurePreview`), and the finalizers light-stop it — process tree killed,
//! teardown script skipped — once the card settles in a parked state.
//!
//! The preview is a real `sh` process in a real temp worktree — only the
//! agents, git, and the forge are simulated.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, CoreError, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, PreviewStatus, Project, ProjectConfig,
    Provider, ProviderFactory, ReviewSub, RunConfig, RunHandle, SimFactory, SimForge, SimGit,
    Store,
};

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

/// Wait for the card's preview to report `want`.
async fn wait_for_preview(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
    want: PreviewStatus,
) {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated { status, .. }
            if e.card_id == card_id && *status == want =>
        {
            Some(())
        }
        _ => None,
    })
    .await
}

/// A store with one project (a long-lived `run_script` so the preview stays up
/// until killed) and one card in `state` with a branch and a REAL worktree dir
/// the preview runs in. `SimGit` is a no-op, so the pre-seeded dir is what
/// keeps `ensure_branch_worktree` (and the preview launch) on a real path.
fn seed(
    config: ProjectConfig,
    state: CardState,
    worktree: &std::path::Path,
) -> (Store, uuid::Uuid) {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        run_script: Some("while true; do sleep 1; done".into()),
        ..config
    };
    let project = Project::new("p", PathBuf::from("/tmp/usine-preview-reap-p"), config);
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = state;
    card.branch = Some("usine/card-x".into());
    card.worktree_path = Some(worktree.to_path_buf());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    (store, card_id)
}

fn executor(store: &Store) -> (usine_core::ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

#[tokio::test]
async fn implement_done_park_reaps_the_preview() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        ProjectConfig::default(),
        CardState::AwaitingReview(ReviewSub::ReadyForReview),
        wt.path(),
    );
    // Opt out of the auto self-review so the finished run parks at
    // `ReadyForReview` instead of continuing into the review pass.
    store.set_auto_review(card_id, false).unwrap();
    let (handle, mut rx) = executor(&store);

    // A revise runs the implement phase in the existing worktree; its
    // `EnsurePreview` brings the app up alongside the run.
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "polish it".into(),
    });

    wait_for_preview(&mut rx, card_id, PreviewStatus::Running).await;
    // The sim run's Done parks the card back at `ReadyForReview` — the
    // finalizer light-stops the preview.
    wait_for_preview(&mut rx, card_id, PreviewStatus::Stopped).await;
    let card = store.get_card(card_id).unwrap();
    assert!(
        matches!(
            card.state,
            CardState::AwaitingReview(ReviewSub::ReadyForReview)
        ),
        "card should be parked at ReadyForReview, got {:?}",
        card.state
    );
    // The preview-info file no longer describes a live app.
    assert!(
        !wt.path().join(".usine-preview.json").exists(),
        "the preview-info file should be cleared by the reap"
    );
}

#[tokio::test]
async fn validation_pass_reaps_light_without_teardown() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        ProjectConfig {
            validate_script: Some("exit 0".into()),
            worktree_teardown_script: Some("touch torn-down".into()),
            ..ProjectConfig::default()
        },
        CardState::AwaitingReview(ReviewSub::SelectingFixes {
            verdicts: Vec::new(),
        }),
        wt.path(),
    );
    let (handle, mut rx) = executor(&store);

    // The non-empty note launches a fix run, which brings the preview up; its
    // Done routes through the validation gate, whose pass parks the card.
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        selected_comment_ids: Vec::new(),
        note: "tweak".into(),
    });

    wait_for_preview(&mut rx, card_id, PreviewStatus::Running).await;
    wait_for_preview(&mut rx, card_id, PreviewStatus::Stopped).await;
    let card = store.get_card(card_id).unwrap();
    assert!(
        matches!(card.state, CardState::AwaitingReview(ReviewSub::ReadyForPr)),
        "card should be parked at ReadyForPr, got {:?}",
        card.state
    );
    // The light kill skips the teardown script — the worktree's infra stays up.
    assert!(
        !wt.path().join("torn-down").exists(),
        "the teardown script must not run on a parked-card reap"
    );
}

/// A provider whose `start` always fails — the "CLI isn't installed" case.
struct FailingProvider(Provider);

#[async_trait::async_trait]
impl AgentProvider for FailingProvider {
    fn provider(&self) -> Provider {
        self.0
    }

    async fn start(&self, _cfg: RunConfig) -> usine_core::Result<RunHandle> {
        Err(CoreError::other("the provider CLI is not installed"))
    }
}

struct FailingFactory;

impl ProviderFactory for FailingFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(FailingProvider(provider))
    }
}

/// A launch failure parks the card at `Failed` without ever entering
/// `run_actor`, so `launch` itself must reap: here a validation fix run's
/// provider fails to start while the mid-gate preview is still up.
#[tokio::test]
async fn failed_fix_launch_reaps_the_preview() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        ProjectConfig {
            validate_script: Some("exit 1".into()),
            ..ProjectConfig::default()
        },
        CardState::AwaitingReview(ReviewSub::ValidationFailed {
            attempt: 1,
            output: "NOPE".into(),
        }),
        wt.path(),
    );
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(FailingFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    // Bring the preview up first (the state a mid-gate card holds), then ask
    // for one more fix: the provider can't start, demoting the card to Failed.
    handle.send(ExecutorCommand::StartPreview { card_id });
    wait_for_preview(&mut rx, card_id, PreviewStatus::Running).await;
    handle.send(ExecutorCommand::FixValidation { card_id });

    wait_for_preview(&mut rx, card_id, PreviewStatus::Stopped).await;
    let card = store.get_card(card_id).unwrap();
    assert!(
        card.state.is_failed(),
        "card should be parked at Failed, got {:?}",
        card.state
    );
}

#[tokio::test]
async fn exhausted_validation_budget_reaps_the_preview() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        ProjectConfig {
            validate_script: Some("echo NOPE; exit 1".into()),
            ..ProjectConfig::default()
        },
        CardState::AwaitingReview(ReviewSub::SelectingFixes {
            verdicts: Vec::new(),
        }),
        wt.path(),
    );
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        selected_comment_ids: Vec::new(),
        note: "tweak".into(),
    });

    wait_for_preview(&mut rx, card_id, PreviewStatus::Running).await;
    // The intermediate fix runs find the preview claim already held (no
    // flicker); only the exhausted park emits Stopped.
    wait_for_preview(&mut rx, card_id, PreviewStatus::Stopped).await;
    let card = store.get_card(card_id).unwrap();
    assert!(
        matches!(
            card.state,
            CardState::AwaitingReview(ReviewSub::ValidationFailed { .. })
        ),
        "card should be parked at ValidationFailed, got {:?}",
        card.state
    );
}
