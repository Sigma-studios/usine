//! One-off sessions over the simulated backend: create → turn → finish,
//! resumed follow-ups, questions, stop, retry, attachments riding exactly one
//! turn, and sessions never waiting on the run gate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    reconcile_interrupted_sessions, spawn_executor, AgentEvent, AgentProvider, AppSettings, Card,
    CardConfig, CardState, CoreError, DesignSub, Effort, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, ExecutorHandle, ModelSpec, Project, ProjectConfig, Provider,
    ProviderFactory, Result, RunConfig, RunHandle, RunMode, Session, SessionAccess, SessionStatus,
    SessionTurnKind, SimFactory, SimForge, SimGit, Store,
};
use uuid::Uuid;

type Runs = Arc<Mutex<Vec<RunConfig>>>;

/// The simulator, recording every run's config. `error_next` makes the next
/// run fail at once (no output); `hang_next` makes it work until cancelled.
struct SpyProvider {
    inner: Arc<dyn AgentProvider>,
    runs: Runs,
    error_next: Arc<AtomicBool>,
    hang_next: Arc<AtomicBool>,
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
        self.runs.lock().unwrap().push(cfg.clone());
        if self.error_next.swap(false, Ordering::SeqCst) {
            let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
            let (ctl_tx, _ctl_rx) = futures::channel::mpsc::unbounded();
            let _ = evt_tx.unbounded_send(AgentEvent::Error {
                message: "boom".into(),
            });
            return Ok(RunHandle {
                events: evt_rx.boxed(),
                control: ctl_tx,
            });
        }
        if self.hang_next.swap(false, Ordering::SeqCst) {
            let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
            let (ctl_tx, mut ctl_rx) = futures::channel::mpsc::unbounded();
            let _ = evt_tx.unbounded_send(AgentEvent::Started {
                session_id: "sess-hang".into(),
            });
            tokio::spawn(async move {
                let _ = ctl_rx.next().await;
                let _ = evt_tx.unbounded_send(AgentEvent::Error {
                    message: "run cancelled".into(),
                });
            });
            return Ok(RunHandle {
                events: evt_rx.boxed(),
                control: ctl_tx,
            });
        }
        self.inner.start(cfg).await
    }
}

#[derive(Default, Clone)]
struct SpyFactory {
    runs: Runs,
    error_next: Arc<AtomicBool>,
    hang_next: Arc<AtomicBool>,
}

impl ProviderFactory for SpyFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(SpyProvider {
            inner: SimFactory.make(provider),
            runs: self.runs.clone(),
            error_next: self.error_next.clone(),
            hang_next: self.hang_next.clone(),
        })
    }
}

impl SpyFactory {
    fn last(&self) -> RunConfig {
        self.runs.lock().unwrap().last().cloned().expect("a run")
    }
    fn count(&self) -> usize {
        self.runs.lock().unwrap().len()
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

/// Wait for a `SessionUpdated` for `id` whose status satisfies `pred`.
async fn wait_status(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    id: Uuid,
    pred: impl Fn(&SessionStatus) -> bool,
) -> Session {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::SessionUpdated(s) if s.id == id && pred(&s.status) => {
            Some((**s).clone())
        }
        _ => None,
    })
    .await
}

fn finished(s: &SessionStatus) -> bool {
    matches!(s, SessionStatus::Finished)
}

fn spawn(store: &Store, spy: &SpyFactory) -> (ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(spy.clone()),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

fn create(handle: &ExecutorHandle, id: Uuid, folder: &Path, prompt: &str) {
    handle.send(ExecutorCommand::CreateSession {
        id,
        folder: folder.to_path_buf(),
        provider: Provider::Claude,
        spec: ModelSpec::new("opus", Effort::High),
        access: SessionAccess::FullAccess,
        prompt: prompt.into(),
    });
}

/// Write `names` into `dir` and stage them as `id`'s attachments, the way the
/// attach commands leave them (without touching the real data dir).
fn stage(store: &Store, id: Uuid, dir: &Path, names: &[&str]) -> Vec<PathBuf> {
    let paths: Vec<PathBuf> = names
        .iter()
        .map(|n| {
            let p = dir.join(n);
            std::fs::write(&p, b"x").unwrap();
            p
        })
        .collect();
    store.set_attachments(id, &paths).unwrap();
    paths
}

#[tokio::test]
async fn a_session_runs_its_first_turn_to_an_unread_finish() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();

    create(&handle, id, folder.path(), "Tidy up the downloads\nplease");
    let s = wait_status(&mut rx, id, finished).await;

    assert_eq!(s.title, "Tidy up the downloads");
    assert!(s.unread, "a finished turn is unread");
    assert!(s.needs_attention());
    let agent = s
        .last_turn_of(SessionTurnKind::Agent)
        .expect("an agent turn");
    assert!(agent.text.contains("Tidy up the downloads"));
    assert!(s
        .last_session
        .as_deref()
        .unwrap()
        .starts_with("sim-session-"));
    assert!(s.cost > usine_core::Cost::ZERO);

    let cfg = spy.last();
    assert_eq!(cfg.mode, RunMode::SessionFull);
    assert_eq!(cfg.project_dir, folder.path());
    assert_eq!(cfg.resume_session, None);
    assert_eq!(cfg.full_prompt(), "Tidy up the downloads\nplease");
    // The dialog's choices are remembered for next time.
    let defaults = store.settings().unwrap().session_defaults.unwrap();
    assert_eq!(defaults.spec.model, "opus");

    handle.send(ExecutorCommand::MarkSessionRead { session_id: id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::SessionUpdated(s) if s.id == id && !s.unread => Some(()),
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn creating_in_a_missing_folder_is_refused() {
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create(&handle, id, Path::new("/definitely/not/here"), "hi");
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast { message, .. } if message.contains("not an existing folder") => {
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(matches!(store.get_session(id), Err(CoreError::NotFound(_))));
    assert_eq!(spy.count(), 0);
}

#[tokio::test]
async fn a_follow_up_resumes_the_stored_conversation() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create(&handle, id, folder.path(), "first");
    let s = wait_status(&mut rx, id, finished).await;
    let cli_session = s.last_session.clone().unwrap();

    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: "second".into(),
    });
    wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    let s = wait_status(&mut rx, id, finished).await;

    let cfg = spy.last();
    assert_eq!(cfg.resume_session.as_deref(), Some(cli_session.as_str()));
    assert_eq!(cfg.full_prompt(), "second");
    let kinds: Vec<_> = s.turns.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        [
            SessionTurnKind::User,
            SessionTurnKind::Agent,
            SessionTurnKind::User,
            SessionTurnKind::Agent
        ]
    );
}

#[tokio::test]
async fn a_question_parks_the_session_until_answered() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create(&handle, id, folder.path(), "change it [ask]");

    let s = wait_status(&mut rx, id, |s| matches!(s, SessionStatus::NeedsAnswer(_))).await;
    assert!(s.unread);
    assert_eq!(s.turns.last().unwrap().kind, SessionTurnKind::Question);

    // A plain message can't jump the question.
    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: "ignore that".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast { message, .. } if message.contains("waiting for an answer") => {
            Some(())
        }
        _ => None,
    })
    .await;

    handle.send(ExecutorCommand::AnswerSession {
        session_id: id,
        text: "Yes, go ahead".into(),
    });
    let s = wait_status(&mut rx, id, finished).await;
    let kinds: Vec<_> = s.turns.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        [
            SessionTurnKind::User,
            SessionTurnKind::Question,
            SessionTurnKind::Answer,
            SessionTurnKind::Agent
        ]
    );
    // The interactive sim took the answer in place: still one run.
    assert_eq!(spy.count(), 1);
}

#[tokio::test]
async fn stopping_a_running_session_finishes_it() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    spy.hang_next.store(true, Ordering::SeqCst);
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create(&handle, id, folder.path(), "long job");
    wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    // Let the run register before stopping it.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Transcript { line, .. }
            if e.card_id == id && line.contains("sess-hang") =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    handle.send(ExecutorCommand::CancelSession { session_id: id });
    let s = wait_status(&mut rx, id, finished).await;
    let last = s.turns.last().unwrap();
    assert_eq!(last.kind, SessionTurnKind::Error);
    assert_eq!(last.text, "Stopped by user");
    // The cancelled run's own "run cancelled" error must not fail it after.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(finished(&store.get_session(id).unwrap().status));
}

#[tokio::test]
async fn a_session_starts_while_the_run_cap_is_saturated() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    store
        .save_settings(&AppSettings {
            max_concurrent_runs: 1,
            ..AppSettings::default()
        })
        .unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/session-cap"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "a", "d", CardConfig::default());
    store.upsert_card(&card).unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);

    // The card's sim plan run parks on its question, holding the only slot.
    handle.send(ExecutorCommand::Start { card_id: card.id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    let id = Uuid::new_v4();
    create(&handle, id, folder.path(), "quick one");
    wait_status(&mut rx, id, finished).await;
}

#[tokio::test]
async fn attachments_ride_exactly_one_turn() {
    let folder = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create(&handle, id, folder.path(), "first");
    wait_status(&mut rx, id, finished).await;

    let staged = stage(&store, id, files.path(), &["shot.png", "log.txt"]);
    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: "look at these".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AttachmentsChanged { paths } if e.card_id == id && paths.is_empty() => {
            Some(())
        }
        _ => None,
    })
    .await;
    let s = wait_status(&mut rx, id, finished).await;

    let user = &s.turns[2];
    assert_eq!(user.kind, SessionTurnKind::User);
    assert_eq!(user.attachments, staged);
    let cfg = spy.last();
    assert_eq!(cfg.attachments, staged);
    let prompt = cfg.full_prompt();
    assert!(prompt.starts_with("look at these"));
    for p in &staged {
        assert!(prompt.contains(&*p.to_string_lossy()));
    }
    assert!(store.get_attachments(id).unwrap().is_empty());

    // Nothing staged → nothing sent with the next message.
    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: "and now?".into(),
    });
    wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    wait_status(&mut rx, id, finished).await;
    assert!(spy.last().attachments.is_empty());
}

#[tokio::test]
async fn a_session_can_start_with_only_attachments() {
    let folder = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    let staged = stage(&store, id, files.path(), &["1a2b3c4d-error.log"]);
    create(&handle, id, folder.path(), "   ");
    let s = wait_status(&mut rx, id, finished).await;
    assert_eq!(s.title, "Attached: error.log");
    assert_eq!(s.turns[0].attachments, staged);
    assert!(spy
        .last()
        .full_prompt()
        .starts_with("See the attached files."));
}

#[tokio::test]
async fn retry_resends_the_last_message_with_its_surviving_files() {
    let folder = tempfile::tempdir().unwrap();
    let files = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    let staged = stage(&store, id, files.path(), &["a.png", "b.txt"]);
    spy.error_next.store(true, Ordering::SeqCst);
    create(&handle, id, folder.path(), "do it");
    let s = wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Failed { .. })).await;
    assert!(s.unread);

    std::fs::remove_file(&staged[1]).unwrap();
    handle.send(ExecutorCommand::RetrySession { session_id: id });
    let s = wait_status(&mut rx, id, finished).await;
    let cfg = spy.last();
    assert_eq!(cfg.attachments, vec![staged[0].clone()]);
    assert!(cfg.full_prompt().starts_with("do it"));
    // Retry re-sends; it doesn't duplicate the message.
    let users = s
        .turns
        .iter()
        .filter(|t| t.kind == SessionTurnKind::User)
        .count();
    assert_eq!(users, 1);
}

#[tokio::test]
async fn a_failed_resume_falls_back_to_a_fresh_run_with_history() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create(&handle, id, folder.path(), "remember the number 42");
    wait_status(&mut rx, id, finished).await;

    spy.error_next.store(true, Ordering::SeqCst);
    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: "what was the number?".into(),
    });
    wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    wait_status(&mut rx, id, finished).await;

    let runs = spy.runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 3);
    assert!(runs[1].resume_session.is_some());
    let fresh = &runs[2];
    assert_eq!(fresh.resume_session, None);
    let prompt = fresh.full_prompt();
    assert!(prompt.contains("User: remember the number 42"));
    assert!(prompt.ends_with("what was the number?"));
}

#[test]
fn reconcile_fails_interrupted_sessions() {
    let store = Store::open_in_memory().unwrap();
    let running = Session::new(
        Uuid::new_v4(),
        PathBuf::from("/tmp"),
        Provider::Claude,
        ModelSpec::new("opus", Effort::High),
        SessionAccess::FullAccess,
        "x",
        vec![],
    );
    let mut done = running.clone();
    done.id = Uuid::new_v4();
    done.status = SessionStatus::Finished;
    store.upsert_session(&running).unwrap();
    store.upsert_session(&done).unwrap();

    assert_eq!(
        reconcile_interrupted_sessions(&store, "Usine quit").unwrap(),
        1
    );
    let s = store.get_session(running.id).unwrap();
    assert_eq!(
        s.status,
        SessionStatus::Failed {
            message: "Usine quit".into()
        }
    );
    assert!(s.unread);
    assert_eq!(
        store.get_session(done.id).unwrap().status,
        SessionStatus::Finished
    );
}

#[test]
fn deleting_a_session_purges_its_transcript_and_attachments() {
    let store = Store::open_in_memory().unwrap();
    let s = Session::new(
        Uuid::new_v4(),
        PathBuf::from("/tmp"),
        Provider::Codex,
        ModelSpec::new("gpt-5-codex", Effort::Medium),
        SessionAccess::ReadOnly,
        "x",
        vec![],
    );
    store.upsert_session(&s).unwrap();
    store.append_transcript(s.id, 1, "line").unwrap();
    store
        .set_attachments(s.id, &[PathBuf::from("/tmp/a.png")])
        .unwrap();
    // Round trip first.
    assert_eq!(store.get_session(s.id).unwrap(), s);

    store.delete_session(s.id).unwrap();
    assert!(matches!(
        store.get_session(s.id),
        Err(CoreError::NotFound(_))
    ));
    assert!(store.load_transcript_entries(s.id).unwrap().is_empty());
    assert!(store.get_attachments(s.id).unwrap().is_empty());
    assert!(!store.all_attachments().unwrap().contains_key(&s.id));
}

#[test]
fn unstaging_keeps_files_staged_after_the_send_was_read() {
    let files = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let id = Uuid::new_v4();
    let sent = stage(&store, id, files.path(), &["a.png"]);
    // A file attached while the send was in flight.
    let late = stage(&store, id, files.path(), &["a.png", "late.txt"]);
    let remaining = store.unstage_attachments(id, &sent).unwrap();
    assert_eq!(remaining, vec![late[1].clone()]);
    assert_eq!(store.get_attachments(id).unwrap(), vec![late[1].clone()]);
    assert!(store.unstage_attachments(id, &late).unwrap().is_empty());
    assert!(store.get_attachments(id).unwrap().is_empty());
}
