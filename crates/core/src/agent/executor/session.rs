//! One-off sessions: ad-hoc agent chats in any folder (see
//! [`crate::domain::session`]).
//!
//! Modelled on the PR-review runs, not the card launch path: a session has no
//! project, worktree or state machine of the card kind. Each message is one
//! headless turn; every turn after the first resumes the CLI conversation.
//! Sessions skip the run gate (`RunMode::is_capped` is false for them), so a
//! quick session never queues behind a busy board.
//!
//! Attachments reuse the card plumbing keyed by the session id: files are
//! *staged* with the ordinary attach commands, and the command that sends a
//! message takes them (atomically) onto that turn.

use std::path::{Path, PathBuf};
use std::sync::Weak;

use futures::channel::mpsc::{self, UnboundedSender};
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use uuid::Uuid;

use super::{append_attachments, lock, release_run, transcript, Executor, RunMap};
use crate::agent::events::{AgentEvent, ExecutorEvent, RunControl, Severity};
use crate::agent::provider::stream::TODO_PROGRESS_PREFIX;
use crate::agent::provider::{RunConfig, RunMode, SESSION_STATUS_PROMPT};
use crate::domain::config::SessionDefaults;
use crate::domain::model::{now_millis, Cost, Intervention, ModelSpec, Provider};
use crate::domain::session::{
    last_status_line, strip_status_lines, truncate_chars, Session, SessionAccess, SessionStatus,
    SessionTransition, SessionTurn, SessionTurnKind, StatusNote, STATUS_MAX,
};
use crate::error::{CoreError, Result};
use crate::infra::persistence::Store;

/// The session actor's idle watchdog. Longer than the board's
/// `RUN_IDLE_TIMEOUT`: sessions may run background subagents, which stay
/// silent after the parent's interim result until they report back (see
/// `SESSION_HELD_RESULT_GRACE`). A hung session costs little — the user sees it
/// Running and can Stop it.
const SESSION_IDLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2 * 60 * 60 + 5 * 60);

// The Claude pump releases a held session result after its own quiet period;
// it must fire before this watchdog, which would fail the session instead.
const _: () = assert!(
    crate::agent::provider::claude::SESSION_HELD_RESULT_GRACE.as_secs()
        < SESSION_IDLE_TIMEOUT.as_secs()
);

/// The prompt of a message that is only attachments.
pub(crate) const ATTACHMENTS_ONLY_PROMPT: &str = "See the attached files.";

/// Heads the prompt of a turn that carries several messages queued while the
/// agent worked.
const QUEUED_HEADER: &str =
    "The user sent these messages while you were working — handle them in order:";

/// How long a live message waits for the run to say whether it took it.
const LIVE_ACCEPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How many earlier turns a fresh fallback (after a failed `--resume`)
/// replays, and how much of each.
const HISTORY_TURNS: usize = 10;
const HISTORY_TURN_CHARS: usize = 500;

/// What one turn sends: kept by the actor so a failed resume can be replayed
/// as a fresh run carrying the same message.
#[derive(Clone)]
struct TurnInput {
    prompt: String,
    attachments: Vec<PathBuf>,
}

impl Executor {
    pub(super) async fn create_session(
        &self,
        id: Uuid,
        folder: PathBuf,
        provider: Provider,
        spec: ModelSpec,
        access: SessionAccess,
        prompt: String,
    ) -> Result<()> {
        match self.store.get_session(id) {
            Ok(_) => return Err(CoreError::other("that session already exists")),
            Err(CoreError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        if !folder.is_absolute() {
            return Err(CoreError::other(format!(
                "{} is not an absolute path",
                folder.display()
            )));
        }
        if !folder.is_dir() {
            return Err(CoreError::other(format!(
                "{} is not an existing folder",
                folder.display()
            )));
        }
        let staged = self.store.get_attachments(id).unwrap_or_default();
        if prompt.trim().is_empty() && staged.iter().all(|p| !p.exists()) {
            return Err(CoreError::other(
                "write a message or attach a file to start",
            ));
        }
        let attachments = existing(&staged);
        let session = Session::new(
            id,
            folder,
            provider,
            spec.clone(),
            access,
            &prompt,
            attachments.clone(),
        );
        self.store.upsert_session(&session)?;
        self.unstage(id, &staged)?;
        self.emit_session(session.clone());

        // Remember the choices for the next dialog.
        let mut settings = self.store.settings()?;
        let defaults = Some(SessionDefaults {
            provider,
            spec,
            access,
        });
        if settings.session_defaults != defaults {
            settings.session_defaults = defaults;
            self.store.save_settings(&settings)?;
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::settings_updated(settings));
        }

        let input = TurnInput {
            prompt: message_prompt(&prompt, &attachments),
            attachments,
        };
        self.launch_session_turn(&session, input, None).await
    }

    /// Send a message. While a turn runs it goes to the agent live when the
    /// run can take it (Claude, text only), else it is queued for the next
    /// turn. An idle session starts a turn with it, carrying any queued
    /// messages first.
    pub(super) async fn send_session_message(&self, id: Uuid, text: String) -> Result<()> {
        let probe = self.store.get_session(id)?;
        let staged = self.store.get_attachments(id).unwrap_or_default();
        let attachments = existing(&staged);
        let blank = text.trim().is_empty();
        if probe.is_running() {
            if blank && attachments.is_empty() {
                return Err(CoreError::other("write a message or attach a file"));
            }
            if attachments.is_empty() && self.steer_session(id, text.trim()).await {
                return Ok(());
            }
            let turn = SessionTurn::new(SessionTurnKind::User, text.trim())
                .with_attachments(attachments.clone());
            let mut queued = false;
            let session = self.store.mutate_session(id, |s| {
                // The turn may have ended meanwhile: then send it below.
                if s.is_running() {
                    s.queue(turn)?;
                    queued = true;
                }
                Ok(())
            })?;
            if queued {
                self.unstage(id, &staged)?;
                self.emit_session(session);
                transcript(
                    &self.store,
                    &self.evt_tx,
                    id,
                    "⏸ message queued for when this turn ends".into(),
                );
                return Ok(());
            }
        }

        let mut check = self.store.get_session(id)?;
        check.apply(SessionTransition::StartTurn)?;
        if blank && attachments.is_empty() && check.queued.is_empty() {
            return Err(CoreError::other("write a message or attach a file"));
        }
        let mut batch = Vec::new();
        let session = self.store.mutate_session(id, |s| {
            s.apply(SessionTransition::StartTurn)?;
            batch = s.take_queued();
            if !blank || !attachments.is_empty() {
                batch.push(
                    SessionTurn::new(SessionTurnKind::User, text.trim())
                        .with_attachments(attachments.clone()),
                );
            }
            for t in &batch {
                s.push_turn(t.clone());
            }
            Ok(())
        })?;
        self.unstage(id, &staged)?;
        self.emit_session(session.clone());
        let resume = session.last_session.clone();
        self.launch_session_turn(&session, queued_input(&batch), resume)
            .await
    }

    /// Hand `text` to the running turn's agent. Whether it took it — it then
    /// joins the conversation at once.
    async fn steer_session(&self, id: Uuid, text: &str) -> bool {
        let Some(control) = lock(&self.session_runs).get(&id).map(|(_, c)| c.clone()) else {
            return false;
        };
        let (tx, rx) = futures::channel::oneshot::channel();
        let sent = control.unbounded_send(RunControl::Message {
            text: text.to_string(),
            accepted: tx,
        });
        if sent.is_err() {
            return false;
        }
        // A dropped sender (a placeholder slot, a run that just ended) is a no.
        let accepted = matches!(
            tokio::time::timeout(LIVE_ACCEPT_TIMEOUT, rx).await,
            Ok(Ok(true))
        );
        if !accepted {
            return false;
        }
        // The agent has it whatever happens next, so record it even if the
        // turn ended meanwhile.
        if let Ok(session) = self.store.mutate_session(id, |s| {
            s.push_turn(SessionTurn::new(SessionTurnKind::User, text));
            Ok(())
        }) {
            self.emit_session(session);
        }
        transcript(
            &self.store,
            &self.evt_tx,
            id,
            "↪ sent to the running agent".into(),
        );
        true
    }

    pub(super) fn unqueue_session_message(&self, id: Uuid, ts: i64) -> Result<()> {
        let session = self.store.mutate_session(id, |s| {
            s.unqueue(ts);
            Ok(())
        })?;
        self.emit_session(session);
        Ok(())
    }

    pub(super) async fn answer_session(&self, id: Uuid, text: String) -> Result<()> {
        let mut probe = self.store.get_session(id)?;
        let question = match &probe.status {
            SessionStatus::NeedsAnswer(iv) => iv.question.clone(),
            _ => return Err(CoreError::other("this session has no pending question")),
        };
        probe.apply(SessionTransition::Answer)?;
        if text.trim().is_empty() {
            return Err(CoreError::other("write an answer"));
        }
        let staged = self.store.get_attachments(id).unwrap_or_default();
        let attachments = existing(&staged);
        let session = self.store.mutate_session(id, |s| {
            s.apply(SessionTransition::Answer)?;
            s.push_turn(
                SessionTurn::new(SessionTurnKind::Answer, text.trim())
                    .with_attachments(attachments.clone()),
            );
            Ok(())
        })?;
        self.unstage(id, &staged)?;
        self.emit_session(session.clone());

        // An interactive run (the simulator) is still parked on the question:
        // hand it the answer and let it carry on.
        if let Some((_, control)) = lock(&self.session_runs).get(&id) {
            let _ = control.unbounded_send(RunControl::Answer {
                text: text.trim().to_string(),
            });
            return Ok(());
        }
        // A one-shot CLI run is gone: resume with the answer as the next turn.
        let input = TurnInput {
            prompt: answer_prompt(&question, &text, &attachments),
            attachments,
        };
        let resume = session.last_session.clone();
        self.launch_session_turn(&session, input, resume).await
    }

    pub(super) fn cancel_session(&self, id: Uuid) -> Result<()> {
        let mut probe = self.store.get_session(id)?;
        probe.apply(SessionTransition::Cancelled)?;
        // Drop the run first so its actor ignores whatever it still emits.
        if let Some((_, control)) = lock(&self.session_runs).remove(&id) {
            let _ = control.unbounded_send(RunControl::Cancel);
        }
        let session = self.store.mutate_session(id, |s| {
            s.apply(SessionTransition::Cancelled)?;
            s.push_turn(SessionTurn::new(SessionTurnKind::Error, "Stopped by user"));
            Ok(())
        })?;
        self.emit_session(session);
        transcript(&self.store, &self.evt_tx, id, "■ stopped by user".into());
        Ok(())
    }

    pub(super) async fn retry_session(&self, id: Uuid) -> Result<()> {
        let session = self.store.get_session(id)?;
        if !matches!(session.status, SessionStatus::Failed { .. }) {
            return Err(CoreError::other("only a failed session can be retried"));
        }
        // The failed turn may have carried several messages (a queued batch,
        // or a task plus a live-steered message): re-send them all.
        let batch = last_batch(&session.turns);
        if batch.is_empty() {
            return Err(CoreError::other("there is no message to re-send"));
        }
        let mut input = batch_input(&session.turns, batch);
        let mut queued = Vec::new();
        let session = self.store.mutate_session(id, |s| {
            s.apply(SessionTransition::StartTurn)?;
            s.last_activity_at = now_millis();
            queued = s.take_queued();
            for t in &queued {
                s.push_turn(t.clone());
            }
            Ok(())
        })?;
        self.emit_session(session.clone());
        let resume = session.last_session.clone();
        if !queued.is_empty() {
            // The messages queued behind the failed turn follow it.
            let more = queued_input(&queued);
            input.prompt = format!(
                "{}\n\nThen, sent while you were working:\n\n{}",
                input.prompt, more.prompt
            );
            input.attachments.extend(more.attachments);
        }
        self.launch_session_turn(&session, input, resume).await
    }

    pub(super) fn rename_session(&self, id: Uuid, title: String) -> Result<()> {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Ok(());
        }
        let session = self.store.mutate_session(id, |s| {
            s.title = truncate_chars(&title, 200);
            Ok(())
        })?;
        self.emit_session(session);
        Ok(())
    }

    pub(super) fn archive_session(&self, id: Uuid, archived: bool) -> Result<()> {
        let session = self.store.mutate_session(id, |s| {
            s.archived = archived;
            Ok(())
        })?;
        self.emit_session(session);
        Ok(())
    }

    pub(super) fn mark_session_read(&self, id: Uuid) -> Result<()> {
        // Only a real change is worth an event: selection fires this on every
        // open.
        if !self.store.get_session(id)?.unread {
            return Ok(());
        }
        let session = self.store.mutate_session(id, |s| {
            s.unread = false;
            Ok(())
        })?;
        self.emit_session(session);
        Ok(())
    }

    pub(super) fn delete_session(&self, id: Uuid) -> Result<()> {
        if let Some((_, control)) = lock(&self.session_runs).remove(&id) {
            let _ = control.unbounded_send(RunControl::Cancel);
        }
        self.store.delete_session(id)?;
        // The managed copies only — never the session's folder.
        let _ = std::fs::remove_dir_all(crate::infra::paths::attachments_dir(id));
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::session_deleted(id));
        Ok(())
    }

    /// Take `staged` off the stage (chips included) once the message carrying
    /// those files is persisted. Done only then: a message rejected on the way keeps its files
    /// staged instead of stranding them in the attachments dir.
    fn unstage(&self, id: Uuid, staged: &[PathBuf]) -> Result<()> {
        let remaining = self.store.unstage_attachments(id, staged)?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::attachments_changed(id, remaining));
        Ok(())
    }

    fn emit_session(&self, session: Session) {
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::session_updated(session));
    }

    /// Start one turn of `session` (already persisted as `Running`) and spawn
    /// its actor. A provider that fails to start fails the session.
    async fn launch_session_turn(
        &self,
        session: &Session,
        input: TurnInput,
        resume: Option<String>,
    ) -> Result<()> {
        let id = session.id;
        let mode = match session.access {
            SessionAccess::FullAccess => RunMode::SessionFull,
            SessionAccess::ReadOnly => RunMode::SessionReadOnly,
        };
        let cfg = RunConfig {
            provider: session.provider,
            project_dir: session.folder.clone(),
            spec: session.spec.clone(),
            mode,
            session_id: id,
            prompt: input.prompt.clone(),
            // Claude gets it as a system prompt (see `claude::build_args`).
            extra_prompt: (session.provider == Provider::Codex)
                .then(|| SESSION_STATUS_PROMPT.to_string()),
            resume_session: resume.clone(),
            attachments: input.attachments.clone(),
        };
        if resume.is_some() {
            transcript(
                &self.store,
                &self.evt_tx,
                id,
                "▶ resuming the conversation".into(),
            );
        } else {
            transcript(&self.store, &self.evt_tx, id, "▶ starting".into());
        }
        let provider = self.providers.make(session.provider);
        let interactive = provider.interactive();
        // Reserve the run slot before starting: a Stop (or Delete) pressed
        // while the provider spins up removes it, and the launch below then
        // kills the fresh process instead of letting it work unseen.
        let run_id = Uuid::new_v4();
        let (placeholder, _) = mpsc::unbounded();
        lock(&self.session_runs).insert(id, (run_id, placeholder));
        let handle = match provider.start(cfg).await {
            Ok(h) => h,
            Err(e) => {
                if is_current(&self.session_runs, id, run_id) {
                    release_run(&self.session_runs, id, run_id);
                    fail_session(&self.store, &self.evt_tx, id, e.to_string());
                    return Err(e);
                }
                return Ok(());
            }
        };
        {
            let mut runs = lock(&self.session_runs);
            match runs.get_mut(&id) {
                Some((rid, control)) if *rid == run_id => *control = handle.control,
                _ => {
                    drop(runs);
                    let _ = handle.control.unbounded_send(RunControl::Cancel);
                    return Ok(());
                }
            }
        }
        tokio::spawn(run_session_actor(SessionRun {
            session_id: id,
            run_id,
            events: handle.events,
            input,
            resumed: resume.is_some(),
            interactive,
            executor: self.self_ref.clone(),
            store: self.store.clone(),
            evt_tx: self.evt_tx.clone(),
            session_runs: self.session_runs.clone(),
        }));
        Ok(())
    }

    /// The `--resume` of a turn failed: relaunch it fresh, carrying a condensed
    /// history of the conversation so far plus the same message.
    ///
    /// Returns a boxed future: the actor that calls this is itself spawned by
    /// the launch it ends up in, and an unboxed `async fn` would make that a
    /// recursive opaque type.
    fn relaunch_session_fresh(&self, id: Uuid, input: TurnInput) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.relaunch_session_fresh_inner(id, input).await })
    }

    /// A turn ended with messages queued (already moved into the conversation,
    /// the session still `Running`): resume the conversation with them.
    /// Boxed for the same reason as [`Self::relaunch_session_fresh`].
    fn continue_session_queue(&self, id: Uuid, input: TurnInput) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let session = match self.store.get_session(id) {
                Ok(s) if s.is_running() => s,
                _ => return,
            };
            let resume = session.last_session.clone();
            if let Err(e) = self.launch_session_turn(&session, input, resume).await {
                tracing::warn!("session {id}: queued turn failed to start: {e}");
            }
        })
    }

    async fn relaunch_session_fresh_inner(&self, id: Uuid, input: TurnInput) {
        let session = match self.store.get_session(id) {
            Ok(s) if s.is_running() => s,
            _ => return,
        };
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            id,
            Severity::Warning,
            "Previous conversation couldn't be resumed — continuing with a summary.",
        ));
        let prompt = fresh_prompt(&session, &input);
        let input = TurnInput {
            prompt,
            attachments: input.attachments,
        };
        if let Err(e) = self.launch_session_turn(&session, input, None).await {
            tracing::warn!("session {id}: fresh relaunch failed: {e}");
        }
    }
}

/// The staged files still on disk.
fn existing(staged: &[PathBuf]) -> Vec<PathBuf> {
    staged.iter().filter(|p| p.exists()).cloned().collect()
}

/// The prompt of a user message: its text (or a stand-in when it is only
/// files), followed by the list of attached files.
fn message_prompt(text: &str, attachments: &[PathBuf]) -> String {
    let text = text.trim();
    let base = if text.is_empty() {
        ATTACHMENTS_ONLY_PROMPT.to_string()
    } else {
        text.to_string()
    };
    if attachments.is_empty() {
        base
    } else {
        append_attachments(base, attachments)
    }
}

/// What a turn carrying `turns` (user messages, oldest first) sends: a single
/// message as is, several numbered under [`QUEUED_HEADER`], with every file
/// still on disk listed once at the end.
fn queued_input(turns: &[SessionTurn]) -> TurnInput {
    let attachments: Vec<PathBuf> = turns
        .iter()
        .flat_map(|t| t.attachments.iter())
        .filter(|p| p.exists())
        .cloned()
        .collect();
    let prompt = match turns {
        [one] => message_prompt(&one.text, &attachments),
        many => {
            let mut out = QUEUED_HEADER.to_string();
            for (i, t) in many.iter().enumerate() {
                let text = match t.text.trim() {
                    "" => ATTACHMENTS_ONLY_PROMPT,
                    text => text,
                };
                out.push_str(&format!("\n\n{}. {text}", i + 1));
            }
            if attachments.is_empty() {
                out
            } else {
                append_attachments(out, &attachments)
            }
        }
    };
    TurnInput {
        prompt,
        attachments,
    }
}

fn is_message(t: &SessionTurn) -> bool {
    matches!(t.kind, SessionTurnKind::User | SessionTurnKind::Answer)
}

/// The turns the latest turn sent: the run of User/Answer turns ending at the
/// last one — several when queued messages went out together or a message was
/// steered in mid-turn. Trailing turns after it (a failed turn's error) are not
/// part of it. Empty when no message was ever sent.
fn last_batch(turns: &[SessionTurn]) -> std::ops::Range<usize> {
    let Some(last) = turns.iter().rposition(is_message) else {
        return turns.len()..turns.len();
    };
    let start = turns[..last]
        .iter()
        .rposition(|t| !is_message(t))
        .map_or(0, |i| i + 1);
    start..last + 1
}

/// What re-sending `turns[batch]` takes: a lone answer quotes its question,
/// anything else goes as [`queued_input`] does.
fn batch_input(turns: &[SessionTurn], batch: std::ops::Range<usize>) -> TurnInput {
    let start = batch.start;
    let sent = &turns[batch];
    match sent {
        [one] if one.kind == SessionTurnKind::Answer => {
            let attachments = existing(&one.attachments);
            let question = turns[..start]
                .iter()
                .rev()
                .find(|t| t.kind == SessionTurnKind::Question)
                .map(|t| t.text.clone())
                .unwrap_or_default();
            TurnInput {
                prompt: answer_prompt(&question, &one.text, &attachments),
                attachments,
            }
        }
        _ => queued_input(sent),
    }
}

fn answer_prompt(question: &str, text: &str, attachments: &[PathBuf]) -> String {
    let base = format!("Answer to your question \"{question}\": {}", text.trim());
    if attachments.is_empty() {
        base
    } else {
        append_attachments(base, attachments)
    }
}

/// The prompt of a fresh run standing in for a failed resume: the last few
/// turns before the current message (each truncated), the earlier files that
/// still exist, then the message itself.
fn fresh_prompt(session: &Session, input: &TurnInput) -> String {
    // The current message is the last batch; history is what came before.
    let earlier = &session.turns[..last_batch(&session.turns).start];
    if earlier.is_empty() {
        return input.prompt.clone();
    }
    let mut out = String::from(
        "This continues an earlier conversation that couldn't be resumed. The most recent \
         exchanges, condensed:\n",
    );
    for t in earlier
        .iter()
        .skip(earlier.len().saturating_sub(HISTORY_TURNS))
    {
        let who = match t.kind {
            SessionTurnKind::User => "User",
            SessionTurnKind::Agent => "Assistant",
            SessionTurnKind::Question => "Assistant (question)",
            SessionTurnKind::Answer => "User (answer)",
            SessionTurnKind::Error => "Note",
        };
        out.push_str(&format!(
            "\n{who}: {}",
            truncate_chars(t.text.trim(), HISTORY_TURN_CHARS)
        ));
    }
    let previous: Vec<&Path> = earlier
        .iter()
        .flat_map(|t| t.attachments.iter())
        .map(PathBuf::as_path)
        .filter(|p| p.exists())
        .collect();
    if !previous.is_empty() {
        out.push_str("\n\nPreviously attached (still available):");
        for p in previous {
            out.push_str("\n- ");
            out.push_str(&p.to_string_lossy());
        }
    }
    out.push_str("\n\nNew message:\n\n");
    out.push_str(&input.prompt);
    out
}

/// Everything a session run's actor needs.
struct SessionRun {
    session_id: Uuid,
    run_id: Uuid,
    events: BoxStream<'static, AgentEvent>,
    input: TurnInput,
    resumed: bool,
    interactive: bool,
    // A `Weak` upgrade for the fresh-relaunch and queued-turn continuations, never a command
    // through `cmd_tx`: the dispatcher's per-id exclusive claim may still be
    // held by the command that started this run, so a queued command could be
    // dropped (see `finalize_run`).
    executor: Weak<Executor>,
    store: Store,
    evt_tx: UnboundedSender<ExecutorEvent>,
    session_runs: RunMap,
}

fn is_current(runs: &RunMap, id: Uuid, run_id: Uuid) -> bool {
    lock(runs)
        .get(&id)
        .map(|(rid, _)| *rid == run_id)
        .unwrap_or(false)
}

/// Pump one session turn: progress goes to the session's transcript, and the
/// turn's ending (answer, question, error) lands on the session and flags it
/// unread. Events from a run that was cancelled or replaced are ignored.
async fn run_session_actor(run: SessionRun) {
    let SessionRun {
        session_id: id,
        run_id,
        mut events,
        input,
        resumed,
        interactive,
        executor,
        store,
        evt_tx,
        session_runs,
    } = run;
    let mut saw_output = false;
    // A read-only Claude run can surface its answer as an `ExitPlanMode`
    // plan; keep it in case the final result comes back empty.
    let mut plan: Option<String> = None;
    let mut settled = false;
    // Whether the agent reported a `usine-status:` line this turn; until it
    // does, its todo list's current item stands in.
    let mut reported = false;
    loop {
        let evt = if interactive {
            match events.next().await {
                Some(evt) => evt,
                None => break,
            }
        } else {
            match tokio::time::timeout(SESSION_IDLE_TIMEOUT, events.next()).await {
                Ok(Some(evt)) => evt,
                Ok(None) => break,
                Err(_) => {
                    if let Some((rid, control)) = lock(&session_runs).get(&id) {
                        if *rid == run_id {
                            let _ = control.unbounded_send(RunControl::Cancel);
                        }
                    }
                    AgentEvent::Error {
                        message: format!(
                            "the agent produced no output for {} min",
                            SESSION_IDLE_TIMEOUT.as_secs() / 60
                        ),
                    }
                }
            }
        };
        if !is_current(&session_runs, id, run_id) {
            // Stopped, deleted or superseded: whatever this run still says is
            // stale.
            return;
        }
        match evt {
            AgentEvent::Started { session_id } => {
                let _ = store.mutate_session(id, |s| {
                    s.last_session = Some(session_id.clone());
                    Ok(())
                });
                transcript(&store, &evt_tx, id, format!("● session {session_id}"));
            }
            AgentEvent::Progress { text } => {
                saw_output = true;
                let note = match last_status_line(&text) {
                    Some(note) => {
                        reported = true;
                        Some(note)
                    }
                    None if !reported => text
                        .strip_prefix(TODO_PROGRESS_PREFIX)
                        .map(|item| truncate_chars(item.trim(), STATUS_MAX)),
                    None => None,
                };
                if let Some(note) = note {
                    let updated = store.mutate_session(id, |s| {
                        if s.is_running() {
                            s.status_note = Some(StatusNote {
                                text: note,
                                ts: now_millis(),
                            });
                        }
                        Ok(())
                    });
                    if let Ok(s) = updated {
                        let _ = evt_tx.unbounded_send(ExecutorEvent::session_updated(s));
                    }
                }
                transcript(&store, &evt_tx, id, text);
            }
            AgentEvent::PlanReady { plan: p } => plan = Some(p),
            AgentEvent::NeedsInput {
                request_id,
                question,
                options,
            } => {
                let iv = Intervention {
                    request_id,
                    question: question.clone(),
                    options,
                };
                let updated = store.mutate_session(id, |s| {
                    s.apply(SessionTransition::NeedsInput(iv))?;
                    s.push_turn(SessionTurn::new(SessionTurnKind::Question, question.trim()));
                    s.unread = true;
                    Ok(())
                });
                if let Ok(s) = updated {
                    let _ = evt_tx.unbounded_send(ExecutorEvent::session_updated(s));
                }
                if !interactive {
                    // One-shot CLI: the answer resumes in a new run.
                    if let Some((_, control)) = lock(&session_runs).get(&id) {
                        let _ = control.unbounded_send(RunControl::Cancel);
                    }
                    release_run(&session_runs, id, run_id);
                    return;
                }
            }
            AgentEvent::Done {
                result, cost_usd, ..
            } => {
                let result = strip_status_lines(&result);
                let text = if result.trim().is_empty() {
                    plan.take().unwrap_or_default()
                } else {
                    result
                };
                // Messages queued during the turn go out at once as the next
                // turn, in the same write: the session never leaves Running,
                // so nothing pings in between.
                let mut next: Option<Vec<SessionTurn>> = None;
                let updated = store.mutate_session(id, |s| {
                    s.apply(SessionTransition::TurnDone)?;
                    s.push_turn(SessionTurn::new(SessionTurnKind::Agent, text.trim()));
                    s.cost += Cost::from_usd(cost_usd);
                    let queued = s.take_queued();
                    if queued.is_empty() {
                        s.unread = true;
                    } else {
                        s.apply(SessionTransition::StartTurn)?;
                        for t in &queued {
                            s.push_turn(t.clone());
                        }
                        next = Some(queued);
                    }
                    Ok(())
                });
                if let Ok(s) = updated {
                    let _ = evt_tx.unbounded_send(ExecutorEvent::session_updated(s));
                }
                if let Some(queued) = next {
                    transcript(
                        &store,
                        &evt_tx,
                        id,
                        format!(
                            "✔ turn finished — sending {} queued message(s)",
                            queued.len()
                        ),
                    );
                    // Free the slot for the next run's launch; a direct call,
                    // never `cmd_tx` (see `SessionRun::executor`).
                    release_run(&session_runs, id, run_id);
                    if let Some(exec) = executor.upgrade() {
                        exec.continue_session_queue(id, queued_input(&queued)).await;
                    }
                    return;
                }
                transcript(&store, &evt_tx, id, "✔ finished".into());
                settled = true;
                break;
            }
            AgentEvent::Error { message } => {
                // A resume that dies before saying anything is a resume
                // failure (a pruned or unknown CLI session): carry on fresh.
                if resumed && !saw_output {
                    release_run(&session_runs, id, run_id);
                    transcript(
                        &store,
                        &evt_tx,
                        id,
                        format!("⚠ resume failed ({message}) — starting fresh"),
                    );
                    if let Some(exec) = executor.upgrade() {
                        exec.relaunch_session_fresh(id, input).await;
                    }
                    return;
                }
                fail_session(&store, &evt_tx, id, message);
                settled = true;
                break;
            }
        }
    }
    // The stream ended without a verdict (the process died quietly).
    if !settled && is_current(&session_runs, id, run_id) {
        fail_session(
            &store,
            &evt_tx,
            id,
            "the agent exited without a result".to_string(),
        );
    }
    release_run(&session_runs, id, run_id);
}

/// Fail the session's running turn, flagging it unread. A no-op if the
/// session already left `Running` (stopped meanwhile).
fn fail_session(store: &Store, evt_tx: &UnboundedSender<ExecutorEvent>, id: Uuid, message: String) {
    let updated = store.mutate_session(id, |s| {
        s.apply(SessionTransition::TurnFailed {
            message: message.clone(),
        })?;
        s.push_turn(SessionTurn::new(SessionTurnKind::Error, message.clone()));
        s.unread = true;
        Ok(())
    });
    if let Ok(s) = updated {
        let _ = evt_tx.unbounded_send(ExecutorEvent::session_updated(s));
    }
    transcript(store, evt_tx, id, format!("✖ {message}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Effort;

    #[test]
    fn message_prompt_lists_files_and_stands_in_for_empty_text() {
        let files = vec![PathBuf::from("/a/1.png")];
        let p = message_prompt("  ", &files);
        assert!(p.starts_with(ATTACHMENTS_ONLY_PROMPT));
        assert!(p.contains("/a/1.png"));
        assert_eq!(message_prompt(" hi ", &[]), "hi");
    }

    #[test]
    fn fresh_prompt_condenses_history_before_the_new_message() {
        let mut s = Session::new(
            Uuid::new_v4(),
            PathBuf::from("/tmp"),
            Provider::Claude,
            ModelSpec::new("opus", Effort::High),
            SessionAccess::FullAccess,
            "first question",
            vec![],
        );
        s.push_turn(SessionTurn::new(SessionTurnKind::Agent, "x".repeat(2000)));
        s.push_turn(SessionTurn::new(SessionTurnKind::User, "follow up"));
        let input = TurnInput {
            prompt: "follow up".into(),
            attachments: vec![],
        };
        let p = fresh_prompt(&s, &input);
        assert!(p.contains("User: first question"));
        assert!(p.contains("Assistant: xxx"));
        assert!(p.len() < 1500, "agent turn is truncated");
        assert!(p.ends_with("New message:\n\nfollow up"));
    }

    #[test]
    fn retry_batch_is_every_message_of_the_failed_turn() {
        let turns = vec![
            SessionTurn::new(SessionTurnKind::User, "old"),
            SessionTurn::new(SessionTurnKind::Agent, "done"),
            SessionTurn::new(SessionTurnKind::User, "the task"),
            SessionTurn::new(SessionTurnKind::User, "steer: use tabs"),
            SessionTurn::new(SessionTurnKind::Error, "boom"),
        ];
        assert_eq!(last_batch(&turns), 2..4);
        let input = batch_input(&turns, last_batch(&turns));
        assert!(input.prompt.contains("1. the task"));
        assert!(input.prompt.contains("2. steer: use tabs"));
        assert!(!input.prompt.contains("old"));

        let answered = vec![
            SessionTurn::new(SessionTurnKind::User, "go"),
            SessionTurn::new(SessionTurnKind::Question, "Which db?"),
            SessionTurn::new(SessionTurnKind::Answer, "sqlite"),
            SessionTurn::new(SessionTurnKind::Error, "boom"),
        ];
        let input = batch_input(&answered, last_batch(&answered));
        assert!(input.prompt.contains("\"Which db?\": sqlite"));
        assert!(last_batch(&[]).is_empty());
    }
}
