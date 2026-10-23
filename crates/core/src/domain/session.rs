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
    /// A message Usine itself sent the agent — the auto-resume after
    /// background agents were cut off. Re-sent by Retry like a user message.
    Notice,
    /// The user stopped the running turn. The conversation stays resumable;
    /// the next message tells the agent it was cut off.
    Interrupted,
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

/// What a background task of the session's agent is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BgTaskKind {
    /// A subagent (`Agent` tool).
    Agent,
    /// A shell command (`Bash`), the agent's own or a subagent's.
    Command,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BgTaskStatus {
    Running,
    Completed,
    Failed,
    /// Stopped on purpose (by the agent, or by the user via the agent).
    Stopped,
    /// Ended by the CLI before its work was done: still running when the
    /// process exited, stopped at a time or memory limit, or a subagent that
    /// "completed" while its own background command was still running.
    CutOff,
    /// A cut-off agent the parent launched again (a newer entry carries on).
    Relaunched,
}

impl BgTaskStatus {
    pub fn label(self) -> &'static str {
        match self {
            BgTaskStatus::Running => "running",
            BgTaskStatus::Completed => "done",
            BgTaskStatus::Failed => "failed",
            BgTaskStatus::Stopped => "stopped",
            BgTaskStatus::CutOff => "cut off",
            BgTaskStatus::Relaunched => "relaunched",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, BgTaskStatus::Running)
    }
}

/// One background task of the session's agent, as the CLI reports it
/// (`system/task_*` stream lines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BgTask {
    pub id: String,
    /// The task that launched it — a subagent's own commands name it; the
    /// parent agent's tasks have none.
    #[serde(default)]
    pub parent: Option<String>,
    pub kind: BgTaskKind,
    pub description: String,
    #[serde(default)]
    pub subagent_type: Option<String>,
    pub status: BgTaskStatus,
    /// Why the CLI stopped it (`deadline`, `memory_pressure`…), when it says.
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub last_tool: Option<String>,
    /// What it is doing, or what it reported when it ended.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub tool_uses: u64,
    pub started_at: i64,
    pub updated_at: i64,
}

/// A change to the session's background tasks, parsed from the CLI stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskEvent {
    Started {
        id: String,
        parent: Option<String>,
        kind: BgTaskKind,
        description: String,
        subagent_type: Option<String>,
        /// Run in the background rather than awaited by its launcher.
        backgrounded: bool,
    },
    Progress {
        id: String,
        last_tool: Option<String>,
        summary: Option<String>,
        tool_uses: Option<u64>,
    },
    Status {
        id: String,
        status: BgTaskStatus,
        reason: Option<String>,
        summary: Option<String>,
        tool_uses: Option<u64>,
    },
    /// The run ended with these tasks' work unfinished (see
    /// [`BgTaskStatus::CutOff`]); sent once, just before the run's end.
    CutOff { ids: Vec<String> },
}

/// The last auto-resume's cut-off agents, kept to tell a stuck loop from one
/// making progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutOffRound {
    /// The cut-off agents' descriptions, sorted.
    pub descriptions: Vec<String>,
    /// [`Session::bg_tool_uses`] when the round was sent.
    pub tool_uses: u64,
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
    /// The agent's background tasks (subagents and their commands) since the
    /// user's last message; settled ones are dropped when the next starts.
    #[serde(default)]
    pub bg_tasks: Vec<BgTask>,
    /// Set while the session auto-resumes cut-off agents (see
    /// [`Session::cut_off_stalled`]); cleared by the user's next message.
    #[serde(default)]
    pub last_cutoff: Option<CutOffRound>,
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
            bg_tasks: Vec::new(),
            last_cutoff: None,
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

impl Session {
    /// Record a background-task event. Returns whether anything changed.
    ///
    /// A settled task never goes back to running from a late progress line,
    /// except that the end of the run may still mark it cut off. A subagent
    /// whose own command is cut off is cut off with it: it reported done
    /// while that command was still at work. Commands the parent runs in the
    /// foreground aren't tracked — they never outlive its turn.
    pub fn apply_task(&mut self, event: &TaskEvent) -> bool {
        let now = now_millis();
        match event {
            TaskEvent::Started {
                id,
                parent,
                kind,
                description,
                subagent_type,
                backgrounded,
            } => {
                if *kind != BgTaskKind::Agent && parent.is_none() && !backgrounded {
                    return false;
                }
                // A cut-off agent launched again under the same description.
                if *kind == BgTaskKind::Agent {
                    for t in &mut self.bg_tasks {
                        if t.kind == BgTaskKind::Agent
                            && t.status == BgTaskStatus::CutOff
                            && t.description == *description
                            && t.id != *id
                        {
                            t.status = BgTaskStatus::Relaunched;
                            t.updated_at = now;
                        }
                    }
                }
                let task = BgTask {
                    id: id.clone(),
                    parent: parent.clone(),
                    kind: *kind,
                    description: description.clone(),
                    subagent_type: subagent_type.clone(),
                    status: BgTaskStatus::Running,
                    reason: None,
                    last_tool: None,
                    summary: None,
                    tool_uses: 0,
                    started_at: now,
                    updated_at: now,
                };
                match self.bg_tasks.iter_mut().find(|t| t.id == *id) {
                    // Resumed (`SendMessage` to a finished agent): it works again.
                    Some(t) => {
                        t.status = BgTaskStatus::Running;
                        t.reason = None;
                        t.updated_at = now;
                    }
                    None => self.bg_tasks.push(task),
                }
                true
            }
            TaskEvent::Progress {
                id,
                last_tool,
                summary,
                tool_uses,
            } => {
                let Some(t) = self.bg_tasks.iter_mut().find(|t| t.id == *id) else {
                    return false;
                };
                if last_tool.is_some() {
                    t.last_tool = last_tool.clone();
                }
                if summary.is_some() {
                    t.summary = summary.clone();
                }
                if let Some(n) = tool_uses {
                    t.tool_uses = t.tool_uses.max(*n);
                }
                t.updated_at = now;
                true
            }
            TaskEvent::Status {
                id,
                status,
                reason,
                summary,
                tool_uses,
            } => {
                let Some(t) = self.bg_tasks.iter_mut().find(|t| t.id == *id) else {
                    return false;
                };
                if t.status.is_terminal() && *status != BgTaskStatus::CutOff {
                    // A task's end is often reported twice (`task_updated`,
                    // then `task_notification`): keep what the later one adds.
                    if t.status != *status {
                        return false;
                    }
                }
                t.status = *status;
                if reason.is_some() {
                    t.reason = reason.clone();
                }
                if summary.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                    t.summary = summary.clone();
                }
                if let Some(n) = tool_uses {
                    t.tool_uses = t.tool_uses.max(*n);
                }
                t.updated_at = now;
                true
            }
            TaskEvent::CutOff { ids } => {
                let owners: Vec<String> = self
                    .bg_tasks
                    .iter()
                    .filter(|t| ids.contains(&t.id))
                    .filter_map(|t| t.parent.clone())
                    .collect();
                let mut changed = false;
                for t in &mut self.bg_tasks {
                    if (ids.contains(&t.id) || owners.contains(&t.id))
                        && t.status != BgTaskStatus::CutOff
                    {
                        t.status = BgTaskStatus::CutOff;
                        t.updated_at = now;
                        changed = true;
                    }
                }
                changed
            }
        }
    }

    /// The subagents whose work a run's end cut short: those among `ids`, and
    /// those owning a command among `ids`. The parent's own commands (a dev
    /// server it started) are not in it: they alone never warrant resuming.
    pub fn lost_agents(&self, ids: &[String]) -> Vec<&BgTask> {
        let owners: Vec<&str> = self
            .bg_tasks
            .iter()
            .filter(|t| ids.contains(&t.id))
            .filter_map(|t| t.parent.as_deref())
            .collect();
        self.bg_tasks
            .iter()
            .filter(|t| {
                t.kind == BgTaskKind::Agent
                    && t.status == BgTaskStatus::CutOff
                    && (ids.contains(&t.id) || owners.contains(&t.id.as_str()))
            })
            .collect()
    }

    /// Cut-off subagents nobody launched again yet.
    pub fn cut_off_agents(&self) -> Vec<&BgTask> {
        self.bg_tasks
            .iter()
            .filter(|t| t.kind == BgTaskKind::Agent && t.status == BgTaskStatus::CutOff)
            .collect()
    }

    /// Subagents at work right now.
    pub fn running_agents(&self) -> Vec<&BgTask> {
        self.bg_tasks
            .iter()
            .filter(|t| t.kind == BgTaskKind::Agent && t.status == BgTaskStatus::Running)
            .collect()
    }

    /// Tool calls made by every background task so far — the measure of
    /// progress between two auto-resumes.
    pub fn bg_tool_uses(&self) -> u64 {
        self.bg_tasks.iter().map(|t| t.tool_uses).sum()
    }

    /// Whether resuming `lost` again would just repeat the last auto-resume:
    /// no fewer agents cut off, and the relaunches did no more than the one
    /// call each that backgrounds their work again. Progress is judged by
    /// counts, not descriptions, since a relaunch is often reworded. Any real
    /// progress keeps the session resuming, however many rounds it takes.
    pub fn cut_off_stalled(&self, lost: &[&BgTask]) -> bool {
        let Some(last) = &self.last_cutoff else {
            return false;
        };
        lost.len() >= last.descriptions.len()
            && self.bg_tool_uses() <= last.tool_uses + lost.len() as u64
    }

    /// An auto-resumed turn ended: the parent was told about the agents cut
    /// off before and dealt with them, whether it relaunched them under
    /// another description or judged them done. Those cut off by this very
    /// turn's end (`keep`) stay cut off.
    pub fn settle_cut_offs(&mut self, keep: &[&str]) {
        if self.last_cutoff.is_none() {
            return;
        }
        let now = now_millis();
        for t in &mut self.bg_tasks {
            if t.kind == BgTaskKind::Agent
                && t.status == BgTaskStatus::CutOff
                && !keep.contains(&t.id.as_str())
            {
                t.status = BgTaskStatus::Relaunched;
                t.updated_at = now;
            }
        }
    }

    /// Remember an auto-resume of `lost` for [`Session::cut_off_stalled`].
    pub fn record_cutoff(&mut self, lost: &[BgTask]) {
        let refs: Vec<&BgTask> = lost.iter().collect();
        self.last_cutoff = Some(CutOffRound {
            descriptions: sorted_descriptions(&refs),
            tool_uses: self.bg_tool_uses(),
        });
    }

    /// The user starts a turn: what is still cut off is handed back (for the
    /// prompt to mention), settled tasks are dropped, and the auto-resume
    /// guard starts over.
    pub fn begin_user_turn(&mut self) -> Vec<BgTask> {
        let cut: Vec<BgTask> = self.cut_off_agents().into_iter().cloned().collect();
        self.bg_tasks.retain(|t| !t.status.is_terminal());
        self.last_cutoff = None;
        cut
    }

    /// The user stopped the session: whatever ran was stopped with it.
    pub fn stop_live_tasks(&mut self) {
        let now = now_millis();
        for t in &mut self.bg_tasks {
            if !t.status.is_terminal() {
                t.status = BgTaskStatus::Stopped;
                t.updated_at = now;
            }
        }
    }

    /// The run ended without the CLI accounting for its tasks (a restart, a
    /// crash, a question tearing the run down): every live one was cut off.
    pub fn cut_off_live_tasks(&mut self) {
        let ids: Vec<String> = self
            .bg_tasks
            .iter()
            .filter(|t| !t.status.is_terminal())
            .map(|t| t.id.clone())
            .collect();
        if !ids.is_empty() {
            self.apply_task(&TaskEvent::CutOff { ids });
        }
    }
}

fn sorted_descriptions(tasks: &[&BgTask]) -> Vec<String> {
    let mut out: Vec<String> = tasks.iter().map(|t| t.description.clone()).collect();
    out.sort();
    out
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
        s.push_turn(SessionTurn::new(SessionTurnKind::Interrupted, "Interrupted by user"));
        assert_eq!(s.last_resendable_turn().unwrap().text, "Speed");
    }

    fn started(id: &str, parent: Option<&str>, kind: BgTaskKind, desc: &str) -> TaskEvent {
        TaskEvent::Started {
            id: id.into(),
            parent: parent.map(str::to_string),
            kind,
            description: desc.into(),
            subagent_type: Some("general-purpose".into()),
            backgrounded: true,
        }
    }

    fn status(id: &str, status: BgTaskStatus) -> TaskEvent {
        TaskEvent::Status {
            id: id.into(),
            status,
            reason: None,
            summary: Some(format!("{status:?}")),
            tool_uses: Some(4),
        }
    }

    #[test]
    fn a_settled_task_is_not_downgraded_but_can_still_be_cut_off() {
        let mut s = session();
        s.apply_task(&started("a", None, BgTaskKind::Agent, "Build"));
        assert!(s.apply_task(&status("a", BgTaskStatus::Completed)));
        // A late progress line or another end leaves it be.
        s.apply_task(&TaskEvent::Progress {
            id: "a".into(),
            last_tool: Some("Bash".into()),
            summary: None,
            tool_uses: Some(9),
        });
        assert_eq!(s.bg_tasks[0].status, BgTaskStatus::Completed);
        assert!(!s.apply_task(&status("a", BgTaskStatus::Stopped)));
        assert_eq!(s.bg_tasks[0].status, BgTaskStatus::Completed);
        assert_eq!(s.bg_tool_uses(), 9, "tool counts only grow");

        // Its own command cut off at the run's end cuts it off too.
        s.apply_task(&started("c", Some("a"), BgTaskKind::Command, "cargo test"));
        s.apply_task(&TaskEvent::CutOff {
            ids: vec!["c".into()],
        });
        assert_eq!(s.bg_tasks[0].status, BgTaskStatus::CutOff);
        let ids = vec!["c".to_string()];
        assert_eq!(s.lost_agents(&ids).len(), 1);
        assert_eq!(s.lost_agents(&ids)[0].id, "a");
    }

    #[test]
    fn foreground_commands_of_the_parent_are_not_tracked() {
        let mut s = session();
        assert!(!s.apply_task(&TaskEvent::Started {
            id: "c".into(),
            parent: None,
            kind: BgTaskKind::Command,
            description: "ls".into(),
            subagent_type: None,
            backgrounded: false,
        }));
        assert!(s.bg_tasks.is_empty());
        // The parent's own background command alone is no lost agent.
        s.apply_task(&started("srv", None, BgTaskKind::Command, "dev server"));
        s.apply_task(&TaskEvent::CutOff {
            ids: vec!["srv".into()],
        });
        assert!(s.lost_agents(&["srv".to_string()]).is_empty());
    }

    #[test]
    fn a_relaunch_supersedes_the_cut_off_entry() {
        let mut s = session();
        s.apply_task(&started("a", None, BgTaskKind::Agent, "Build"));
        s.apply_task(&TaskEvent::CutOff {
            ids: vec!["a".into()],
        });
        assert_eq!(s.cut_off_agents().len(), 1);
        s.apply_task(&started("b", None, BgTaskKind::Agent, "Build"));
        assert_eq!(s.bg_tasks[0].status, BgTaskStatus::Relaunched);
        assert!(s.cut_off_agents().is_empty());
        assert_eq!(s.running_agents().len(), 1);
    }

    #[test]
    fn the_guard_pauses_only_a_repeat_without_progress() {
        let mut s = session();
        s.apply_task(&started("a", None, BgTaskKind::Agent, "Build"));
        s.apply_task(&TaskEvent::CutOff {
            ids: vec!["a".into()],
        });
        let lost: Vec<BgTask> = s.cut_off_agents().into_iter().cloned().collect();
        let refs: Vec<&BgTask> = lost.iter().collect();
        assert!(!s.cut_off_stalled(&refs), "the first round always resumes");
        s.record_cutoff(&lost);
        assert!(s.cut_off_stalled(&refs));

        // The relaunch re-backgrounds its work in one call: still stuck.
        s.apply_task(&started("b", None, BgTaskKind::Agent, "Build"));
        s.apply_task(&TaskEvent::Progress {
            id: "b".into(),
            last_tool: None,
            summary: None,
            tool_uses: Some(1),
        });
        s.apply_task(&TaskEvent::CutOff {
            ids: vec!["b".into()],
        });
        let again: Vec<&BgTask> = s.lost_agents(&["b".to_string()]);
        assert!(s.cut_off_stalled(&again));

        // Real work is progress.
        s.apply_task(&TaskEvent::Progress {
            id: "b".into(),
            last_tool: None,
            summary: None,
            tool_uses: Some(6),
        });
        let again: Vec<&BgTask> = s.lost_agents(&["b".to_string()]);
        assert!(!s.cut_off_stalled(&again));

        // A clean end of the auto-resumed turn settles what it was told about.
        let mut settled = s.clone();
        settled.settle_cut_offs(&[]);
        assert!(settled.cut_off_agents().is_empty());

        // A user's turn hands back what is still cut off and starts over.
        let cut = s.begin_user_turn();
        assert_eq!(cut.len(), 1);
        assert!(s.bg_tasks.is_empty());
        assert!(s.last_cutoff.is_none());
    }

    #[test]
    fn the_guard_sees_through_a_reworded_relaunch() {
        let mut s = session();
        s.apply_task(&started("a", None, BgTaskKind::Agent, "Build the walls"));
        s.apply_task(&TaskEvent::CutOff {
            ids: vec!["a".into()],
        });
        let lost: Vec<BgTask> = s.cut_off_agents().into_iter().cloned().collect();
        s.record_cutoff(&lost);

        // Relaunched under another name, cut off again after one call.
        s.apply_task(&started("b", None, BgTaskKind::Agent, "Finish the walls"));
        assert_eq!(s.cut_off_agents().len(), 1, "no exact match yet");
        s.apply_task(&TaskEvent::Progress {
            id: "b".into(),
            last_tool: None,
            summary: None,
            tool_uses: Some(1),
        });
        s.apply_task(&TaskEvent::CutOff {
            ids: vec!["b".into()],
        });
        let again: Vec<&BgTask> = s.lost_agents(&["b".to_string()]);
        assert_eq!(again.len(), 1);
        assert!(s.cut_off_stalled(&again), "reworded, still no progress");

        // The round's end settles the old entry, keeping only the new loss.
        s.settle_cut_offs(&["b"]);
        let cut: Vec<&str> = s.cut_off_agents().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(cut, vec!["b"]);
    }

    #[test]
    fn stop_and_restart_settle_live_tasks() {
        let mut s = session();
        s.apply_task(&started("a", None, BgTaskKind::Agent, "Build"));
        s.apply_task(&started("b", None, BgTaskKind::Agent, "Test"));
        s.apply_task(&status("b", BgTaskStatus::Completed));
        let mut stopped = s.clone();
        stopped.stop_live_tasks();
        assert_eq!(stopped.bg_tasks[0].status, BgTaskStatus::Stopped);
        s.cut_off_live_tasks();
        assert_eq!(s.bg_tasks[0].status, BgTaskStatus::CutOff);
        assert_eq!(s.bg_tasks[1].status, BgTaskStatus::Completed);
    }

    #[test]
    fn sessions_saved_before_background_tasks_still_load() {
        let mut v = serde_json::to_value(session()).unwrap();
        let obj = v.as_object_mut().unwrap();
        obj.remove("bg_tasks");
        obj.remove("last_cutoff");
        let s: Session = serde_json::from_value(v).unwrap();
        assert!(s.bg_tasks.is_empty());
    }
}
