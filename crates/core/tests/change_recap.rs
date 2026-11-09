//! "Request changes" reports under its own request, in the Agent Chat log,
//! and never overwrites what was there before: the implement run's hand-off
//! keeps describing the task as a whole, and on an open PR the merge gate's
//! fixes recap stays put.
//!
//! Before this, a change run was a plain re-implement: it was asked for a new
//! `usine-handoff` block and `finalize_run` stored it over the original, so
//! the reviewer lost the recap of the work they had just read.
//!
//! The exceptions are the hand-off's "Test it" checklist, risks and open
//! questions, which a change can falsify: the change run is shown them and may
//! send back any of them in a `usine-handoff-revision` block, which replaces
//! only the lists it carries. Fix runs (self-review fixes here) get the same
//! chance.

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
async fn a_requested_change_gets_its_own_recap_and_keeps_the_hand_off_unless_it_revises_the_tests()
{
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
        Some(original.clone()),
        "a change run that sends no revised checklist leaves the hand-off as it was"
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
        !implement[1].contains("`usine-handoff`"),
        "a change run is not asked for a hand-off:\n{}",
        implement[1]
    );
    assert!(implement[1].contains("usine-handoff-revision"));
    assert!(implement[1].contains("Current risks:"));
    let scenario = &original
        .tests
        .first()
        .expect("the sim hands off tests")
        .scenario;
    assert!(
        implement[1].contains(scenario.as_str()),
        "the change run is shown the current checklist:\n{}",
        implement[1]
    );
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
    let original = handoff_with_tests();
    store.set_handoff(card_id, &original).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(ScriptedFactory {
            result: Some(REVISED_REPLY.into()),
            ..Default::default()
        }),
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
    assert_eq!(
        store.get_handoff(card_id).unwrap().unwrap().tests,
        vec![usine_core::TestItem::from("Click Save")],
        "the revised checklist replaces the old one"
    );
    assert_eq!(
        store.get_handoff(card_id).unwrap().unwrap().risks,
        vec!["The button label changed".to_string()],
        "the revised risks replace the old ones"
    );
    assert_eq!(store.get_pending_change(card_id).unwrap(), None);
}

/// A reply that revises the checklist, risks and questions alongside its recap.
const REVISED_REPLY: &str = "Reworded the paragraph.\n\n```usine-handoff-revision\n\
{\"tests\": [\"Click Save\"], \"risks\": [\"The button label changed\"], \"questions\": []}\n```";

/// A reply that revises only the risks.
const RISKS_ONLY_REPLY: &str = "Reworded the paragraph.\n\n```usine-handoff-revision\n\
{\"risks\": [\"Needs a migration\"]}\n```";

/// A reply whose revision block is garbled (a misspelled key).
const GARBLED_REPLY: &str =
    "Reworded the paragraph.\n\n```usine-handoff-revision\n{\"risk\": [\"x\"]}\n```";

fn handoff_with_tests() -> usine_core::Handoff {
    usine_core::Handoff {
        summary: "TL;DR: added the form.".into(),
        changes: vec![usine_core::Change {
            path: "src/form.rs".into(),
            what: "new form".into(),
            kind: "feat".into(),
        }],
        tests: vec![
            usine_core::TestItem::from("Click Submit"),
            usine_core::TestItem::from("[verified] Reload the form"),
        ],
        risks: vec!["no migration".into()],
        questions: vec!["Keep the old label?".into()],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_change_that_revises_the_tests_replaces_only_the_checklist() {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, "/tmp/change-recap-tests");
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
    card.branch = Some("usine/thing".into());
    card.worktree_path = Some(PathBuf::from("/tmp/change-recap-tests-wt"));
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_auto_review(card_id, false).unwrap();
    let original = handoff_with_tests();
    store.set_handoff(card_id, &original).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(ScriptedFactory {
            result: Some(REVISED_REPLY.into()),
            ..Default::default()
        }),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "Rename the button to Save.".into(),
    });

    let log = wait_for_change_entry(&mut rx, card_id).await;
    assert_eq!(
        log.exchanges.last().unwrap().answer,
        "Reworded the paragraph.",
        "the block is stripped from the Agent Chat recap"
    );
    let revised = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::HandoffUpdated { handoff } if e.card_id == card_id => {
            Some(handoff.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(
        revised.tests,
        vec![usine_core::TestItem::from("Click Save")]
    );
    assert_eq!(revised.risks, vec!["The button label changed"]);
    assert!(revised.questions.is_empty(), "`[]` wipes the questions");
    assert_eq!(
        usine_core::Handoff {
            tests: original.tests.clone(),
            risks: original.risks.clone(),
            questions: original.questions.clone(),
            ..revised.clone()
        },
        original,
        "summary and changes are the original hand-off's"
    );
    assert_eq!(store.get_handoff(card_id).unwrap(), Some(revised));
}

/// Park a card at the pre-PR gate with `handoff`, run one change on it whose
/// reply is `reply`, and return the hand-off it emitted.
async fn revise_once(dir: &str, handoff: &usine_core::Handoff, reply: &str) -> usine_core::Handoff {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, dir);
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
    card.branch = Some("usine/thing".into());
    card.worktree_path = Some(PathBuf::from(format!("{dir}-wt")));
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_auto_review(card_id, false).unwrap();
    store.set_handoff(card_id, handoff).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(ScriptedFactory {
            result: Some(reply.into()),
            ..Default::default()
        }),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "Change it.".into(),
    });
    let revised = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::HandoffUpdated { handoff } if e.card_id == card_id => {
            Some(handoff.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(store.get_handoff(card_id).unwrap(), Some(revised.clone()));
    revised
}

#[tokio::test]
async fn a_risks_only_revision_keeps_the_checklist_and_its_verified_marks() {
    let original = handoff_with_tests();
    let revised = revise_once("/tmp/change-recap-risks", &original, RISKS_ONLY_REPLY).await;
    assert_eq!(revised.risks, vec!["Needs a migration"]);
    assert_eq!(revised.tests, original.tests, "tests not re-sent stay as they were");
    assert!(revised.tests[1].verified, "…verified mark included");
    assert_eq!(revised.questions, original.questions);
}

#[tokio::test]
async fn a_garbled_revision_flags_the_note_and_a_good_one_clears_the_flag() {
    let original = handoff_with_tests();
    let flagged = revise_once("/tmp/change-recap-garbled", &original, GARBLED_REPLY).await;
    assert!(flagged.revision_malformed);
    assert_eq!(
        usine_core::Handoff {
            revision_malformed: false,
            ..flagged.clone()
        },
        original,
        "the stored lists are kept as they were"
    );

    let cleared = revise_once("/tmp/change-recap-garbled-2", &flagged, RISKS_ONLY_REPLY).await;
    assert!(!cleared.revision_malformed);
    assert_eq!(cleared.risks, vec!["Needs a migration"]);
}

/// Delegates to the sim for every run except a fix run, which ends with
/// `fix_reply` — and records every prompt.
struct FixScriptFactory {
    fix_reply: String,
    prompts: Prompts,
}

struct FixScriptProvider {
    inner: Arc<dyn AgentProvider>,
    fix_reply: String,
    prompts: Prompts,
}

#[async_trait::async_trait]
impl AgentProvider for FixScriptProvider {
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
        if cfg.mode != RunMode::ApplyFixes {
            return self.inner.start(cfg).await;
        }
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, _ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(AgentEvent::Started {
            session_id: "sess-fix".into(),
        });
        let _ = evt_tx.unbounded_send(AgentEvent::Done {
            result: self.fix_reply.clone(),
            cost_usd: 0.0,
            usage: usine_core::Usage::default(),
        });
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

impl ProviderFactory for FixScriptFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(FixScriptProvider {
            inner: SimFactory.make(provider),
            fix_reply: self.fix_reply.clone(),
            prompts: self.prompts.clone(),
        })
    }
}

/// Drive a card to the self-review picker over the sim, with `handoff`
/// standing in for whatever the implement run handed off, then apply every
/// finding. Returns the store, event stream and recorded prompts.
async fn run_self_fixes(
    dir: &str,
    handoff: Option<usine_core::Handoff>,
    fix_reply: &str,
) -> (Store, uuid::Uuid, UnboundedReceiver<ExecutorEvent>, Prompts) {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, dir);
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(FixScriptFactory {
            fix_reply: fix_reply.into(),
            prompts: prompts.clone(),
        }),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::Start { card_id });
    let verdicts = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card_id => match &c.state {
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) => {
                Some(verdicts.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    match handoff {
        Some(h) => store.set_handoff(card_id, &h).unwrap(),
        None => store.set_handoff(card_id, &usine_core::Handoff::default()).unwrap(),
    }
    let picked = verdicts
        .into_iter()
        .map(|mut v| {
            v.selected = true;
            v
        })
        .collect();
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts: picked,
        note: String::new(),
        prompt: None,
    });
    (store, card_id, rx, prompts)
}

fn fix_prompt(prompts: &Prompts) -> String {
    prompts
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| *m == RunMode::ApplyFixes)
        .map(|(_, p)| p.clone())
        .expect("a fix run was launched")
}

#[tokio::test]
async fn a_self_review_fix_run_can_revise_the_hand_off_and_refreshes_the_diff() {
    let original = handoff_with_tests();
    let reply = "TL;DR: fixed it.\n\n```usine-fixes\n{\"outcomes\": []}\n```\n\n\
```usine-handoff-revision\n{\"risks\": [\"Needs a migration\"]}\n```";
    let (store, card_id, mut rx, prompts) =
        run_self_fixes("/tmp/change-recap-self-fix", Some(original.clone()), reply).await;

    let mut revised = None;
    let mut diff_refreshed = false;
    wait_for(&mut rx, |e| {
        if e.card_id == card_id {
            match &e.kind {
                ExecutorEventKind::HandoffUpdated { handoff } => revised = Some(handoff.clone()),
                ExecutorEventKind::DiffUpdated { .. } => diff_refreshed = true,
                _ => {}
            }
        }
        (revised.is_some() && diff_refreshed).then_some(())
    })
    .await;
    let revised = revised.unwrap();
    assert_eq!(revised.risks, vec!["Needs a migration"]);
    assert_eq!(revised.tests, original.tests);
    assert_eq!(revised.summary, original.summary);
    assert_eq!(store.get_handoff(card_id).unwrap(), Some(revised));

    let prompt = fix_prompt(&prompts);
    assert!(prompt.contains("usine-handoff-revision"), "{prompt}");
    assert!(prompt.contains("- no migration\n"), "the fix run sees the risks");
    assert!(prompt.contains("- Keep the old label?\n"));
    assert!(prompt.contains("\"scenario\":\"Reload the form\""));
    assert!(prompt.contains("\"verified\":true}"));
}

#[tokio::test]
async fn a_fix_run_on_a_card_without_a_hand_off_is_not_asked_to_revise_one() {
    let reply = "TL;DR: fixed it.\n\n```usine-fixes\n{\"outcomes\": []}\n```";
    let (_store, card_id, mut rx, prompts) =
        run_self_fixes("/tmp/change-recap-self-fix-none", None, reply).await;
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::FixReportUpdated { .. } if e.card_id == card_id => Some(()),
        _ => None,
    })
    .await;
    let prompt = fix_prompt(&prompts);
    assert!(!prompt.contains("usine-handoff-revision"), "{prompt}");
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

/// An agent that finishes with prose (or the scripted `result`) — or, with `hang`,
/// starts and never finishes (its event sender is kept alive).
#[derive(Default)]
struct ScriptedFactory {
    hang: bool,
    /// The final message; a plain prose recap when unset.
    result: Option<String>,
    held: Arc<Mutex<Vec<UnboundedSender<AgentEvent>>>>,
}

struct ScriptedProvider {
    hang: bool,
    result: Option<String>,
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
                result: self
                    .result
                    .clone()
                    .unwrap_or_else(|| "Reworded the paragraph.".into()),
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
            result: self.result.clone(),
            held: self.held.clone(),
        })
    }
}
