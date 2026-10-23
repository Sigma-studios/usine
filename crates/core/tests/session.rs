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
    reconcile_interrupted_sessions, spawn_executor, AgentEvent, AgentProvider, AppSettings,
    BgTaskKind, BgTaskStatus, Card,
    CardConfig, CardState, CoreError, DesignSub, Effort, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, ExecutorHandle, ModelSpec, Project, ProjectConfig, Provider,
    ProviderFactory, Result, RunConfig, RunHandle, RunMode, Session, SessionAccess, SessionStatus,
    SessionTurnKind, SimFactory, SimForge, SimGit, Store, TaskEvent,
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
    let s = wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    assert!(!s.needs_attention(), "a running turn doesn't count");
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
    let s = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::SessionUpdated(s) if s.id == id && !s.unread => Some(s.clone()),
        _ => None,
    })
    .await;
    assert!(
        s.needs_attention(),
        "reading it doesn't clear it from the counts"
    );

    handle.send(ExecutorCommand::SetSessionBlocked {
        session_id: id,
        blocked: true,
        note: Some("  waiting on infra ".into()),
    });
    let s = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::SessionUpdated(s) if s.id == id && s.blocked => Some(s.clone()),
        _ => None,
    })
    .await;
    assert!(!s.needs_attention(), "a blocked session doesn't count");
    assert_eq!(s.blocked_note.as_deref(), Some("waiting on infra"));
    assert_eq!(
        store.get_session(id).unwrap().blocked_note.as_deref(),
        Some("waiting on infra"),
        "the note is persisted"
    );

    // Running more work keeps the marker: only the user lifts it.
    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: "and the desktop".into(),
    });
    wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    let s = wait_status(&mut rx, id, finished).await;
    assert!(s.blocked, "a turn never unblocks the session");
    assert!(!s.needs_attention());
    assert!(store.get_session(id).unwrap().blocked);

    handle.send(ExecutorCommand::SetSessionBlocked {
        session_id: id,
        blocked: false,
        note: None,
    });
    let s = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::SessionUpdated(s) if s.id == id && !s.blocked => Some(s.clone()),
        _ => None,
    })
    .await;
    assert!(s.needs_attention(), "unblocked, it counts again");
    assert_eq!(s.blocked_note, None, "unblocking drops the note");
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

fn create_with(handle: &ExecutorHandle, id: Uuid, folder: &Path, provider: Provider, prompt: &str) {
    let spec = CardConfig::default_for(provider).implement;
    handle.send(ExecutorCommand::CreateSession {
        id,
        folder: folder.to_path_buf(),
        provider,
        spec,
        access: SessionAccess::FullAccess,
        prompt: prompt.into(),
    });
}

fn send(handle: &ExecutorHandle, id: Uuid, text: &str) {
    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: text.into(),
    });
}

/// Send while running and wait for the command to release its claim: a send
/// arriving while the previous one still holds it is dropped as a double
/// submit.
async fn send_settled(
    handle: &ExecutorHandle,
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    id: Uuid,
    text: &str,
) {
    send(handle, id, text);
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::CardBusy { busy: false } if e.card_id == id => Some(()),
        _ => None,
    })
    .await;
}

/// Wait for a `SessionUpdated` for `id` matching `pred`.
async fn wait_session(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    id: Uuid,
    pred: impl Fn(&Session) -> bool,
) -> Session {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::SessionUpdated(s) if s.id == id && pred(s) => Some((**s).clone()),
        _ => None,
    })
    .await
}

fn user_texts(s: &Session) -> Vec<String> {
    s.turns
        .iter()
        .filter(|t| t.kind == SessionTurnKind::User)
        .map(|t| t.text.clone())
        .collect()
}

// Codex can't take live messages: what the user sends mid-turn is queued, can
// be taken back, and goes out as ONE resumed turn when the turn ends — the
// session staying Running throughout (so it pings once, at the very end).
#[tokio::test]
async fn messages_sent_while_running_are_queued_and_sent_as_one_turn() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create_with(&handle, id, folder.path(), Provider::Codex, "[slow] start");
    // The agent's `usine-status:` line becomes the session's note.
    let s = wait_session(&mut rx, id, |s| s.status_note.is_some()).await;
    assert!(s.is_running());
    assert!(spy.last().full_prompt().contains("usine-status:"));

    send_settled(&handle, &mut rx, id, "one").await;
    send_settled(&handle, &mut rx, id, "two").await;
    let s = store.get_session(id).unwrap();
    assert_eq!(s.queued.len(), 2);
    handle.send(ExecutorCommand::UnqueueSessionMessage {
        session_id: id,
        ts: s.queued[0].ts,
    });
    wait_session(&mut rx, id, |s| s.queued.len() == 1).await;
    send_settled(&handle, &mut rx, id, "three").await;
    assert_eq!(store.get_session(id).unwrap().queued.len(), 2);
    assert_eq!(spy.count(), 1, "nothing launches while the turn runs");

    // The first Finished is the end of the queued turn: no stop in between.
    let s = wait_status(&mut rx, id, finished).await;
    assert_eq!(user_texts(&s), ["[slow] start", "two", "three"]);
    assert!(s.queued.is_empty());
    let kinds: Vec<_> = s.turns.iter().map(|t| t.kind).collect();
    assert_eq!(
        kinds,
        [
            SessionTurnKind::User,
            SessionTurnKind::Agent,
            SessionTurnKind::User,
            SessionTurnKind::User,
            SessionTurnKind::Agent
        ]
    );
    // Status lines are progress, not part of the reply.
    assert!(s.turns.iter().all(|t| !t.text.contains("usine-status")));
    assert_eq!(spy.count(), 2);
    let cfg = spy.last();
    assert!(cfg.resume_session.is_some());
    let prompt = cfg.full_prompt();
    assert!(prompt.starts_with("The user sent these messages while you were working"));
    assert!(prompt.contains("1. two\n\n2. three"), "{prompt}");
    assert!(!prompt.contains(". one"), "{prompt}");
}

// A simulated Claude takes a message mid-turn, like the real CLI: it joins
// the conversation at once and the same run handles it.
#[tokio::test]
async fn a_message_sent_to_a_running_claude_session_steers_it_live() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create(&handle, id, folder.path(), "[slow] start");
    wait_session(&mut rx, id, |s| s.status_note.is_some()).await;

    send(&handle, id, "steer me");
    let s = wait_session(&mut rx, id, |s| user_texts(s).len() == 2).await;
    assert!(s.is_running());
    assert!(s.queued.is_empty());

    let s = wait_status(&mut rx, id, finished).await;
    assert_eq!(spy.count(), 1, "the running turn handled it");
    let agent = s.last_turn_of(SessionTurnKind::Agent).unwrap();
    assert!(agent.text.contains("steer me"), "{}", agent.text);
    assert!(
        s.status_note.is_some(),
        "the last note stays until the next turn"
    );
}

// A Stop keeps the queue; the next Send carries it first.
#[tokio::test]
async fn a_stop_keeps_the_queue_for_the_next_message() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();
    create_with(&handle, id, folder.path(), Provider::Codex, "[slow] start");
    wait_session(&mut rx, id, |s| s.status_note.is_some()).await;
    send(&handle, id, "later");
    wait_session(&mut rx, id, |s| s.queued.len() == 1).await;

    handle.send(ExecutorCommand::CancelSession { session_id: id });
    let s = wait_status(&mut rx, id, finished).await;
    assert_eq!(s.queued.len(), 1);

    send(&handle, id, "now");
    wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    let s = wait_status(&mut rx, id, finished).await;
    assert!(s.queued.is_empty());
    assert_eq!(user_texts(&s), ["[slow] start", "later", "now"]);
    assert!(spy.last().full_prompt().contains("1. later\n\n2. now"));

    // A blank send flushes a queue on its own.
    let mut stored = store.get_session(id).unwrap();
    assert!(stored.queued.is_empty());
    stored
        .queued
        .push(usine_core::SessionTurn::new(SessionTurnKind::User, "solo"));
    store.upsert_session(&stored).unwrap();
    send(&handle, id, "  ");
    wait_status(&mut rx, id, |s| matches!(s, SessionStatus::Running)).await;
    wait_status(&mut rx, id, finished).await;
    assert!(spy.last().full_prompt().starts_with("solo"));
}

/// Every `SessionUpdated` for `id` until it settles (`Finished`), the last
/// one included.
async fn updates_until_finished(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    id: Uuid,
) -> Vec<Session> {
    let mut seen = Vec::new();
    loop {
        let s = wait_for(rx, |e| match &e.kind {
            ExecutorEventKind::SessionUpdated(s) if s.id == id => Some((**s).clone()),
            _ => None,
        })
        .await;
        let done = finished(&s.status);
        seen.push(s);
        if done {
            return seen;
        }
    }
}

#[tokio::test]
async fn background_agents_are_tracked_to_completion() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();

    create(&handle, id, folder.path(), "Survey it [bg-agent]");
    let seen = updates_until_finished(&mut rx, id).await;
    assert!(
        seen.iter()
            .any(|s| s.is_running() && s.running_agents().len() == 1),
        "the agent shows while it works"
    );
    let s = seen.last().unwrap();
    let agents: Vec<_> = s
        .bg_tasks
        .iter()
        .filter(|t| t.kind == BgTaskKind::Agent)
        .collect();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].status, BgTaskStatus::Completed);
    assert_eq!(agents[0].summary.as_deref(), Some("All 42 tests pass"));
    assert_eq!(agents[0].tool_uses, 3);
    assert_eq!(spy.count(), 1, "nothing to resume");
    assert!(s.unread);
    assert!(s.turns.iter().all(|t| t.kind != SessionTurnKind::Notice));
}

#[tokio::test]
async fn a_cut_off_agent_is_resumed_without_the_session_ever_settling() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();

    create(&handle, id, folder.path(), "Integrate [bg-cutoff]");
    let seen = updates_until_finished(&mut rx, id).await;
    let (last, before) = seen.split_last().unwrap();
    assert!(
        before.iter().all(|s| s.is_running() && !s.unread),
        "no attention, no unread until the resumed turn ends"
    );
    assert_eq!(spy.count(), 2, "one auto-resume");
    let resume = spy.last();
    assert!(resume.resume_session.is_some(), "it resumes the conversation");
    let prompt = resume.full_prompt();
    assert!(prompt.contains("\"Integrate the walls\" (general-purpose)"));
    assert!(prompt.contains("its own background command (\"cargo test --workspace\")"));
    assert!(prompt.contains("Resume or relaunch every one"));

    let notices: Vec<_> = last
        .turns
        .iter()
        .filter(|t| t.kind == SessionTurnKind::Notice)
        .collect();
    assert_eq!(notices.len(), 1);
    let agents: Vec<_> = last
        .bg_tasks
        .iter()
        .filter(|t| t.kind == BgTaskKind::Agent)
        .map(|t| t.status)
        .collect();
    assert_eq!(
        agents,
        [BgTaskStatus::Relaunched, BgTaskStatus::Completed],
        "the cut-off entry is superseded by its relaunch"
    );
    assert!(last.unread);
    assert!(last.cut_off_agents().is_empty());
}

#[tokio::test]
async fn auto_resume_keeps_going_while_each_round_makes_progress() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();

    create(&handle, id, folder.path(), "Go [bg-rounds]");
    let seen = updates_until_finished(&mut rx, id).await;
    assert_eq!(spy.count(), 3, "two cut-off rounds, then done");
    let last = seen.last().unwrap();
    assert_eq!(
        last.turns
            .iter()
            .filter(|t| t.kind == SessionTurnKind::Notice)
            .count(),
        2
    );
    assert!(!last.turns.last().unwrap().text.contains("auto-resume paused"));
    assert!(
        last.cut_off_agents().is_empty(),
        "the agents the resumed turns were told about are settled"
    );
}

#[tokio::test]
async fn a_stuck_cut_off_loop_pauses_after_one_resume_until_the_user_writes() {
    let folder = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    let id = Uuid::new_v4();

    create(&handle, id, folder.path(), "Go [bg-cutoff-stuck]");
    let s = wait_status(&mut rx, id, finished).await;
    assert_eq!(spy.count(), 2, "one auto-resume, then the guard holds");
    assert!(s.unread);
    assert!(s.needs_attention());
    let reply = s.last_turn_of(SessionTurnKind::Agent).unwrap();
    assert!(reply.text.contains("cut off again with no progress"));
    assert_eq!(s.cut_off_agents().len(), 1);

    // The user's message names the agent, and starts the guard over: the
    // same stuck agent gets one more automatic round.
    handle.send(ExecutorCommand::SendSessionMessage {
        session_id: id,
        text: "try again".into(),
    });
    let s = wait_status(&mut rx, id, finished).await;
    assert_eq!(spy.count(), 4);
    let prompts: Vec<String> = spy
        .runs
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.full_prompt())
        .collect();
    assert!(prompts[2].starts_with("try again"));
    assert!(prompts[2].contains("never relaunched: \"Stuck worker [bg-cutoff-stuck]\""));
    assert!(s.last_turn_of(SessionTurnKind::Agent).unwrap().text.contains("auto-resume paused"));
}

#[tokio::test]
async fn a_restart_cuts_off_live_agents_and_retry_names_them() {
    let store = Store::open_in_memory().unwrap();
    let mut s = Session::new(
        Uuid::new_v4(),
        std::env::temp_dir(),
        Provider::Claude,
        ModelSpec::new("opus", Effort::High),
        SessionAccess::FullAccess,
        "work",
        vec![],
    );
    s.apply_task(&TaskEvent::Started {
        id: "a1".into(),
        parent: None,
        kind: BgTaskKind::Agent,
        description: "Long job".into(),
        subagent_type: None,
        backgrounded: true,
    });
    store.upsert_session(&s).unwrap();
    reconcile_interrupted_sessions(&store, "Usine restarted").unwrap();
    let s = store.get_session(s.id).unwrap();
    assert_eq!(s.bg_tasks[0].status, BgTaskStatus::CutOff);

    let spy = SpyFactory::default();
    let (handle, mut rx) = spawn(&store, &spy);
    handle.send(ExecutorCommand::RetrySession { session_id: s.id });
    let done = wait_status(&mut rx, s.id, finished).await;
    assert!(spy.last().full_prompt().contains("never relaunched: \"Long job\""));
    assert!(done.bg_tasks.is_empty(), "settled tasks are dropped on a new turn");
}
