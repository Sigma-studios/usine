//! One-off sessions: ad-hoc Claude/Codex chat threads started in any folder.
//!
//! A session is deliberately not a card — no project, git, worktree, PR or
//! column. Each user message is one headless turn, and every turn after the
//! first resumes the CLI conversation. The status is a tiny state machine of
//! its own ([`Session::apply`]); cards and sessions share no states.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::{now_millis, Cost, Intervention, ModelSpec, Provider};
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
    #[serde(rename = "cost_usd", default)]
    pub cost: Cost,
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
            cost: Cost::ZERO,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, SessionStatus::Running)
    }

    /// Whether the session should pull the user's eye: an unread ending, or a
    /// question waiting on them. Archived sessions never do.
    pub fn needs_attention(&self) -> bool {
        !self.archived && (self.unread || matches!(self.status, SessionStatus::NeedsAnswer(_)))
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
    fn attention_and_resendable_turn() {
        let mut s = session();
        assert!(!s.needs_attention());
        s.apply(SessionTransition::NeedsInput(question())).unwrap();
        assert!(s.needs_attention());
        s.archived = true;
        assert!(!s.needs_attention());
        s.archived = false;
        s.apply(SessionTransition::Cancelled).unwrap();
        assert!(!s.needs_attention());
        s.unread = true;
        assert!(s.needs_attention());

        s.push_turn(SessionTurn::new(SessionTurnKind::Agent, "done"));
        s.push_turn(SessionTurn::new(SessionTurnKind::Answer, "Speed"));
        s.push_turn(SessionTurn::new(SessionTurnKind::Error, "Stopped by user"));
        assert_eq!(s.last_resendable_turn().unwrap().text, "Speed");
    }
}
