//! End-to-end smoke of the self-review fix picker over the simulated backends:
//! a free-form note typed there must launch a fix run even when the user checked
//! none of the agent's findings; with no note and nothing checked, the picker
//! must still short-circuit straight to the PR; and an edited finding must reach
//! the fix run's prompt as edited — the command carries the verdicts wholesale.
//! The picker's steering also rides through: a per-finding instruction reaches
//! the prompt attached to its comment, and a hand-edited task is sent verbatim
//! (blank meaning "run nothing" even with rows checked).

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

/// Every prompt handed to a provider, tagged with the run mode that asked for it
/// (the same spy as `handoff.rs`).
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

/// Spawn an executor over the simulated backends holding one plan-skipping card,
/// and drive it to the self-review fix picker. Also returns the captured prompts
/// and the picker's verdicts, so a test can edit and re-send them.
async fn card_at_the_fix_picker() -> (
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
    Store,
    uuid::Uuid,
    Prompts,
    Vec<usine_core::FixVerdict>,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/review-note"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "d", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    // Straight to implementing — the plan phase would block on a sim question.
    store.set_skip_plan(card_id, true).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(prompts.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    // Implement; the self-review pass auto-starts when the run lands.
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

    (handle, rx, store, card_id, prompts, verdicts)
}

#[tokio::test]
async fn a_note_alone_launches_a_fix_run() {
    let (handle, mut rx, store, card_id, _prompts, _verdicts) = card_at_the_fix_picker().await;

    // Nothing checked, but the user typed their own comment: the agent must run.
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts: vec![],
        note: "  also rename `x` to `count`  ".into(),
        prompt: None,
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::ApplyingFixes) => Some(()),
            // Skipping to the PR here would mean the note was silently dropped.
            CardState::AwaitingReview(ReviewSub::ReadyForPr) => {
                panic!("a note-only apply must run the agent, not skip to the PR")
            }
            _ => None,
        },
        _ => None,
    })
    .await;

    // The run lands and the pass completes as usual.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::AwaitingReview(ReviewSub::ReadyForPr)) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // The note is kept (trimmed) so a later "back to start" folds it into the prompt.
    let qa = store.get_card(card_id).unwrap().qa_log;
    assert_eq!(qa, vec!["Requested change: also rename `x` to `count`"]);
}

#[tokio::test]
async fn an_edited_finding_reaches_the_fix_prompt_as_edited() {
    let (handle, mut rx, store, card_id, prompts, mut verdicts) = card_at_the_fix_picker().await;

    // The user rewords the first finding before applying it — the fix run must
    // receive their wording, not the agent's.
    let original = verdicts[0].comment.body.clone();
    verdicts[0].comment.body = "EDITED: only rename the log field, keep the metric".into();
    verdicts[0].selected = true;
    for v in &mut verdicts[1..] {
        v.selected = false;
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

    let fixes = prompts
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| *m == RunMode::ApplyFixes)
        .map(|(_, p)| p.clone())
        .expect("a fix run");
    assert!(
        fixes.contains("EDITED: only rename the log field, keep the metric"),
        "the fix prompt must carry the edited finding:\n{fixes}"
    );
    assert!(
        !fixes.contains(&original),
        "and not the agent's original wording:\n{fixes}"
    );

    // The "Fix applied" restart-log line is recorded only once the fix run
    // lands its commit (stashed at launch; see `apply_self_fixes`) — and it
    // carries the user's wording, since the edited verdicts went wholesale.
    let qa = store.get_card(card_id).unwrap().qa_log;
    assert!(
        qa.iter()
            .any(|l| l.starts_with("Fix applied per review comment:")
                && l.contains("EDITED: only rename the log field")),
        "the landed fix must put its 'Fix applied' line on the restart log: {qa:?}"
    );
}

#[tokio::test]
async fn nothing_checked_and_no_note_still_skips_to_the_pr() {
    let (handle, mut rx, store, card_id, _prompts, _verdicts) = card_at_the_fix_picker().await;

    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts: vec![],
        note: "   ".into(),
        prompt: None,
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::ReadyForPr) => Some(()),
            CardState::AwaitingReview(ReviewSub::ApplyingFixes) => {
                panic!("with nothing to do the picker must skip straight to the PR")
            }
            _ => None,
        },
        _ => None,
    })
    .await;

    assert!(store.get_card(card_id).unwrap().qa_log.is_empty());
}

#[tokio::test]
async fn a_per_finding_instruction_reaches_the_fix_prompt() {
    let (handle, mut rx, store, card_id, prompts, mut verdicts) = card_at_the_fix_picker().await;

    verdicts[0].selected = true;
    verdicts[0].instruction = "do it in the extractor, not the caller".into();
    for v in &mut verdicts[1..] {
        v.selected = false;
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

    let fixes = prompts
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| *m == RunMode::ApplyFixes)
        .map(|(_, p)| p.clone())
        .expect("a fix run");
    assert!(
        fixes.contains("do it in the extractor, not the caller"),
        "the steering must reach the fix prompt:\n{fixes}"
    );

    let qa = store.get_card(card_id).unwrap().qa_log;
    assert!(
        qa.iter()
            .any(|l| l.starts_with("Fix applied per review comment:")
                && l.ends_with("as instructed: do it in the extractor, not the caller")),
        "the restart log must restate the steering: {qa:?}"
    );
}

#[tokio::test]
async fn an_edited_task_is_sent_verbatim() {
    let (handle, mut rx, store, card_id, prompts, mut verdicts) = card_at_the_fix_picker().await;

    verdicts[0].selected = true;
    for v in &mut verdicts[1..] {
        v.selected = false;
    }
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts,
        note: String::new(),
        prompt: Some("Do exactly this: rename the log field and nothing else.".into()),
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

    let fixes = prompts
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| *m == RunMode::ApplyFixes)
        .map(|(_, p)| p.clone())
        .expect("a fix run");
    assert!(
        fixes.contains("Do exactly this: rename the log field and nothing else."),
        "the hand-written task must be what the agent gets:\n{fixes}"
    );
    assert!(
        !fixes.contains("Address the following review comments:"),
        "and it must replace the generated task, not be appended to it:\n{fixes}"
    );

    // The bookkeeping still follows the checkboxes, and the edited task itself
    // goes on the restart log so a later reset knows what was actually sent.
    let qa = store.get_card(card_id).unwrap().qa_log;
    assert!(
        qa.iter()
            .any(|l| l.starts_with("Fix applied per review comment:")),
        "a checked row is still logged as fixed: {qa:?}"
    );
    assert!(
        qa.iter()
            .any(|l| l.starts_with("Fix task as sent (edited by the user):")
                && l.contains("Do exactly this")),
        "the sent task must be on the log: {qa:?}"
    );
}

#[tokio::test]
async fn a_blank_edited_task_runs_nothing() {
    let (handle, mut rx, store, card_id, _prompts, mut verdicts) = card_at_the_fix_picker().await;

    // Rows are checked, but the user blanked the task: nothing to run.
    for v in &mut verdicts {
        v.selected = true;
    }
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts,
        note: String::new(),
        prompt: Some("   ".into()),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::ReadyForPr) => Some(()),
            CardState::AwaitingReview(ReviewSub::ApplyingFixes) => {
                panic!("a blank task must not launch a run")
            }
            _ => None,
        },
        _ => None,
    })
    .await;

    assert!(store.get_card(card_id).unwrap().qa_log.is_empty());
}

#[tokio::test]
async fn an_edited_task_does_not_log_the_dropped_note() {
    let (handle, mut rx, store, card_id, prompts, mut verdicts) = card_at_the_fix_picker().await;

    // The user typed a note AND then hand-wrote the task: the note is replaced
    // wholesale, so it never reaches the agent — and must not be recorded as a
    // request either, or a later "back to start" would restate it.
    verdicts[0].selected = true;
    for v in &mut verdicts[1..] {
        v.selected = false;
    }
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts,
        note: "also rename `x` to `count`".into(),
        prompt: Some("Do exactly this: rename the log field and nothing else.".into()),
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

    let fixes = prompts
        .lock()
        .unwrap()
        .iter()
        .find(|(m, _)| *m == RunMode::ApplyFixes)
        .map(|(_, p)| p.clone())
        .expect("a fix run");
    assert!(
        !fixes.contains("also rename `x` to `count`"),
        "the note is not part of the hand-written task:\n{fixes}"
    );

    let qa = store.get_card(card_id).unwrap().qa_log;
    assert!(
        !qa.iter().any(|l| l.starts_with("Requested change:")),
        "a note the agent never saw must not be logged as a request: {qa:?}"
    );
}
