//! Proves the investigation lifecycle end to end through the executor: an
//! "investigate only" card starts a read-only run in the main checkout (no
//! worktree, no branch), parks on its conclusion, takes a follow-up round that
//! carries the prior conclusion as context, and converts — in place — into an
//! implementation whose description holds the findings while keeping the cost.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardKind, CardState, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Project, ProjectConfig, Provider,
    ProviderFactory, Result, RunConfig, RunHandle, RunMode, SimFactory, SimForge, SimGit, Store,
};

/// Every run config handed to a provider: `(mode, workdir, full prompt)`.
type Runs = Arc<Mutex<Vec<(RunMode, PathBuf, String)>>>;

struct SpyProvider {
    inner: Arc<dyn AgentProvider>,
    runs: Runs,
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
        self.runs.lock().unwrap().push((
            cfg.mode,
            cfg.project_dir.clone(),
            cfg.full_prompt(),
        ));
        self.inner.start(cfg).await
    }
}

struct SpyFactory(Runs);

impl ProviderFactory for SpyFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(SpyProvider {
            inner: SimFactory.make(provider),
            runs: self.0.clone(),
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

/// Wait for a `CardUpdated` whose state satisfies `pred`, returning the card.
async fn wait_for_state(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    pred: impl Fn(&CardState) -> bool,
) -> Card {
    wait_for(rx, |evt| match &evt.kind {
        ExecutorEventKind::CardUpdated(card) if pred(&card.state) => Some((**card).clone()),
        _ => None,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn investigation_concludes_follows_up_and_converts_in_place() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
    store.upsert_project(&project).unwrap();

    let mut config = CardConfig::default();
    config.kind = CardKind::Investigation;
    let card = Card::new(project.id, "Audit the cache", "Is the cache bounded?", config);
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let runs: Runs = Arc::new(Mutex::new(Vec::new()));
    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(runs.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    // Start → a read-only investigate run in the MAIN checkout, then Concluded.
    exec.send(ExecutorCommand::Start { card_id });
    let card = wait_for_state(&mut rx, |s| matches!(s, CardState::Concluded { .. })).await;
    assert!(card.needs_attention(), "the conclusion drives the badge");
    assert!(card.worktree_path.is_none(), "no worktree for investigations");
    assert!(card.branch.is_none(), "no branch either");
    assert!(!card.cost.is_zero(), "the run's cost lands on the card");
    let first_cost = card.cost;
    {
        let runs = runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        let (mode, dir, prompt) = &runs[0];
        assert_eq!(*mode, RunMode::Investigate);
        assert_eq!(dir, &project.path, "runs in the main checkout");
        assert!(
            prompt.contains("READ-ONLY investigation"),
            "the investigate instruction rides in the prompt"
        );
        assert!(prompt.contains("Is the cache bounded?"));
    }
    let first_conclusion = match &card.state {
        CardState::Concluded { conclusion } => conclusion.clone(),
        _ => unreachable!(),
    };

    // Follow-up → re-runs with the prior conclusion in context, re-concludes.
    exec.send(ExecutorCommand::FollowUpInvestigation {
        card_id,
        feedback: "How big does it get under real traffic?".into(),
    });
    wait_for_state(&mut rx, |s| {
        matches!(s, CardState::Investigating(usine_core::RunSub::Running))
    })
    .await;
    let card = wait_for_state(&mut rx, |s| matches!(s, CardState::Concluded { .. })).await;
    assert!(card.cost > first_cost, "follow-up spend accumulates");
    {
        let runs = runs.lock().unwrap();
        assert_eq!(runs.len(), 2);
        let (mode, _, prompt) = &runs[1];
        assert_eq!(*mode, RunMode::Investigate);
        assert!(
            prompt.contains(first_conclusion.trim()),
            "the prior conclusion rides along on the follow-up"
        );
        assert!(prompt.contains("How big does it get under real traffic?"));
    }

    // Convert → same card, back in the starting block as a Task, findings in
    // the description, follow-up folded as context, cost kept, session cleared.
    exec.send(ExecutorCommand::ConvertToImplementation { card_id });
    let card = wait_for_state(&mut rx, |s| matches!(s, CardState::StartingBlock)).await;
    assert_eq!(card.config.kind, CardKind::Task);
    assert!(card.description.contains("## Findings (from investigation)"));
    assert!(card.description.contains("How big does it get under real traffic?"));
    assert!(card.qa_log.is_empty(), "the folded Q&A log is cleared");
    assert!(!card.cost.is_zero(), "investigation spend stays on the card");
    assert!(card.last_session.is_none(), "the next run starts fresh");
    // The conversion also resets the skip-plan flag to the default mode.
    assert!(!store.get_skip_plan(card_id).unwrap());

    // Exclusive commands sent while another holds the card are dropped as
    // duplicates (the dispatcher's double-click guard), and each releases its
    // claim (`busy: false`) only *after* its `CardUpdated` echo — so drain the
    // first convert's release before sending the next command.
    async fn wait_released(rx: &mut UnboundedReceiver<ExecutorEvent>) {
        wait_for(rx, |evt| match &evt.kind {
            ExecutorEventKind::CardBusy { busy: false } => Some(()),
            _ => None,
        })
        .await
    }
    wait_released(&mut rx).await;

    // Converting a card that isn't concluded is a silent no-op (double-click).
    exec.send(ExecutorCommand::ConvertToImplementation { card_id });
    wait_released(&mut rx).await;
    // Prove it did nothing by driving the next lifecycle step successfully:
    // the card is a Task now, so Start enters the design phase.
    exec.send(ExecutorCommand::Start { card_id });
    wait_for_state(&mut rx, |s| {
        matches!(s, CardState::Designing(usine_core::DesignSub::Running))
    })
    .await;
    assert_eq!(runs.lock().unwrap().last().unwrap().0, RunMode::Plan);
}
