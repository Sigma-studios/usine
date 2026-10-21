//! One-off sessions: ad-hoc Claude/Codex chat threads started in any folder.
//!
//! A session is deliberately not a card — no project, git, worktree, PR or
//! column. Each user message is one headless turn, and every turn after the
//! first resumes the CLI conversation. The status is a tiny state machine of
//! its own ([`Session::apply`]); cards and sessions share no states.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::{normalize_blocked_note, now_millis, Cost, Intervention, ModelSpec, Provider};
use crate::error::{CoreError, Result};

/// Longest derived title, in chars.
const TITLE_MAX: usize = 80;

/// What a session's agent may do in its folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SessionAccess {
    /// Edit files and run commands without asking (a terminal `claude
    /// --dangerously-skip-permissions`).
    #[default]
    FullAccess,
    /// Inspect only (`--permission-mode plan` / a read-only sandbox).
    ReadOnly,
}

impl SessionAccess {
    pub fn label(self) -> &'static str {
        match self {
            SessionAccess::FullAccess => "Full access",
            SessionAccess::ReadOnly => "Read-only",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    /// A turn is in flight.
    Running,
    /// The agent asked a question; the one-shot run was torn down to wait for
    /// the answer, which starts a resumed turn.
    NeedsAnswer(Intervention),
    /// The last turn ended normally (or was stopped by the user).
    Finished,
    /// The turn errored, or Usine quit while it was running. Retryable.
    Failed { message: String },
}

impl SessionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            SessionStatus::Running => "Running",
            SessionStatus::NeedsAnswer(_) => "Needs answer",
            SessionStatus::Finished => "Finished",
            SessionStatus::Failed { .. } => "Failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionTurnKind {
    User,
    Agent,
    Question,
    Answer,
    Error,
}

/// One entry of a session's conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub ts: i64,
    pub kind: SessionTurnKind,
    pub text: String,
    /// Managed copies of the files sent with this turn (User/Answer turns
    /// only). Sent once; the resumed conversation already holds them.
    #[serde(default)]
    pub attachments: Vec<PathBuf>,
}

impl SessionTurn {
    pub fn new(kind: SessionTurnKind, text: impl Into<String>) -> Self {
        SessionTurn {
            ts: now_millis(),
            kind,
            text: text.into(),
            attachments: Vec::new(),
        }
    }

    pub fn with_attachments(mut self, attachments: Vec<PathBuf>) -> Self {
        self.attachments = attachments;
        self
    }
}

/// The agent's latest self-reported progress line (`usine-status: …`), shown
/// in place of a bare "Working…" while a turn runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusNote {
    pub text: String,
    pub ts: i64,
}

/// A status change, applied by [`Session::apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionTransition {
    /// A new user message starts a turn.
    StartTurn,
    /// The user's answer to the pending question starts a turn.
    Answer,
    NeedsInput(Intervention),
    TurnDone,
    TurnFailed {
        message: String,
    },
    /// The user stopped the session.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: Uuid,
    /// First line of the first prompt, ≤ 80 chars; renamable.
    pub title: String,
    /// Any absolute directory; need not be a git repo or a project.
    pub folder: PathBuf,
    pub provider: Provider,
    pub spec: ModelSpec,
    #[serde(default)]
    pub access: SessionAccess,
    pub status: SessionStatus,
    #[serde(default)]
    pub turns: Vec<SessionTurn>,
    /// CLI session id from the latest run's `Started`, for `--resume`.
    pub last_session: Option<String>,
    pub created_at: i64,
    pub last_activity_at: i64,
    /// A turn ended while the user wasn't looking.
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub archived: bool,
    /// The user marked it blocked: it stays out of the attention counts until
    /// they unblock it. Running a turn never clears it.
    #[serde(default)]
    pub blocked: bool,
    /// The optional message the user typed when marking it blocked; only set
    /// while `blocked`.
    #[serde(default)]
    pub blocked_note: Option<String>,
    #[serde(rename = "cost_usd", default)]
    pub cost: Cost,
    /// What the agent last said it is doing; cleared whenever a turn starts.
    #[serde(default)]
    pub status_note: Option<StatusNote>,
    /// Messages the user sent while a turn ran, waiting for it to end: they go
    /// out together as the next resumed turn.
    #[serde(default)]
    pub queued: Vec<SessionTurn>,
}

impl Session {
    /// A new session whose first turn is `prompt` (+ `attachments`). Starts
    /// `Running`: creating a session always launches its first turn.
    pub fn new(
        id: Uuid,
        folder: PathBuf,
        provider: Provider,
        spec: ModelSpec,
        access: SessionAccess,
        prompt: &str,
        attachments: Vec<PathBuf>,
    ) -> Self {
        let now = now_millis();
        let title = derive_title(prompt, &attachments);
        Session {
            id,
            title,
            folder,
            provider,
            spec,
            access,
            status: SessionStatus::Running,
            turns: vec![SessionTurn::new(SessionTurnKind::User, prompt.trim())
                .with_attachments(attachments)],
            last_session: None,
            created_at: now,
            last_activity_at: now,
            unread: false,
            archived: false,
            blocked: false,
            blocked_note: None,
            cost: Cost::ZERO,
            status_note: None,
            queued: Vec::new(),
        }
    }

    /// Set (or clear) the user's blocked marker and its message, like
    /// [`Card::set_blocked`](super::model::Card::set_blocked).
    pub fn set_blocked(&mut self, blocked: bool, note: Option<String>) {
        self.blocked = blocked;
        self.blocked_note = normalize_blocked_note(blocked, note);
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, SessionStatus::Running)
    }

    /// Whether the session counts toward the attention badges: it sits idle
    /// (ended, failed, or asking a question) until the user archives, deletes
    /// or blocks it. Reading it doesn't clear it; a running turn doesn't count.
    pub fn needs_attention(&self) -> bool {
        !self.archived && !self.blocked && !self.is_running()
    }

    /// An attention-worthy session asking the user a question — the urgent
    /// case.
    pub fn needs_answer(&self) -> bool {
        self.needs_attention() && matches!(self.status, SessionStatus::NeedsAnswer(_))
    }

    /// The last turn the user sent (a message or an answer) — what Retry
    /// re-sends.
    pub fn last_resendable_turn(&self) -> Option<&SessionTurn> {
        self.turns
            .iter()
            .rev()
            .find(|t| matches!(t.kind, SessionTurnKind::User | SessionTurnKind::Answer))
    }

    /// The last turn of `kind`, if any.
    pub fn last_turn_of(&self, kind: SessionTurnKind) -> Option<&SessionTurn> {
        self.turns.iter().rev().find(|t| t.kind == kind)
    }

    pub fn push_turn(&mut self, turn: SessionTurn) {
        self.last_activity_at = turn.ts.max(self.last_activity_at);
        self.turns.push(turn);
    }

    /// Hold a message sent while a turn runs, for the next turn. Refused
    /// unless a turn is running: an idle session sends it right away. Its
    /// stamp is made unique in the queue (it keys the UI row and
    /// [`Session::unqueue`]), nudged a millisecond past the latest if needed.
    pub fn queue(&mut self, mut turn: SessionTurn) -> Result<()> {
        if !self.is_running() {
            return Err(illegal("nothing is running to queue behind"));
        }
        if let Some(latest) = self.queued.iter().map(|t| t.ts).max() {
            turn.ts = turn.ts.max(latest + 1);
        }
        self.last_activity_at = turn.ts.max(self.last_activity_at);
        self.queued.push(turn);
        Ok(())
    }

    /// Drop the queued message stamped `ts`. Whether one was removed.
    pub fn unqueue(&mut self, ts: i64) -> bool {
        let before = self.queued.len();
        self.queued.retain(|t| t.ts != ts);
        self.queued.len() != before
    }

    /// Empty the queue, oldest message first.
    pub fn take_queued(&mut self) -> Vec<SessionTurn> {
        std::mem::take(&mut self.queued)
    }

    /// Apply a status transition. Only the status moves here; turns, unread
    /// and cost are the executor's business.
    pub fn apply(&mut self, t: SessionTransition) -> Result<()> {
        use SessionStatus as S;
        use SessionTransition as T;
        let next = match (&self.status, t) {
            (S::Running, T::StartTurn | T::Answer) => {
                return Err(illegal("a turn is already running"));
            }
            (S::NeedsAnswer(_), T::StartTurn) => {
                return Err(illegal("the agent is waiting for an answer"));
            }
            (S::NeedsAnswer(_), T::Answer) => S::Running,
            (_, T::Answer) => return Err(illegal("there is no question to answer")),
            (S::Finished | S::Failed { .. }, T::StartTurn) => S::Running,
            (S::Running, T::NeedsInput(i)) => S::NeedsAnswer(i),
            (S::Running, T::TurnDone) => S::Finished,
            (S::Running, T::TurnFailed { message }) => S::Failed { message },
            (S::Running | S::NeedsAnswer(_), T::Cancelled) => S::Finished,
            (s, t) => {
                return Err(illegal(&format!(
                    "{t:?} while {}",
                    s.label().to_lowercase()
                )))
            }
        };
        if matches!(next, S::Running) {
            // A new turn: the previous one's progress line is stale.
            self.status_note = None;
        }
        self.status = next;
        Ok(())
    }
}

fn illegal(msg: &str) -> CoreError {
    CoreError::IllegalTransition(format!("session: {msg}"))
}

/// The title a new session gets: the first non-empty prompt line, else the
/// first attached file's name, else a placeholder.
pub fn derive_title(prompt: &str, attachments: &[PathBuf]) -> String {
    if let Some(line) = prompt.lines().map(str::trim).find(|l| !l.is_empty()) {
        return truncate_chars(line, TITLE_MAX);
    }
    if let Some(first) = attachments.first() {
        return truncate_chars(&format!("Attached: {}", file_label(first)), TITLE_MAX);
    }
    "Untitled session".to_string()
}

/// A managed attachment's user-facing name: the `<8 hex>-` uniqueness prefix
/// stripped (mirrors `infra::paths::attachment_label`, kept here so the domain
/// stays I/O-free).
fn file_label(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.split_once('-') {
        Some((prefix, original)) if prefix.len() == 8 => original.to_string(),
        _ => name,
    }
}

/// The marker opening an agent's progress line (see [`StatusNote`]).
pub const STATUS_PREFIX: &str = "usine-status:";

/// Longest status note kept, in chars.
pub const STATUS_MAX: usize = 80;

/// The text of a `usine-status:` line, if `line` is one (case-insensitive,
/// surrounding backticks tolerated).
fn status_of(line: &str) -> Option<&str> {
    let line = line.trim().trim_matches('`').trim();
    let head = line.get(..STATUS_PREFIX.len())?;
    head.eq_ignore_ascii_case(STATUS_PREFIX)
        .then(|| line[STATUS_PREFIX.len()..].trim())
}

/// The last non-empty `usine-status:` line in `text`, trimmed and cut to
/// [`STATUS_MAX`] chars.
pub fn last_status_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .filter_map(status_of)
        .find(|s| !s.is_empty())
        .map(|s| truncate_chars(s, STATUS_MAX))
}

/// `text` without its `usine-status:` lines — they are progress, not part of
/// the agent's reply.
pub fn strip_status_lines(text: &str) -> String {
    if !text.lines().any(|l| status_of(l).is_some()) {
        return text.to_string();
    }
    text.lines()
        .filter(|l| status_of(l).is_none())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `s` cut to at most `max` chars, with an ellipsis when cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Effort;

    fn session() -> Session {
        Session::new(
            Uuid::new_v4(),
            PathBuf::from("/tmp"),
            Provider::Claude,
            ModelSpec::new("opus", Effort::High),
            SessionAccess::FullAccess,
            "  \n Fix the build\nmore",
            vec![],
        )
    }

    fn question() -> Intervention {
        Intervention {
            request_id: "r".into(),
            question: "Which?".into(),
            options: vec![],
        }
    }

    #[test]
    fn new_session_runs_its_first_turn() {
        let s = session();
        assert!(s.is_running());
        assert_eq!(s.title, "Fix the build");
        assert_eq!(s.turns.len(), 1);
        assert_eq!(s.turns[0].kind, SessionTurnKind::User);
    }

    #[test]
    fn title_falls_back_to_attachment_then_placeholder() {
        let files = vec![PathBuf::from("/x/1a2b3c4d-screen shot.png")];
        assert_eq!(derive_title("  \n ", &files), "Attached: screen shot.png");
        assert_eq!(derive_title("", &[]), "Untitled session");
        let long = "x".repeat(200);
        assert_eq!(derive_title(&long, &[]).chars().count(), TITLE_MAX);
    }

    #[test]
    fn start_turn_is_rejected_while_running_or_waiting() {
        let mut s = session();
        assert!(s.apply(SessionTransition::StartTurn).is_err());
        s.apply(SessionTransition::NeedsInput(question())).unwrap();
        assert!(s.apply(SessionTransition::StartTurn).is_err());
        s.apply(SessionTransition::Answer).unwrap();
        assert!(s.is_running());
    }

    #[test]
    fn answer_outside_needs_answer_is_rejected() {
        let mut s = session();
        s.apply(SessionTransition::TurnDone).unwrap();
        assert!(s.apply(SessionTransition::Answer).is_err());
        s.apply(SessionTransition::StartTurn).unwrap();
        assert!(s.apply(SessionTransition::Answer).is_err());
    }

    #[test]
    fn turn_outcomes_and_cancel() {
        let mut s = session();
        s.apply(SessionTransition::TurnFailed {
            message: "boom".into(),
        })
        .unwrap();
        assert_eq!(
            s.status,
            SessionStatus::Failed {
                message: "boom".into()
            }
        );
        // A stray completion after the turn settled is refused.
        assert!(s.apply(SessionTransition::TurnDone).is_err());
        s.apply(SessionTransition::StartTurn).unwrap();
        s.apply(SessionTransition::NeedsInput(question())).unwrap();
        s.apply(SessionTransition::Cancelled).unwrap();
        assert_eq!(s.status, SessionStatus::Finished);
        assert!(s.apply(SessionTransition::Cancelled).is_err());
    }

    #[test]
    fn queue_only_while_running_and_unqueue_by_stamp() {
        let mut s = session();
        let mut a = SessionTurn::new(SessionTurnKind::User, "a");
        a.ts = 1;
        let mut b = SessionTurn::new(SessionTurnKind::User, "b");
        b.ts = 2;
        s.queue(a).unwrap();
        s.queue(b.clone()).unwrap();
        assert!(s.unqueue(1));
        assert!(!s.unqueue(1));
        assert_eq!(s.queued, vec![b.clone()]);
        s.apply(SessionTransition::TurnDone).unwrap();
        assert!(s
            .queue(SessionTurn::new(SessionTurnKind::User, "c"))
            .is_err());
        assert_eq!(s.take_queued(), vec![b]);
        assert!(s.queued.is_empty());
    }

    #[test]
    fn queued_messages_of_the_same_millisecond_get_distinct_stamps() {
        let mut s = session();
        let mut a = SessionTurn::new(SessionTurnKind::User, "a");
        a.ts = 5;
        let mut b = SessionTurn::new(SessionTurnKind::User, "b");
        b.ts = 5;
        s.queue(a).unwrap();
        s.queue(b).unwrap();
        assert_eq!(s.queued[1].ts, 6);
        assert!(s.unqueue(5));
        assert_eq!(s.queued.len(), 1);
        assert_eq!(s.queued[0].text, "b");
    }

    #[test]
    fn status_lines_are_found_and_stripped() {
        let text = "Looking.\nusine-status: Reading the parser\n`USINE-STATUS: Running tests`\nok";
        assert_eq!(last_status_line(text).as_deref(), Some("Running tests"));
        assert_eq!(last_status_line("no status here"), None);
        assert_eq!(last_status_line("usine-status:   "), None);
        let long = format!("usine-status: {}", "x".repeat(200));
        assert_eq!(last_status_line(&long).unwrap().chars().count(), STATUS_MAX);
        assert_eq!(strip_status_lines(text), "Looking.\nok");
        assert_eq!(strip_status_lines("  keep  as is \n"), "  keep  as is \n");
    }

    #[test]
    fn a_new_turn_clears_the_status_note() {
        let mut s = session();
        s.status_note = Some(StatusNote {
            text: "Reading".into(),
            ts: 1,
        });
        s.apply(SessionTransition::TurnDone).unwrap();
        assert!(s.status_note.is_some(), "kept until the next turn");
        s.apply(SessionTransition::StartTurn).unwrap();
        assert!(s.status_note.is_none());
    }

    #[test]
    fn set_blocked_normalizes_and_clears_the_note() {
        let mut s = session();
        s.set_blocked(true, Some("  waiting on infra  ".into()));
        assert!(s.blocked);
        assert_eq!(s.blocked_note.as_deref(), Some("waiting on infra"));
        assert!(!s.needs_attention());

        // Re-marking replaces the message: the "edit blocked message" path.
        s.set_blocked(true, Some("waiting on review".into()));
        assert_eq!(s.blocked_note.as_deref(), Some("waiting on review"));

        // Blank is the same as none, and the marker stays.
        s.set_blocked(true, Some("   ".into()));
        assert!(s.blocked);
        assert_eq!(s.blocked_note, None);

        // Unmarking drops the message, whatever was passed.
        s.set_blocked(true, Some("stale".into()));
        s.set_blocked(false, Some("ignored".into()));
        assert!(!s.blocked);
        assert_eq!(s.blocked_note, None);
    }

    #[test]
    fn attention_and_resendable_turn() {
        let mut s = session();
        // Running: doesn't count, blocked or not.
        assert!(!s.needs_attention());
        s.blocked = true;
        assert!(!s.needs_attention());
        s.blocked = false;
        s.apply(SessionTransition::NeedsInput(question())).unwrap();
        assert!(s.needs_attention());
        assert!(s.needs_answer());
        s.archived = true;
        assert!(!s.needs_attention());
        assert!(!s.needs_answer());
        s.archived = false;
        s.blocked = true;
        assert!(!s.needs_attention());
        assert!(!s.needs_answer());
        s.blocked = false;
        // Ended: counts whether read or not, but isn't urgent.
        s.apply(SessionTransition::Cancelled).unwrap();
        assert!(s.needs_attention());
        assert!(!s.needs_answer());
        s.unread = true;
        assert!(s.needs_attention());
        s.blocked = true;
        assert!(!s.needs_attention());
        s.blocked = false;

        s.push_turn(SessionTurn::new(SessionTurnKind::Agent, "done"));
        s.push_turn(SessionTurn::new(SessionTurnKind::Answer, "Speed"));
        s.push_turn(SessionTurn::new(SessionTurnKind::Error, "Stopped by user"));
        assert_eq!(s.last_resendable_turn().unwrap().text, "Speed");
    }
}
