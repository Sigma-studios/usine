//! The abandoned-plan guard measures plan PROSE, not payload.
//!
//! A plan run that bails mid-thought ("I'll wait for the findings") stays
//! retryable instead of becoming an approvable plan. Once plans carry a
//! machine-facing `usine-plan` outline block, a short bail that still emits the
//! block would clear the length floor on JSON alone — so the floor is applied
//! to the prose the user would actually read.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, DesignSub, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Project, ProjectConfig, Provider,
    ProviderFactory, RunConfig, RunHandle, SimForge, SimGit, Store,
};

/// An agent whose plan run ends on one scripted result.
struct ScriptedProvider(String);

#[async_trait::async_trait]
impl AgentProvider for ScriptedProvider {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn interactive(&self) -> bool {
        false
    }
    async fn start(&self, _cfg: RunConfig) -> usine_core::Result<RunHandle> {
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, _ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(usine_core::AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        let _ = evt_tx.unbounded_send(usine_core::AgentEvent::Done {
            result: self.0.clone(),
            cost_usd: 0.0,
            usage: usine_core::Usage::default(),
        });
        drop(evt_tx);
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

struct ScriptedFactory(String);

impl ProviderFactory for ScriptedFactory {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(ScriptedProvider(self.0.clone()))
    }
}

async fn plan_state(result: &str) -> CardState {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-plan-floor"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(ScriptedFactory(result.to_string())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::Start { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card_id => {
            (!matches!(c.state, CardState::Designing(DesignSub::Running))).then(|| c.state.clone())
        }
        _ => None,
    })
    .await
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

/// The outline block is machine-facing padding as far as the floor is
/// concerned: a bail that carries one is still a bail.
#[tokio::test]
async fn an_outline_block_does_not_pad_a_bailed_plan_run_over_the_floor() {
    let result = "I'll wait for the findings.\n\n```usine-plan\n{\"v\":1,\"tldr\":[\"Something long enough to clear two hundred characters on its own, which is exactly the failure this pins: JSON is not plan prose.\"],\"files\":[\"crates/core/src/agent/plan.rs\",\"crates/core/src/agent/executor/actor.rs\",\"crates/app/src/ui/detail/plan.rs\"]}\n```\n";
    assert!(
        result.trim().chars().count() > 200,
        "the raw result clears the floor"
    );
    let state = plan_state(result).await;
    assert!(
        matches!(state, CardState::Failed { .. }),
        "a bailed plan run must stay retryable, got {state:?}"
    );
}

/// The other side of the line: real plan prose is promoted as before.
#[tokio::test]
async fn a_real_plan_is_promoted_for_approval() {
    let prose = "## Plan\n\nRework the fix report so the checklist follows the picker rows, then \
                 render it at both gates. This is long enough to be a plan, and reads as one: it \
                 names the files, the order, and how it is checked afterwards.\n";
    let state = plan_state(prose).await;
    assert!(
        matches!(
            state,
            CardState::Designing(DesignSub::AwaitingApproval { .. })
        ),
        "got {state:?}"
    );
}
