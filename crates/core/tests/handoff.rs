//! Proves the implement run hands off to the human who reviews it: the prompt
//! asks for the recap, the finished run's `usine-handoff` block is parsed and
//! stored, and the card arrives at the awaiting-review gate with it in hand.
//!
//! The gate is where the user decides whether to self-review, revise, or open a
//! PR, and until now they arrived there blind — no idea what the agent built,
//! what it was unsure about, or what to click. A spying provider captures the
//! exact prompt each run receives.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, Handoff, Project, ProjectConfig, Provider, ProviderFactory,
    Result, ReviewSub, RunConfig, RunHandle, RunMode, SimFactory, SimForge, SimGit, Store,
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

/// The prompt of the first run in `mode`, or `None` if no such run was launched.
fn prompt_for(prompts: &Prompts, mode: RunMode) -> Option<String> {
    prompts
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| *m == mode)
        .map(|(_, p)| p.clone())
}

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

/// A store + executor with a spying provider, and a card set to skip planning.
fn setup(
    dir: &str,
) -> (
    Store,
    Prompts,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
    uuid::Uuid,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from(dir), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(prompts.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    (store, prompts, handle, rx, card_id)
}

#[tokio::test]
async fn an_implement_run_hands_off_its_recap_before_the_card_reaches_the_gate() {
    let (store, prompts, handle, mut rx, card_id) = setup("/tmp/handoff-implement");

    handle.send(ExecutorCommand::Start { card_id });

    // The hand-off lands *before* the gate: the user never sees a bare panel.
    let handoff = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::HandoffUpdated { handoff } if e.card_id == card_id => {
            Some(handoff.clone())
        }
        // The card must not reach the gate first.
        ExecutorEventKind::CardUpdated(c) if matches!(c.state, CardState::AwaitingReview(_)) => {
            panic!("the card reached the review gate before its hand-off arrived")
        }
        _ => None,
    })
    .await;
    wait_for_the_gate(&mut rx).await;

    assert!(!handoff.summary.is_empty(), "the recap carries a summary");
    assert!(
        !handoff.questions.is_empty(),
        "and the agent's open questions"
    );
    assert!(
        !handoff.tests.is_empty(),
        "and what's worth testing by hand"
    );
    assert_eq!(
        store.all_handoffs().unwrap().get(&card_id),
        Some(&handoff),
        "and it's persisted, so it survives a restart at the gate"
    );

    // The implement run was asked for it; the block never reaches the transcript.
    let implement = prompt_for(&prompts, RunMode::Implement).expect("an implement run");
    assert!(
        implement.contains("usine-handoff"),
        "the implement prompt must ask for a hand-off:\n{implement}"
    );
    let transcript = store.load_transcript(card_id).unwrap();
    assert!(
        transcript
            .iter()
            .all(|line| !line.contains("usine-handoff")),
        "the machine-facing block is stripped from the transcript: {transcript:?}"
    );
}

#[tokio::test]
async fn only_implement_runs_are_asked_for_a_hand_off() {
    let (_store, prompts, handle, mut rx, card_id) = setup("/tmp/handoff-modes");

    handle.send(ExecutorCommand::Start { card_id });

    // The self-review auto-starts on implement-done; apply its fixes — a write
    // run that reports through the fixes recap instead, and so must not be
    // asked for a hand-off of its own.
    let mut verdicts = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) => {
                Some(verdicts.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    for v in &mut verdicts {
        v.selected = true;
    }
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts,
        note: String::new(),
        prompt: None,
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::AwaitingReview(ReviewSub::ReadyForPr)) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    let fixes = prompt_for(&prompts, RunMode::ApplyFixes).expect("a fix run");
    assert!(
        !fixes.contains("usine-handoff"),
        "a fix run must not be asked for a hand-off:\n{fixes}"
    );
    // …but it is asked to open its recap with a TL;DR.
    assert!(
        fixes.contains("TL;DR"),
        "a fix run is asked for a TL;DR-led recap:\n{fixes}"
    );
    // Both write runs still author their own commit message.
    assert!(fixes.contains("usine-commit"));
    let implement = prompt_for(&prompts, RunMode::Implement).expect("an implement run");
    assert!(implement.contains("usine-commit"));
    assert!(
        implement.contains("usine-handoff") && implement.contains("TL;DR"),
        "the implement prompt asks for a hand-off whose summary leads with a TL;DR"
    );

    let review = prompt_for(&prompts, RunMode::Review).expect("a self-review run");
    assert!(
        !review.contains("usine-handoff"),
        "nor a read-only review run"
    );
}

#[tokio::test]
async fn a_do_over_drops_the_discarded_attempts_hand_off() {
    let (store, _prompts, handle, mut rx, card_id) = setup("/tmp/handoff-reset");
    // Park at the manual gate: with the auto self-review on, its in-flight claim
    // races the `BackToStart` below and can silently drop it (the real UI holds
    // the buttons disabled through that window via `CardBusy`).
    store.set_auto_review(card_id, false).unwrap();

    handle.send(ExecutorCommand::Start { card_id });
    wait_for_the_gate(&mut rx).await;
    assert!(store.all_handoffs().unwrap().contains_key(&card_id));

    // The recap describes the attempt being thrown away — it must not survive to
    // describe the next one.
    handle.send(ExecutorCommand::BackToStart { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if matches!(c.state, CardState::StartingBlock) => {
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(
        !store.all_handoffs().unwrap().contains_key(&card_id),
        "back to start must drop the discarded attempt's hand-off"
    );
}

#[test]
fn storing_an_empty_hand_off_clears_the_previous_one() {
    // A re-implement whose agent forgot the block clears the field rather than
    // leaving the last attempt's recap to describe this one.
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/handoff-clear"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "d", CardConfig::default());
    store.upsert_card(&card).unwrap();

    let handoff = Handoff {
        summary: "Built it.".into(),
        questions: vec![],
        tests: vec!["Click the button".into()],
        ..Default::default()
    };
    store.set_handoff(card.id, &handoff).unwrap();
    assert_eq!(store.all_handoffs().unwrap().get(&card.id), Some(&handoff));

    store.set_handoff(card.id, &Handoff::default()).unwrap();
    assert!(!store.all_handoffs().unwrap().contains_key(&card.id));
}

#[test]
fn a_v1_hand_off_already_on_disk_still_reads_back() {
    // Records written before the v2 schema hold `tests` as bare strings and no
    // `v`/`changes`/`risks` — they must survive the upgrade, not vanish from the
    // panel.
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/handoff-v1"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "d", CardConfig::default());
    store.upsert_card(&card).unwrap();

    let v1: Handoff = serde_json::from_str(
        r#"{"summary":"Built it.","questions":["Why?"],"tests":["[verified] Click the button"]}"#,
    )
    .expect("a v1 payload deserializes");
    store.set_handoff(card.id, &v1).unwrap();

    let back = store.get_handoff(card.id).unwrap().expect("stored");
    assert_eq!(back.v, 1);
    assert_eq!(back.summary, "Built it.");
    assert_eq!(back.tests[0].scenario, "Click the button");
    assert!(back.tests[0].verified, "the v1 prefix became a field");
    assert!(back.changes.is_empty() && back.risks.is_empty());
}
