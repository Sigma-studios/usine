//! Proves the read-only runs (self-review, PR-comment triage) are handed the
//! card's background — the approved plan and the decisions/change requests the
//! user made after writing the description.
//!
//! Without it, those runs are a cold conversation whose only statement of intent
//! is the original description, so a deliberate mid-flight adjustment ("use a
//! channel, not a mutex") is indistinguishable from a deviation from the spec and
//! comes back as a review finding. A spying provider captures the exact prompt
//! each run receives.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, Project, ProjectConfig, Provider, ProviderFactory, Result,
    ReviewSub, RunConfig, RunHandle, RunMode, SimFactory, SimForge, SimGit, Store,
};

/// Every prompt handed to a provider, tagged with the run mode that asked for it.
type Prompts = Arc<Mutex<Vec<(RunMode, String)>>>;

/// Wraps the simulator, recording each run's full prompt before delegating.
struct SpyProvider {
    inner: Arc<dyn AgentProvider>,
    prompts: Prompts,
}

#[async_trait::async_trait]
impl AgentProvider for SpyProvider {
    fn provider(&self) -> Provider {
        self.inner.provider()
    }
    fn interactive(&self) -> bool {
        self.inner.interactive()
    }
    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        self.prompts
            .lock()
            .unwrap()
            .push((cfg.mode, cfg.full_prompt()));
        self.inner.start(cfg).await
    }
}

struct SpyFactory(Prompts);

impl ProviderFactory for SpyFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(SpyProvider {
            inner: SimFactory.make(provider),
            prompts: self.0.clone(),
        })
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

/// The prompt of the first run in `mode`.
fn prompt_for(prompts: &Prompts, mode: RunMode) -> String {
    prompts
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| *m == mode)
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| panic!("no {mode:?} run was launched"))
}

/// Await the card parking at the pre-PR review gate.
async fn wait_for_the_gate(rx: &mut UnboundedReceiver<ExecutorEvent>) {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(
                c.state,
                CardState::AwaitingReview(ReviewSub::ReadyForReview)
            ) =>
        {
            Some(())
        }
        _ => None,
    })
    .await
}

#[tokio::test]
async fn the_self_review_sees_the_plan_and_the_users_mid_flight_changes() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/review-context"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();

    // The description says mutex; the plan and the user's later change request say
    // channel. The committed code will follow the latter.
    let card = Card::new(
        project.id,
        "c",
        "Guard the cache with a mutex.",
        CardConfig::default(),
    );
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store
        .save_plan(card_id, "Step 1: guard the cache.")
        .unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(prompts.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    // Implement, then ask for a change from the awaiting-review gate.
    handle.send(ExecutorCommand::Start { card_id });
    wait_for_the_gate(&mut rx).await;

    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "Use a channel instead of a mutex.".into(),
    });
    // Back through implementing, and out at the gate again.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if matches!(c.state, CardState::Implementing(_)) => {
            Some(())
        }
        _ => None,
    })
    .await;
    wait_for_the_gate(&mut rx).await;

    handle.send(ExecutorCommand::SelfReview { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. }) => Some(()),
            _ => None,
        },
        _ => None,
    })
    .await;

    let review = prompt_for(&prompts, RunMode::Review);
    // The original description still anchors the review…
    assert!(review.contains("Guard the cache with a mutex."));
    // …but the change the user asked for afterwards is there too, framed as
    // context so the channel-vs-mutex divergence isn't reported as a finding.
    assert!(
        review.contains("Use a channel instead of a mutex."),
        "the self-review must see the user's change request:\n{review}"
    );
    assert!(
        review.contains("Step 1: guard the cache."),
        "and the approved plan"
    );
    assert!(
        review.contains("do NOT report"),
        "framed as context, not spec"
    );

    // The implement run is handed the feedback directly and needs no background.
    let implement = prompt_for(&prompts, RunMode::Implement);
    assert!(!implement.contains("do NOT report"));
}

#[tokio::test]
async fn a_do_over_drops_the_plan_so_it_cant_leak_into_the_next_review() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/review-context-reset"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store
        .save_plan(card_id, "STALE PLAN from the first attempt")
        .unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(prompts.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    handle.send(ExecutorCommand::Start { card_id });
    wait_for_the_gate(&mut rx).await;

    // A genuine do-over discards the attempt — and its plan with it.
    handle.send(ExecutorCommand::BackToStart { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if matches!(c.state, CardState::StartingBlock) => {
            Some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(
        store.get_plan(card_id).unwrap(),
        None,
        "back to start must drop the discarded attempt's plan"
    );

    // Re-run with planning skipped: there's no plan now, so none reaches the review.
    handle.send(ExecutorCommand::Start { card_id });
    wait_for_the_gate(&mut rx).await;
    handle.send(ExecutorCommand::SelfReview { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. }) => Some(()),
            _ => None,
        },
        _ => None,
    })
    .await;

    let review = prompt_for(&prompts, RunMode::Review);
    assert!(
        !review.contains("STALE PLAN"),
        "the discarded attempt's plan must not reach the review:\n{review}"
    );
}
