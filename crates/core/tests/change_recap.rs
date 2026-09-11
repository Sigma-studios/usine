//! "Request changes" reports under its own request, in the Agent Chat log,
//! and never overwrites what was there before: the implement run's hand-off
//! keeps describing the task as a whole, and on an open PR the merge gate's
//! fixes recap stays put.
//!
//! Before this, a change run was a plain re-implement: it was asked for a new
//! `usine-handoff` block and `finalize_run` stored it over the original, so
//! the reviewer lost the recap of the work they had just read.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentEvent, AgentProvider, Card, CardAnswers, CardConfig, CardState,
    ExchangeKind, ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, PrInfo,
    PrReviewSub, Project, ProjectConfig, Provider, ProviderFactory, Result, ReviewSub, RunConfig,
    RunHandle, RunMode, SimFactory, SimForge, SimGit, Store,
};

/// Every prompt handed to a provider, tagged with the run mode that asked for it.
type Prompts = Arc<Mutex<Vec<(RunMode, String)>>>;

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

async fn wait_for_state(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
    want: impl Fn(&CardState) -> bool,
) {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card_id && want(&c.state) => Some(()),
        _ => None,
    })
    .await
}

/// The log carried by the first `AnswersUpdated` whose newest entry is a change.
async fn wait_for_change_entry(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
) -> CardAnswers {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { answers }
            if e.card_id == card_id
                && answers
                    .exchanges
                    .last()
                    .is_some_and(|x| x.kind == ExchangeKind::Change) =>
        {
            Some(answers.clone())
        }
        _ => None,
    })
    .await
}

fn project(store: &Store, dir: &str) -> Project {
    let project = Project::new("p", PathBuf::from(dir), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    project
}

#[tokio::test]
async fn a_requested_change_gets_its_own_recap_and_keeps_the_hand_off() {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, "/tmp/change-recap-revise");
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();
    // Park at the manual gate, so the revise below can't race the auto
    // self-review's in-flight claim.
    store.set_auto_review(card_id, false).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(prompts.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    handle.send(ExecutorCommand::Start { card_id });
    let at_gate = |s: &CardState| matches!(s, CardState::AwaitingReview(ReviewSub::ReadyForReview));
    wait_for_state(&mut rx, card_id, at_gate).await;
    let original = store
        .get_handoff(card_id)
        .unwrap()
        .expect("the first run handed off");

    let feedback = "Rename the button to Save.";
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: format!("  {feedback}\n"),
    });

    let log = wait_for_change_entry(&mut rx, card_id).await;
    let entry = log.exchanges.last().unwrap();
    assert_eq!(entry.question, feedback, "the request, trimmed");
    assert!(
        !entry.answer.is_empty(),
        "the run's recap is the entry's body"
    );
    assert!(
        !entry.answer.contains("usine-handoff"),
        "machine-facing blocks are stripped: {}",
        entry.answer
    );
    assert!(!log.superseded, "the recap describes the current work");
    wait_for_state(&mut rx, card_id, at_gate).await;

    assert_eq!(
        store.get_handoff(card_id).unwrap(),
        Some(original),
        "the change run must not replace the original hand-off"
    );
    assert_eq!(store.get_pending_change(card_id).unwrap(), None);

    let implement: Vec<String> = prompts
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| *m == RunMode::Implement)
        .map(|(_, p)| p.clone())
        .collect();
    assert_eq!(implement.len(), 2, "the first run, then the change run");
    assert!(implement[0].contains("usine-handoff"));
    assert!(
        !implement[1].contains("usine-handoff"),
        "a change run is not asked for a hand-off:\n{}",
        implement[1]
    );
    assert!(implement[1].contains(usine_core::CHANGE_RECAP_INSTRUCTION));
    assert!(implement[1].contains(feedback));
}

#[tokio::test]
async fn a_change_on_an_open_pr_leaves_the_fixes_recap_alone() {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, "/tmp/change-recap-post-pr");
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::ReadyToMerge;
    card.branch = Some("feat/thing".into());
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
    store.set_review_recap(card_id, "old recap").unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(ScriptedFactory::default()),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::RequestPostPrChange {
        card_id,
        feedback: "tweak the wording".into(),
    });

    let log = wait_for_change_entry(&mut rx, card_id).await;
    let entry = log.exchanges.last().unwrap();
    assert_eq!(entry.question, "tweak the wording");
    assert_eq!(entry.answer, "Reworded the paragraph.");
    wait_for_state(&mut rx, card_id, |s| {
        matches!(
            s,
            CardState::ReadyToMerge | CardState::PrReview(PrReviewSub::Idle)
        )
    })
    .await;
    assert_eq!(
        store.get_review_recap(card_id).unwrap().as_deref(),
        Some("old recap"),
        "the merge gate's fixes recap is not the change's to overwrite"
    );
    assert_eq!(store.get_pending_change(card_id).unwrap(), None);
}

#[tokio::test]
async fn cancelling_a_change_run_drops_its_request() {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, "/tmp/change-recap-cancel");
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
    card.branch = Some("usine/thing".into());
    card.worktree_path = Some(PathBuf::from("/tmp/change-recap-cancel-wt"));
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let factory = ScriptedFactory {
        hang: true,
        ..Default::default()
    };
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(factory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "make it blue".into(),
    });
    // The run has really started once `launch` supersedes the log — the
    // `Implementing` update itself lands before the request is stashed.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { .. } if e.card_id == card_id => Some(()),
        _ => None,
    })
    .await;
    assert_eq!(
        store.get_pending_change(card_id).unwrap().as_deref(),
        Some("make it blue")
    );

    handle.send(ExecutorCommand::Cancel { card_id });
    wait_for_state(&mut rx, card_id, |s| {
        !matches!(s, CardState::Implementing(_))
    })
    .await;
    // The request is dropped only after the cancel transition lands (which
    // is what emits the state update above), so give it a beat.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while store.get_pending_change(card_id).unwrap().is_some()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.get_pending_change(card_id).unwrap(),
        None,
        "a stale request must not turn a later run into a change run"
    );
    // Stop steps back to the review gate it was asked from — not the starting
    // block, whose next Start would re-cut the branch over the committed work.
    let card = store.get_card(card_id).unwrap();
    assert_eq!(
        card.state,
        CardState::AwaitingReview(ReviewSub::ReadyForReview)
    );
    assert_eq!(card.branch.as_deref(), Some("usine/thing"));
    assert!(card.worktree_path.is_some());
}

/// Stopping the run an approval started steps back to the plan, and undoes
/// what the approval set up, so approving again starts from a clean cut.
#[tokio::test]
async fn stopping_an_approved_plan_run_returns_to_the_plan() {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, "/tmp/change-recap-approve-stop");
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let plan = CardState::Designing(usine_core::DesignSub::AwaitingApproval {
        plan: "1. Paint it blue.".into(),
    });
    card.state = plan.clone();
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let factory = ScriptedFactory {
        hang: true,
        ..Default::default()
    };
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(factory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::ApprovePlan { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { .. } if e.card_id == card_id => Some(()),
        _ => None,
    })
    .await;
    let running = store.get_card(card_id).unwrap();
    assert!(running.worktree_path.is_some() && running.branch.is_some());
    assert!(store.get_plan(card_id).unwrap().is_some());

    handle.send(ExecutorCommand::Cancel { card_id });
    wait_for_state(&mut rx, card_id, |s| *s == plan).await;
    // The approval's artifacts are undone just after the state lands.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while store.get_card(card_id).unwrap().worktree_path.is_some()
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let card = store.get_card(card_id).unwrap();
    assert_eq!(card.state, plan);
    assert_eq!(card.worktree_path, None);
    assert_eq!(card.branch, None);
    assert_eq!(store.get_plan(card_id).unwrap(), None);
}

/// An agent that finishes with prose and no blocks at all — or, with `hang`,
/// starts and never finishes (its event sender is kept alive).
#[derive(Default)]
struct ScriptedFactory {
    hang: bool,
    held: Arc<Mutex<Vec<UnboundedSender<AgentEvent>>>>,
}

struct ScriptedProvider {
    hang: bool,
    held: Arc<Mutex<Vec<UnboundedSender<AgentEvent>>>>,
}

#[async_trait::async_trait]
impl AgentProvider for ScriptedProvider {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn interactive(&self) -> bool {
        false
    }
    async fn start(&self, _cfg: RunConfig) -> Result<RunHandle> {
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, mut ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        if self.hang {
            self.held.lock().unwrap().push(evt_tx);
            // Like a real CLI killed on Stop, the stream ends once the run is
            // cancelled — Stop's discard waits for exactly that.
            let held = self.held.clone();
            tokio::spawn(async move {
                let _ = ctl_rx.next().await;
                held.lock().unwrap().clear();
            });
        } else {
            let _ = evt_tx.unbounded_send(AgentEvent::Done {
                result: "Reworded the paragraph.".into(),
                cost_usd: 0.0,
                usage: usine_core::Usage::default(),
            });
        }
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

impl ProviderFactory for ScriptedFactory {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(ScriptedProvider {
            hang: self.hang,
            held: self.held.clone(),
        })
    }
}
