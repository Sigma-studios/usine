//! A fix run's per-finding outcomes survive the round trip: the findings the
//! user ticked are stashed at launch, the run's `usine-fixes` block is parsed
//! when it lands, and the two are stored joined so the merge gate can show
//! "you asked for N, here is what came back".
//!
//! Driven end to end over the simulated backends (whose fix run emits the
//! block), because the value here is entirely in the wiring: the stash is
//! written at one end of the executor and taken at the other.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, FixItem, FixOutcome, FixReport, Outcome, PrInfo, Project,
    ProjectConfig, Provider, ProviderFactory, ReviewSub, RunConfig, RunHandle, SimFactory,
    SimForge, SimGit, Store,
};

/// Await events until `f` returns `Some`, failing the test on timeout.
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

#[tokio::test]
async fn a_self_review_fix_run_reports_its_outcomes_against_what_was_picked() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/fix-outcomes"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    // Straight to implementing — the plan phase would block on a sim question.
    store.set_skip_plan(card_id, true).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    handle.send(ExecutorCommand::Start { card_id });
    let verdicts = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) => {
                Some(verdicts.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(verdicts.len(), 2, "the sim self-review finds two things");

    // Tick BOTH rows. The sim's fix run only reports on the first, which is the
    // case worth pinning: an under-reporting run must leave the second visible
    // as unanswered rather than silently dropping it.
    let picked: Vec<_> = verdicts
        .into_iter()
        .map(|mut v| {
            v.selected = true;
            v
        })
        .collect();
    let ids: Vec<u64> = picked.iter().map(|v| v.comment.id).collect();
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts: picked,
        note: String::new(),
        prompt: None,
    });

    let report = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::FixReportUpdated { report } if e.card_id == card_id => {
            Some(report.clone())
        }
        _ => None,
    })
    .await;

    assert_eq!(
        report.items.iter().map(|i| i.id).collect::<Vec<_>>(),
        ids,
        "the checklist is the rows the user ticked, in picker order"
    );
    assert_eq!(report.tally(), (1, 2), "one of the two came back addressed");
    let rows = report.rows();
    assert_eq!(rows[0].outcome.as_ref().unwrap().outcome, Outcome::Partial);
    assert!(
        !rows[0].outcome.as_ref().unwrap().note.is_empty(),
        "the run's note is what makes 'partial' actionable"
    );
    assert!(
        rows[1].outcome.is_none(),
        "a picked finding the run never mentioned stays on the checklist"
    );

    // Persisted, so the merge gate and the Done panel read it back after a restart…
    let stored = store.get_fix_report(card_id).unwrap().expect("stored");
    assert_eq!(stored, report);
    // …and the stash is consumed, so the next run's checklist is its own.
    assert!(store.take_pending_fix_items(card_id).unwrap().is_empty());

    // The block is machine-facing: it must not survive into the prose recap.
    if let Some(recap) = store.get_review_recap(card_id).unwrap() {
        assert!(!recap.contains("usine-fixes"), "recap: {recap}");
    }
}

/// A later fix run that reports nothing — a note-only reprompt on a PR whose
/// agent emitted no `usine-fixes` block — must CLEAR the checklist rather than
/// leave the previous run's outcomes describing it. The merge gate reads this
/// field to say "3 of 3 addressed"; pointing that at a run that never happened
/// is worse than saying nothing.
#[tokio::test]
async fn a_fix_run_that_reports_nothing_clears_the_previous_checklist() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/fix-outcomes-stale"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
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

    // A checklist from an earlier run, as the gate would be rendering it.
    let stale = FixReport {
        v: 1,
        items: vec![FixItem {
            id: 1,
            label: "the old finding".into(),
            path: "src/a.rs".into(),
            line: Some(4),
            severity: "high".into(),
        }],
        outcomes: vec![FixOutcome {
            id: 1,
            outcome: Outcome::Fixed,
            note: String::new(),
        }],
        malformed: false,
    };
    store.set_fix_report(card_id, &stale).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SilentFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::RequestPostPrChange {
        card_id,
        feedback: "tweak the wording".into(),
    });

    let report = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::FixReportUpdated { report } if e.card_id == card_id => {
            Some(report.clone())
        }
        _ => None,
    })
    .await;
    assert!(report.is_empty(), "stale checklist survived: {report:?}");
    assert!(store.get_fix_report(card_id).unwrap().is_none());
}

/// An agent that finishes with prose and no blocks at all.
struct SilentProvider;

#[async_trait::async_trait]
impl AgentProvider for SilentProvider {
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
            result: "Reworded the paragraph.".into(),
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

struct SilentFactory;

impl ProviderFactory for SilentFactory {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(SilentProvider)
    }
}
