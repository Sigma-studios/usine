//! Claude provider: drives the `claude` binary in headless stream-json mode.
//!
//! Each phase is a one-shot run: we pass the prompt as a positional arg, stream
//! the NDJSON stdout through [`super::stream::parse_claude_line`], and exit when
//! the process does. A run ends when the process exits, not at its first
//! `result` line: that line only closes a *turn*, and while background work
//! (`Monitor`, `Bash run_in_background`) is pending the CLI keeps the process
//! alive and wakes the agent for further turns. The last `result` is the run's
//! answer (see [`TurnGate`]). The plan phase runs read-only (`--permission-mode plan`);
//! implement/fix phases run autonomously (`--dangerously-skip-permissions`) in
//! the card's worktree. Relies on the user's existing `claude` auth.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::{sleep_until, timeout, Instant};

use crate::agent::events::{AgentEvent, RunControl};
use crate::domain::model::{supported_efforts, Provider, Usage};
use crate::error::{CoreError, Result};

use super::stream::{
    errored_tool_result_ids, exit_plan_mode_tool_id, parse_claude_line, replayed_user_text,
};
use super::{AgentProvider, RunConfig, RunHandle, RunMode, SESSION_STATUS_PROMPT};

/// Build the argv for a `claude` headless run, **excluding** the binary name and
/// the prompt (the prompt is appended as the final positional arg by `start`).
///
/// - Plan → `--permission-mode plan` (read-only) + a caller-supplied
///   `--session-id`.
/// - Implement / ApplyFixes → `--dangerously-skip-permissions` so the agent can
///   edit files and run commands autonomously inside the isolated worktree. The
///   approved plan / fix list rides along in the prompt, so no resume is needed.
///
/// - SessionFull / SessionReadOnly → the same two permission levels, for a
///   one-off session in the user's folder.
///
/// Every card mode also denies the background-orchestration tools (`Agent`, `Task`,
/// `ScheduleWakeup`, `Workflow`): a one-shot `claude -p` run is single-turn, but
/// those tools let the agent launch async sub-agents and end its turn early, so
/// the run reports that interim turn as its result instead of the real
/// plan/edits. Denying them forces the agent to finish in one turn. (Interim
/// turns from the tools we *do* allow — `Monitor`, background `Bash` — are
/// handled in [`pump`] by [`TurnGate`], which waits for the process to exit.)
/// Sessions allow `Agent`/`Task` and rely on that same gate.
///
/// Sessions also take their input as stream-json on stdin (`start` writes the
/// prompt there instead of appending it), echoed back with
/// `--replay-user-messages`, so the user can write to the agent mid-turn (see
/// [`LiveInput`]), and get [`SESSION_STATUS_PROMPT`] appended to the system
/// prompt on every turn.
pub fn build_args(cfg: &RunConfig) -> Vec<String> {
    // Claude Code clamps an unsupported `--effort` itself, but we clamp here too so
    // the flag we pass reflects the model's real capability (e.g. Haiku, which has
    // no effort tiers).
    let effort = cfg
        .spec
        .effort
        .clamp_to(supported_efforts(Provider::Claude, &cfg.spec.model));
    let mut args = vec![
        "-p".to_string(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--model".into(),
        cfg.spec.model.clone(),
        "--effort".into(),
        effort.claude_flag().into(),
    ];

    // Resume continues an existing conversation (full memory). We never pass a
    // fixed --session-id (which *creates* a session and would collide on re-run).
    if let Some(session) = &cfg.resume_session {
        args.push("--resume".into());
        args.push(session.clone());
    }

    // Session-only options, each taking exactly one value, kept ahead of the
    // variadic `--disallowedTools` below.
    if cfg.mode.is_session() {
        args.extend([
            "--input-format".into(),
            "stream-json".into(),
            "--replay-user-messages".into(),
            "--append-system-prompt".into(),
            SESSION_STATUS_PROMPT.into(),
        ]);
    }

    // Single-turn guard: deny the tools that let the agent spawn background work
    // or schedule a later turn. Pushed *before* the mode flag below so this
    // variadic option is bounded by the next flag and can't swallow the trailing
    // prompt positional that `start` appends.
    // Sessions keep `Agent`/`Task`: a terminal-style session is expected to
    // use subagents, and [`TurnGate`] already holds the result until the
    // process exits, so the subagents' work is covered.
    args.push("--disallowedTools".into());
    args.push(if cfg.mode.is_session() {
        "ScheduleWakeup,Workflow".into()
    } else {
        "Agent,Task,ScheduleWakeup,Workflow".into()
    });

    match cfg.mode {
        // Read-only phases (planning, self-review, comment triage, Q&A,
        // investigation): restrict to read-only tools so the agent can inspect
        // but never edit.
        RunMode::Plan
        | RunMode::Review
        | RunMode::Triage
        | RunMode::Question
        | RunMode::Investigate
        | RunMode::SessionReadOnly => {
            args.push("--permission-mode".into());
            args.push("plan".into());
        }
        RunMode::Implement | RunMode::ApplyFixes | RunMode::SessionFull => {
            args.push("--dangerously-skip-permissions".into());
        }
    }

    // Terminate option parsing so the prompt `start` appends is always read as a
    // positional. Without it a prompt starting with `-` — a description opening
    // on a markdown bullet — is taken for a flag ("error: unknown option '- fix
    // the thing'") and the run dies before it begins. Also belt-and-braces for
    // the variadic-option hazard the ordering above already guards against.
    args.push("--".into());
    args
}

pub struct ClaudeProvider;

#[async_trait]
impl AgentProvider for ClaudeProvider {
    fn provider(&self) -> Provider {
        Provider::Claude
    }

    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        let session = cfg.mode.is_session();
        let mut args = build_args(&cfg);
        if !session {
            args.push(cfg.full_prompt());
        }

        let mut child = Command::new("claude")
            .args(&args)
            .current_dir(&cfg.project_dir)
            .stdin(if session {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                CoreError::provider(format!("failed to spawn `claude` (is it installed?): {e}"))
            })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::provider("claude stdout unavailable"))?;
        let stderr = child.stderr.take();
        let live = match child.stdin.take() {
            Some(stdin) => Some(
                LiveInput::open(stdin, &cfg.full_prompt())
                    .await
                    .map_err(|e| CoreError::provider(format!("could not write to claude: {e}")))?,
            ),
            None => None,
        };

        let (evt_tx, evt_rx) = mpsc::unbounded::<AgentEvent>();
        let (ctl_tx, ctl_rx) = mpsc::unbounded::<RunControl>();

        let grace = if session {
            SESSION_HELD_RESULT_GRACE
        } else {
            HELD_RESULT_GRACE
        };
        tokio::spawn(pump(child, stdout, stderr, evt_tx, ctl_rx, grace, live));

        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

/// A session run's stdin, kept open so the user can write to the agent while
/// it works (`--input-format stream-json`).
///
/// Verified live on CLI 2.1.303: a message written mid-turn is picked up at the
/// next tool boundary (its `--replay-user-messages` echo arrives right after
/// the running tool's result) and handled before that turn's `result`; once
/// stdin closes the process still finishes pending background subagents and
/// exits. So stdin is closed at a `result` with no written message still
/// unechoed — that is what lets the process end, and [`TurnGate`] keeps ending
/// the run at process exit as before.
struct LiveInput {
    /// `None` once closed: later messages are refused (and get queued).
    stdin: Option<ChildStdin>,
    /// Messages written whose echo hasn't come back yet.
    pending: usize,
    /// Whether the turn's own prompt has been echoed: later echoes are the
    /// user's live messages.
    seen_prompt: bool,
}

/// After a `result` with a written message still unechoed, how long stdout
/// may stay quiet before stdin is closed anyway, so a lost echo can't keep
/// the process (and the session) alive.
const UNECHOED_CLOSE: Duration = Duration::from_secs(60);

impl LiveInput {
    /// Write the turn's prompt as the first message.
    async fn open(stdin: ChildStdin, prompt: &str) -> std::io::Result<Self> {
        let mut live = LiveInput {
            stdin: Some(stdin),
            pending: 0,
            seen_prompt: false,
        };
        live.write(prompt).await?;
        Ok(live)
    }

    async fn write(&mut self, text: &str) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        };
        let mut line = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]},
        })
        .to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        self.pending += 1;
        Ok(())
    }

    /// Write a live message; whether the agent will get it this run.
    async fn send(&mut self, text: &str) -> bool {
        if self.stdin.is_none() {
            return false;
        }
        match self.write(text).await {
            Ok(()) => true,
            Err(_) => {
                self.stdin = None;
                false
            }
        }
    }

    /// Track one stdout line: echoes settle written messages, and a turn's
    /// `result` closes stdin once nothing written is still in flight. Returns
    /// the activity note for a picked-up live message.
    fn observe(&mut self, line: &str, events: &[AgentEvent]) -> Option<AgentEvent> {
        if let Some(text) = replayed_user_text(line) {
            self.pending = self.pending.saturating_sub(1);
            if !self.seen_prompt {
                self.seen_prompt = true;
                return None;
            }
            let first = text.lines().map(str::trim).find(|l| !l.is_empty())?;
            return Some(AgentEvent::Progress {
                text: format!(
                    "↪ picked up: {}",
                    crate::domain::session::truncate_chars(first, 60)
                ),
            });
        }
        let result = events
            .iter()
            .any(|e| matches!(e, AgentEvent::Done { .. } | AgentEvent::Error { .. }));
        if result && self.pending == 0 {
            self.close();
        }
        None
    }

    fn is_open(&self) -> bool {
        self.stdin.is_some()
    }

    fn close(&mut self) {
        self.stdin = None;
    }
}

/// Buffers an `ExitPlanMode` plan until its approve/reject verdict is known, so
/// a headless-rejected plan can't pre-empt the agent's real final answer.
///
/// A headless plan run (`--permission-mode plan`, no interactive approver) can't
/// approve an `ExitPlanMode` call, so the CLI rejects it and the agent
/// re-presents its real plan — including any `usine-questions` block — as the
/// run's final `result`. Forwarding the `ExitPlanMode` draft the instant it
/// arrives fires a premature `PlanReady`, moving the card to "awaiting approval"
/// so the executor then discards that later, authoritative result (the one
/// carrying the questions) as a no-op `Done`. So we hold the draft until the
/// terminal `Done`: if the call was rejected we surface the final result
/// instead; only an un-rejected call is trusted verbatim.
#[derive(Default)]
struct PlanGate {
    /// `(tool_use id, draft plan)` from an `ExitPlanMode` call not yet resolved.
    pending: Option<(String, String)>,
    /// Whether the buffered call's `tool_result` came back errored (rejected).
    rejected: bool,
}

impl PlanGate {
    /// Note a rejection verdict for the buffered plan if `line` is the errored
    /// `tool_result` for it. Call once per line, before [`PlanGate::gate`].
    fn observe(&mut self, line: &str) {
        if let Some((id, _)) = &self.pending {
            if errored_tool_result_ids(line).iter().any(|e| e == id) {
                self.rejected = true;
            }
        }
    }

    /// Transform one line's parsed events, holding an `ExitPlanMode` plan back
    /// and re-emitting it (resolved against its verdict) at the terminal `Done`.
    fn gate(&mut self, line: &str, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::with_capacity(events.len() + 1);
        for evt in events {
            match evt {
                AgentEvent::PlanReady { plan } => {
                    // Hold, don't forward: the verdict isn't known yet.
                    self.pending = Some((exit_plan_mode_tool_id(line).unwrap_or_default(), plan));
                    self.rejected = false;
                }
                AgentEvent::Done { .. } => {
                    if let Some(plan) = self.resolve(&evt) {
                        out.push(AgentEvent::PlanReady { plan });
                    }
                    out.push(evt);
                }
                AgentEvent::Error { .. } => {
                    // The run failed outright; the buffered plan is moot.
                    self.pending = None;
                    out.push(evt);
                }
                other => out.push(other),
            }
        }
        out
    }

    /// Resolve the buffered plan at `Done`: a rejected call yields the run's
    /// final result (the agent's authoritative re-presentation), falling back to
    /// the tool draft only if that result is empty. An un-rejected call is
    /// trusted as-is. Returns `None` when no plan was buffered.
    fn resolve(&mut self, done: &AgentEvent) -> Option<String> {
        let (_, draft) = self.pending.take()?;
        let result = match done {
            AgentEvent::Done { result, .. } => result.as_str(),
            _ => "",
        };
        Some(if self.rejected && !result.trim().is_empty() {
            result.to_string()
        } else {
            draft
        })
    }

    /// Flush any plan still buffered at end-of-stream — e.g. an un-rejected plan
    /// whose run produced no trailing `result` line.
    fn finish(&mut self) -> Option<AgentEvent> {
        self.pending
            .take()
            .map(|(_, draft)| AgentEvent::PlanReady { plan: draft })
    }
}

/// Holds the run's terminal event until the `claude` process exits.
///
/// A `result` line ends a *turn*, not the run: when the agent ended its turn
/// waiting on background work (`Monitor`, `Bash run_in_background`), the CLI
/// keeps the process alive, wakes the agent when that work reports, and emits a
/// fresh `result` per turn. Forwarding the first one as `Done` finalized the
/// card on an interim "waiting for…" message and dropped the real answer (with
/// its `usine-commit`/`usine-handoff` blocks). So every `Done`/`Error` is held,
/// each later one replaces it, and [`TurnGate::finish`] releases the last at EOF.
/// The exception is a result kept by [`TurnGate::keep_held`] (it answered the
/// turn before a live message): it is joined ahead of the final one.
#[derive(Default)]
struct TurnGate {
    /// The latest terminal event (`Done`/`Error`) seen, not yet forwarded.
    held: Option<AgentEvent>,
    /// Results of earlier turns that must reach the final answer.
    kept: Vec<String>,
    /// Whether `held` is the last of `kept`, so the next event is the agent
    /// picking up the live message rather than resuming after background work.
    held_kept: bool,
    /// Token usage summed over every turn's `result` (each reports its own turn).
    usage: Usage,
    /// Whether the current resumption has been announced in the activity log.
    resumed_announced: bool,
}

impl TurnGate {
    /// Hold back terminal events, passing everything else through. The first
    /// event after a held result means the agent resumed: say so once.
    fn hold(&mut self, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::with_capacity(events.len());
        for evt in events {
            match evt {
                AgentEvent::Done {
                    result,
                    cost_usd,
                    usage,
                } => {
                    self.usage += usage;
                    // `total_cost_usd` is cumulative for the process: the last wins.
                    self.held = Some(AgentEvent::Done {
                        result,
                        cost_usd,
                        usage: self.usage,
                    });
                    self.resumed_announced = false;
                    self.held_kept = false;
                }
                AgentEvent::Error { .. } => {
                    self.held = Some(evt);
                    self.resumed_announced = false;
                    self.held_kept = false;
                }
                other => {
                    if self.held.is_some() && !self.resumed_announced && !self.held_kept {
                        self.resumed_announced = true;
                        out.push(AgentEvent::Progress {
                            text: "Previous turn ended; agent resumed after background work".into(),
                        });
                    }
                    out.push(other);
                }
            }
        }
        out
    }

    /// Whether a turn's result is waiting for the run to end.
    fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    /// Keep the held result: a live message written before it was read
    /// makes the CLI run another turn, whose result must not replace it.
    fn keep_held(&mut self) {
        if let Some(AgentEvent::Done { result, .. }) = &self.held {
            if !result.trim().is_empty() {
                self.kept.push(result.clone());
            }
            self.held_kept = true;
        }
    }

    /// The run's final terminal event, released once the run has ended, with
    /// any kept results joined ahead of a final `Done`.
    fn finish(&mut self) -> Option<AgentEvent> {
        let kept = std::mem::take(&mut self.kept);
        match self.held.take()? {
            AgentEvent::Done {
                result,
                cost_usd,
                usage,
            } if !kept.is_empty() => {
                // A still-kept held result is already the last of `kept`.
                let mut parts = kept;
                if !self.held_kept && !result.trim().is_empty() {
                    parts.push(result);
                }
                Some(AgentEvent::Done {
                    result: parts.join("\n\n"),
                    cost_usd,
                    usage,
                })
            }
            evt => Some(evt),
        }
    }
}

/// How long a run may stay silent with a turn's `result` held before that
/// result is taken as final and the process is killed.
///
/// The end of a run is normally stdout EOF, but EOF can be held off forever: a
/// long, quiet background task, or a grandchild still holding the CLI's stdout
/// after `claude` has exited. Without this, the executor's idle watchdog would
/// cancel the run and fail the card, throwing away a complete answer. It must be
/// longer than the CLI's 10-minute cap on `Monitor`/background `Bash` (those
/// wake the agent when they expire) and shorter than the executor's
/// `RUN_IDLE_TIMEOUT`, which is asserted there. Both measure time since the last
/// event forwarded to the executor.
pub(crate) const HELD_RESULT_GRACE: Duration = Duration::from_secs(12 * 60);

/// [`HELD_RESULT_GRACE`] for one-off sessions. Sessions allow `Agent`/`Task`,
/// and a `run_in_background` subagent has no 10-minute cap: the parent's
/// interim result ("waiting on the subagent") is followed by silence until the
/// subagent reports back. Releasing it early would mark the session Finished
/// and kill the subagent mid-work — the exact failure sessions exist to avoid.
/// The user can always Stop a session, so a long wait costs little. Must stay
/// below the session actor's `SESSION_IDLE_TIMEOUT`, asserted there.
pub(crate) const SESSION_HELD_RESULT_GRACE: Duration = Duration::from_secs(2 * 60 * 60);

/// How long to keep reading stdout once `claude` itself has exited, for lines
/// still in the pipe. A grandchild holding the pipe open won't delay us longer.
const EXIT_DRAIN: Duration = Duration::from_secs(2);

/// Read the NDJSON stream to completion, forwarding normalized events. The run's
/// `Done`/`Error` is sent only at the end — earlier `result` lines are interim
/// turns (see [`TurnGate`]). The end is stdout EOF, `claude` exiting, or
/// `grace` of silence after a turn's result ([`HELD_RESULT_GRACE`], or
/// [`SESSION_HELD_RESULT_GRACE`] for sessions). Reacts to Cancel by killing the
/// child. A session run (`live`) writes `Message` controls to the agent's
/// stdin while it is open; other runs refuse them. (Questions can't be
/// answered mid-flight, so `Answer` controls are ignored.)
async fn pump(
    mut child: Child,
    stdout: tokio::process::ChildStdout,
    stderr: Option<tokio::process::ChildStderr>,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    mut ctl_rx: mpsc::UnboundedReceiver<RunControl>,
    grace: Duration,
    mut live: Option<LiveInput>,
) {
    // Drain stderr concurrently so a useful message survives a non-zero exit.
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_string(&mut buf).await;
        }
        buf
    });

    let mut lines = BufReader::new(stdout).lines();
    let mut saw_terminal = false;
    let mut cancelled = false;
    // Disable the control branch once its channel closes, otherwise `next()`
    // returns `Ready(None)` forever and the select spins at 100% CPU.
    let mut ctl_open = true;
    // Holds an ExitPlanMode plan until its approve/reject verdict is known.
    let mut gate = PlanGate::default();
    // Holds each turn's result until the run ends; the last one wins.
    let mut turns = TurnGate::default();
    // When the executor last received an event: its idle watchdog counts from
    // here, so the held-result grace does too.
    let mut last_sent = Instant::now();
    let mut exit_status = None;
    let mut timed_out = false;

    // When stdout last produced a line (for closing a stuck live stdin).
    let mut last_line = Instant::now();

    let forward = |line: &str,
                   gate: &mut PlanGate,
                   turns: &mut TurnGate,
                   live: &mut Option<LiveInput>,
                   last_sent: &mut Instant| {
        gate.observe(line);
        let events = parse_claude_line(line).unwrap_or_default();
        if let Some(note) = live.as_mut().and_then(|l| l.observe(line, &events)) {
            *last_sent = Instant::now();
            let _ = evt_tx.unbounded_send(note);
        }
        // A result read while a live message is still unechoed answers the
        // turn before it: the CLI takes the message as a next turn in this
        // process, so that answer must not be replaced by the next result.
        let late_message = live.as_ref().is_some_and(|l| l.pending > 0)
            && events.iter().any(|e| matches!(e, AgentEvent::Done { .. }));
        // Terminal events (Done/Error, and the PlanReady resolved against
        // them) are held until the end, so none pass here.
        let passed = turns.hold(events);
        if late_message {
            turns.keep_held();
        }
        for evt in gate.gate(line, passed) {
            *last_sent = Instant::now();
            let _ = evt_tx.unbounded_send(evt);
        }
    };

    loop {
        let stdin_open = live.as_ref().is_some_and(LiveInput::is_open);
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    last_line = Instant::now();
                    forward(&line, &mut gate, &mut turns, &mut live, &mut last_sent)
                }
                _ => break, // EOF or read error
            },
            status = child.wait(), if exit_status.is_none() => {
                exit_status = Some(status);
                // `claude` is gone; read what's left in the pipe, but don't wait
                // on a grandchild (a background task) that still holds it open.
                while let Ok(Ok(Some(line))) = timeout(EXIT_DRAIN, lines.next_line()).await {
                    forward(&line, &mut gate, &mut turns, &mut live, &mut last_sent);
                }
                break;
            },
            _ = sleep_until(last_line + UNECHOED_CLOSE), if stdin_open && turns.is_holding() => {
                // A written message's echo never came: stop waiting on it so
                // the process can end.
                if let Some(l) = live.as_mut() {
                    l.close();
                }
            },
            _ = sleep_until(last_sent + grace), if turns.is_holding() => {
                timed_out = true;
                break;
            },
            ctl = ctl_rx.next(), if ctl_open => match ctl {
                Some(RunControl::Cancel) | Some(RunControl::Interrupt) => {
                    cancelled = true;
                    break;
                }
                Some(RunControl::Message { text, accepted }) => {
                    let ok = match live.as_mut() {
                        Some(l) => l.send(&text).await,
                        None => false,
                    };
                    let _ = accepted.send(ok);
                }
                Some(RunControl::Answer { .. }) => { /* no question to answer mid-flight */ }
                None => ctl_open = false,
            },
        }
    }

    if exit_status.is_none() && (cancelled || timed_out) {
        let _ = child.start_kill();
    }
    if timed_out {
        let _ = evt_tx.unbounded_send(AgentEvent::Progress {
            text: format!(
                "No output for {} min since the agent's last turn; using that turn's result",
                grace.as_secs() / 60
            ),
        });
    }

    // End of run: release the final turn's result, resolving any held plan
    // against it; failing that, surface a plan still held by the gate (an
    // un-rejected plan whose run produced no trailing `result` line). A
    // cancelled run forwards neither.
    if !cancelled {
        let tail = match turns.finish() {
            // `line` is only read for PlanReady tool ids; none arrive here.
            Some(evt) => gate.gate("", vec![evt]),
            None => gate.finish().into_iter().collect(),
        };
        for evt in tail {
            saw_terminal = true;
            let _ = evt_tx.unbounded_send(evt);
        }
    }

    let status = match exit_status {
        Some(status) => status.ok(),
        None => child.wait().await.ok(),
    };
    // A grandchild may still hold stderr open too; don't wait on it for a run
    // that already has its answer.
    let stderr_text = if saw_terminal {
        stderr_task.abort();
        String::new()
    } else {
        stderr_task.await.unwrap_or_default()
    };

    if !saw_terminal && !cancelled {
        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        let detail = stderr_text.trim();
        let message = if detail.is_empty() {
            format!("claude exited with code {code} before producing a result")
        } else {
            format!("claude failed (code {code}): {detail}")
        };
        let _ = evt_tx.unbounded_send(AgentEvent::Error { message });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Effort, ModelSpec, Provider};
    use std::path::PathBuf;
    use uuid::Uuid;

    fn cfg(mode: RunMode) -> RunConfig {
        RunConfig {
            provider: Provider::Claude,
            project_dir: PathBuf::from("/repo"),
            spec: ModelSpec::new("opus", Effort::High),
            mode,
            session_id: Uuid::nil(),
            prompt: "do it".into(),
            extra_prompt: None,
            resume_session: None,
            attachments: Vec::new(),
        }
    }

    fn pair(args: &[String], flag: &str) -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .map(|i| args[i + 1].clone())
    }

    #[test]
    fn plan_args_are_read_only_without_fixed_session() {
        let args = build_args(&cfg(RunMode::Plan));
        assert_eq!(pair(&args, "--permission-mode").as_deref(), Some("plan"));
        assert_eq!(pair(&args, "--model").as_deref(), Some("opus"));
        assert_eq!(pair(&args, "--effort").as_deref(), Some("high"));
        // No fixed --session-id, so re-runs (Resume/Reject) don't collide.
        assert!(!args.contains(&"--session-id".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(
            pair(&args, "--output-format").as_deref(),
            Some("stream-json")
        );
    }

    #[test]
    fn a_pinned_fable_5_1_id_is_forwarded_verbatim_with_its_effort_clamped() {
        // The CLI only resolves the bare aliases, so a pinned id must reach it
        // unrewritten.
        let mut c = cfg(RunMode::Implement);
        c.spec = ModelSpec::new("claude-fable-5-1", Effort::Max);
        let args = build_args(&c);
        assert_eq!(pair(&args, "--model").as_deref(), Some("claude-fable-5-1"));
        assert_eq!(pair(&args, "--effort").as_deref(), Some("max"));

        // Fable 5.1 has no `ultra` tier — clamp down rather than let the CLI
        // reject the flag.
        c.spec = ModelSpec::new("claude-fable-5-1", Effort::Ultra);
        assert_eq!(pair(&build_args(&c), "--effort").as_deref(), Some("max"));
    }

    #[test]
    fn question_args_are_read_only() {
        let args = build_args(&cfg(RunMode::Question));
        assert_eq!(pair(&args, "--permission-mode").as_deref(), Some("plan"));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn investigate_args_are_read_only() {
        // Investigations must never be able to edit: same read-only plan
        // permission mode as the other inspect-only phases.
        let args = build_args(&cfg(RunMode::Investigate));
        assert_eq!(pair(&args, "--permission-mode").as_deref(), Some("plan"));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn implement_args_skip_permissions() {
        let args = build_args(&cfg(RunMode::Implement));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn resume_adds_resume_flag() {
        let mut c = cfg(RunMode::Implement);
        c.resume_session = Some("sess-123".into());
        let args = build_args(&c);
        assert!(args.windows(2).any(|w| w == ["--resume", "sess-123"]));
    }

    #[test]
    fn denies_background_orchestration_before_the_mode_flag() {
        for (mode, mode_flag) in [
            (RunMode::Plan, "--permission-mode"),
            (RunMode::Implement, "--dangerously-skip-permissions"),
        ] {
            let args = build_args(&cfg(mode));
            let di = args
                .iter()
                .position(|a| a == "--disallowedTools")
                .expect("--disallowedTools present");
            // The denied set rides as one comma-separated value (not variadic args).
            assert!(args[di + 1].contains("Agent"));
            assert!(args[di + 1].contains("ScheduleWakeup"));
            // Must precede the mode flag so the variadic option can't eat the prompt.
            let mf = args.iter().position(|a| a == mode_flag).unwrap();
            assert!(di < mf, "disallowedTools must come before {mode_flag}");
        }
    }

    #[test]
    fn sessions_allow_subagents_with_their_access_level() {
        for (mode, flag) in [
            (RunMode::SessionFull, "--dangerously-skip-permissions"),
            (RunMode::SessionReadOnly, "--permission-mode"),
        ] {
            let mut c = cfg(mode);
            c.resume_session = Some("sess-1".into());
            let args = build_args(&c);
            let denied = pair(&args, "--disallowedTools").unwrap();
            assert_eq!(denied, "ScheduleWakeup,Workflow");
            assert!(!denied.contains("Agent"));
            let di = args.iter().position(|a| a == "--disallowedTools").unwrap();
            let mf = args.iter().position(|a| a == flag).unwrap();
            assert!(di < mf);
            assert!(args.windows(2).any(|w| w == ["--resume", "sess-1"]));
            assert_eq!(args.last().map(String::as_str), Some("--"));
        }
        assert_eq!(
            pair(
                &build_args(&cfg(RunMode::SessionReadOnly)),
                "--permission-mode"
            )
            .as_deref(),
            Some("plan")
        );
    }

    #[test]
    fn sessions_take_stream_input_and_the_status_prompt_before_the_variadic_option() {
        for mode in [RunMode::SessionFull, RunMode::SessionReadOnly] {
            let args = build_args(&cfg(mode));
            assert_eq!(
                pair(&args, "--input-format").as_deref(),
                Some("stream-json")
            );
            assert!(args.contains(&"--replay-user-messages".to_string()));
            assert_eq!(
                pair(&args, "--append-system-prompt").as_deref(),
                Some(SESSION_STATUS_PROMPT)
            );
            let at = |flag: &str| args.iter().position(|a| a == flag).unwrap();
            assert!(at("--append-system-prompt") < at("--disallowedTools"));
            assert!(at("--input-format") < at("--disallowedTools"));
            assert!(at("--append-system-prompt") < at("--"));
        }
        // Card runs keep the prompt on argv and get no status instruction.
        let args = build_args(&cfg(RunMode::Implement));
        assert!(!args.contains(&"--input-format".to_string()));
        assert!(!args.contains(&"--append-system-prompt".to_string()));
    }

    /// The prompt is appended by `start`, so argv must end with `--`: a
    /// description opening on a markdown bullet would otherwise be parsed as a
    /// flag and the run would die at "unknown option '- ...'".
    #[test]
    fn every_mode_terminates_options_before_the_prompt() {
        for mode in [
            RunMode::Plan,
            RunMode::Implement,
            RunMode::ApplyFixes,
            RunMode::Review,
            RunMode::Triage,
            RunMode::Question,
            RunMode::Investigate,
            RunMode::SessionFull,
            RunMode::SessionReadOnly,
        ] {
            let args = build_args(&cfg(mode));
            assert_eq!(
                args.last().map(String::as_str),
                Some("--"),
                "{mode:?} must end with the option terminator"
            );
        }
        let mut c = cfg(RunMode::Implement);
        c.resume_session = Some("sess-123".into());
        assert_eq!(build_args(&c).last().map(String::as_str), Some("--"));
    }

    /// Drive a `TurnGate` + `PlanGate` over the raw lines of a run, returning the
    /// forwarded events — mirrors exactly what `pump` does per line and at EOF.
    fn drive_gate(lines: &[&str]) -> Vec<AgentEvent> {
        let mut gate = PlanGate::default();
        let mut turns = TurnGate::default();
        let mut out = Vec::new();
        for line in lines {
            gate.observe(line);
            out.extend(gate.gate(line, turns.hold(parse_claude_line(line).unwrap())));
        }
        match turns.finish() {
            Some(evt) => out.extend(gate.gate("", vec![evt])),
            None => out.extend(gate.finish()),
        }
        out
    }

    fn kinds(events: &[AgentEvent]) -> Vec<&'static str> {
        events
            .iter()
            .map(|e| match e {
                AgentEvent::PlanReady { .. } => "plan",
                AgentEvent::Done { .. } => "done",
                AgentEvent::Error { .. } => "error",
                AgentEvent::Progress { .. } => "progress",
                _ => "other",
            })
            .collect()
    }

    fn resumed_notes(events: &[AgentEvent]) -> usize {
        events
            .iter()
            .filter(
                |e| matches!(e, AgentEvent::Progress { text } if text.contains("agent resumed")),
            )
            .count()
    }

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;

    fn plans(events: &[AgentEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::PlanReady { plan } => Some(plan.clone()),
                _ => None,
            })
            .collect()
    }

    // A rejected ExitPlanMode: the buffered draft is dropped and the plan that
    // surfaces is the agent's real final result — the one carrying the questions.
    #[test]
    fn plan_gate_prefers_final_result_when_exit_plan_rejected() {
        let events = drive_gate(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"ExitPlanMode","input":{"plan":"early draft — see questions below"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"rejected"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Full plan.\n```usine-questions\n[{\"question\":\"A?\"}]\n```","total_cost_usd":0.1}"#,
        ]);
        let plans = plans(&events);
        assert_eq!(plans.len(), 1, "exactly one plan should surface");
        assert!(
            plans[0].contains("usine-questions"),
            "surfaced plan must be the final result (with questions)"
        );
        assert!(
            !plans[0].contains("early draft"),
            "the pre-empted tool draft must not be surfaced"
        );
        // PlanReady is emitted *before* the terminal Done so the executor sees
        // AwaitingApproval before the Done lands.
        let kinds: Vec<_> = events
            .iter()
            .map(|e| match e {
                AgentEvent::PlanReady { .. } => "plan",
                AgentEvent::Done { .. } => "done",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["plan", "done"]);
    }

    // An approved (un-rejected) ExitPlanMode is trusted verbatim, not replaced by
    // the (possibly terse) trailing result.
    #[test]
    fn plan_gate_trusts_approved_exit_plan_draft() {
        let events = drive_gate(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"ExitPlanMode","input":{"plan":"the approved plan"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"User has approved your plan."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok, starting","total_cost_usd":0.1}"#,
        ]);
        assert_eq!(plans(&events), vec!["the approved plan".to_string()]);
    }

    // A rejected ExitPlanMode whose run then produced no usable final text falls
    // back to the buffered draft rather than surfacing an empty plan.
    #[test]
    fn plan_gate_falls_back_to_draft_when_rejected_result_is_empty() {
        let events = drive_gate(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"ExitPlanMode","input":{"plan":"the draft plan"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"rejected"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"","total_cost_usd":0.1}"#,
        ]);
        assert_eq!(plans(&events), vec!["the draft plan".to_string()]);
    }

    // No ExitPlanMode: ordinary events pass through untouched.
    #[test]
    fn plan_gate_passes_through_runs_without_a_plan() {
        let events = drive_gate(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.1}"#,
        ]);
        assert!(plans(&events).is_empty());
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));
    }

    // A turn that ends waiting on background work is interim: the run's single
    // Done is the *last* result, with usage summed and the last cost.
    #[test]
    fn turn_gate_forwards_only_the_final_result() {
        let events = drive_gate(&[
            INIT,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Waiting for the app to come up…","total_cost_usd":0.5,"usage":{"input_tokens":10,"output_tokens":1}}"#,
            INIT,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"app is up, testing"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Waiting on the preview file.","total_cost_usd":0.8,"usage":{"input_tokens":20,"output_tokens":2}}"#,
            INIT,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.\n```usine-handoff\n{}\n```","total_cost_usd":1.2,"usage":{"input_tokens":30,"output_tokens":3}}"#,
        ]);
        let dones: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Done {
                    result,
                    cost_usd,
                    usage,
                } => Some((result.clone(), *cost_usd, *usage)),
                _ => None,
            })
            .collect();
        assert_eq!(dones.len(), 1, "exactly one Done per process");
        let (result, cost, usage) = &dones[0];
        assert!(
            result.contains("usine-handoff"),
            "Done must carry the final answer"
        );
        assert_eq!(*cost, 1.2);
        assert_eq!(
            *usage,
            Usage {
                input_tokens: 60,
                output_tokens: 6
            }
        );
        assert_eq!(kinds(&events).last(), Some(&"done"));
        // One note per resumption.
        assert_eq!(resumed_notes(&events), 2);
    }

    // The plan is resolved against the *final* result, even when the
    // ExitPlanMode rejection happened in an earlier turn.
    #[test]
    fn plan_gate_resolves_against_the_last_turn() {
        let events = drive_gate(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"ExitPlanMode","input":{"plan":"early draft"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"rejected"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Waiting on the build.","total_cost_usd":0.1}"#,
            INIT,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"Full plan.\n```usine-questions\n[{\"question\":\"A?\"}]\n```","total_cost_usd":0.2}"#,
        ]);
        let plans = plans(&events);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].contains("usine-questions"));
        let tail: Vec<_> = kinds(&events)
            .into_iter()
            .filter(|k| *k != "progress" && *k != "other")
            .collect();
        assert_eq!(tail, vec!["plan", "done"]);
    }

    // An interim failed turn followed by a successful one: the run succeeded.
    #[test]
    fn turn_gate_a_later_success_supersedes_an_interim_error() {
        let events = drive_gate(&[
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"tool blew up"}"#,
            INIT,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"recovered","total_cost_usd":0.3}"#,
        ]);
        let k = kinds(&events);
        assert!(!k.contains(&"error"));
        assert_eq!(k.iter().filter(|k| **k == "done").count(), 1);
    }

    // A single-turn run gets no resumption note.
    #[test]
    fn turn_gate_single_turn_has_no_resumption_note() {
        let events = drive_gate(&[
            INIT,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"done","total_cost_usd":0.1}"#,
        ]);
        assert_eq!(resumed_notes(&events), 0);
        assert_eq!(kinds(&events), vec!["other", "progress", "done"]);
    }

    // A held result is only released by `finish`; a cancelled run never calls
    // it, so nothing terminal is forwarded mid-stream.
    #[test]
    fn turn_gate_holds_terminal_events_until_finish() {
        let mut turns = TurnGate::default();
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"interim","total_cost_usd":0.1}"#;
        assert!(turns.hold(parse_claude_line(line).unwrap()).is_empty());
        assert!(matches!(turns.finish(), Some(AgentEvent::Done { .. })));
        assert!(turns.finish().is_none());
    }

    // A result read while a live message was unechoed answered the turn
    // before it: it is joined ahead of the reply to the late message, and the
    // agent picking that message up is not taken for a background resumption.
    #[test]
    fn turn_gate_keeps_a_result_read_before_a_late_live_message() {
        let mut turns = TurnGate::default();
        let first = r#"{"type":"result","subtype":"success","is_error":false,"result":"main answer","total_cost_usd":0.1}"#;
        let next = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"on it"}]}}"#;
        let second = r#"{"type":"result","subtype":"success","is_error":false,"result":"steer reply","total_cost_usd":0.2}"#;
        assert!(turns.hold(parse_claude_line(first).unwrap()).is_empty());
        turns.keep_held();
        let passed = turns.hold(parse_claude_line(next).unwrap());
        assert_eq!(resumed_notes(&passed), 0);
        assert!(turns.hold(parse_claude_line(second).unwrap()).is_empty());
        match turns.finish() {
            Some(AgentEvent::Done { result, .. }) => {
                assert_eq!(result, "main answer\n\nsteer reply")
            }
            other => panic!("expected Done, got {other:?}"),
        }

        // No later result (the message was never handled): the kept one stands.
        let mut turns = TurnGate::default();
        turns.hold(parse_claude_line(first).unwrap());
        turns.keep_held();
        match turns.finish() {
            Some(AgentEvent::Done { result, .. }) => assert_eq!(result, "main answer"),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    const RESULT: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"final answer","total_cost_usd":0.1}"#;

    /// Run `pump_with_grace` over a `sh -c script` child and collect what it
    /// forwards, failing if the run doesn't end within `limit`.
    async fn pump_script(script: &str, grace: Duration, limit: Duration) -> Vec<AgentEvent> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take();
        let (evt_tx, evt_rx) = mpsc::unbounded();
        let (_ctl_tx, ctl_rx) = mpsc::unbounded();
        timeout(
            limit,
            pump(child, stdout, stderr, evt_tx, ctl_rx, grace, None),
        )
        .await
        .expect("run should have ended");
        evt_rx.collect().await
    }

    const REPLAY_PROMPT: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"do it"}]},"isReplay":true}"#;
    const REPLAY_STEER: &str = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"also this"}]},"isReplay":true}"#;

    /// Start a session-style run over a fake `claude` (`sh -c script`) whose
    /// stdin is piped and already carries the prompt, as `start` does.
    async fn live_script(
        script: &str,
    ) -> (
        tokio::task::JoinHandle<()>,
        mpsc::UnboundedSender<RunControl>,
        mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let live = LiveInput::open(child.stdin.take().unwrap(), "do it")
            .await
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take();
        let (evt_tx, evt_rx) = mpsc::unbounded();
        let (ctl_tx, ctl_rx) = mpsc::unbounded();
        let task = tokio::spawn(pump(
            child,
            stdout,
            stderr,
            evt_tx,
            ctl_rx,
            Duration::from_secs(60),
            Some(live),
        ));
        (task, ctl_tx, evt_rx)
    }

    async fn message(ctl: &mpsc::UnboundedSender<RunControl>, text: &str) -> bool {
        let (tx, rx) = futures::channel::oneshot::channel();
        ctl.unbounded_send(RunControl::Message {
            text: text.into(),
            accepted: tx,
        })
        .unwrap();
        rx.await.unwrap_or(false)
    }

    // A live message is written while stdin is open, its echo is noted, and
    // the turn's result then closes stdin — which is what lets the CLI exit
    // (the fake waits on `cat` for EOF).
    #[tokio::test]
    async fn pump_writes_live_messages_and_closes_stdin_at_the_result() {
        let script = format!(
            "read p; echo '{REPLAY_PROMPT}'; read m; echo '{REPLAY_STEER}'; \
             echo '{RESULT}'; cat >/dev/null; exit 0"
        );
        let (task, ctl, events) = live_script(&script).await;
        assert!(message(&ctl, "also this").await);
        timeout(Duration::from_secs(10), task)
            .await
            .expect("stdin should close at the result so the run ends")
            .unwrap();
        let events: Vec<AgentEvent> = events.collect().await;
        assert!(final_done(&events), "{:?}", kinds(&events));
        let picked: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Progress { text } if text.starts_with("↪ picked up") => Some(text),
                _ => None,
            })
            .collect();
        // The turn's own prompt is echoed too, but only the live message is noted.
        assert_eq!(picked, vec!["↪ picked up: also this"]);
    }

    // Once the result closed stdin, a later message is refused (the caller
    // queues it), even though the process is still finishing up.
    #[tokio::test]
    async fn pump_refuses_live_messages_once_stdin_is_closed() {
        let script =
            format!("read p; echo '{REPLAY_PROMPT}'; echo '{RESULT}'; cat >/dev/null; sleep 1");
        let (task, ctl, events) = live_script(&script).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!message(&ctl, "too late").await);
        timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        let events: Vec<AgentEvent> = events.collect().await;
        assert!(final_done(&events), "{:?}", kinds(&events));
    }

    // Non-session runs have no stdin to write to.
    #[tokio::test]
    async fn pump_refuses_live_messages_without_live_input() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (evt_tx, _evt_rx) = mpsc::unbounded();
        let (ctl_tx, ctl_rx) = mpsc::unbounded();
        let task = tokio::spawn(pump(
            child,
            stdout,
            None,
            evt_tx,
            ctl_rx,
            Duration::from_secs(60),
            None,
        ));
        assert!(!message(&ctl_tx, "hi").await);
        ctl_tx.unbounded_send(RunControl::Cancel).unwrap();
        task.await.unwrap();
    }

    fn final_done(events: &[AgentEvent]) -> bool {
        matches!(events.last(), Some(AgentEvent::Done { result, .. }) if result == "final answer")
    }

    // `claude` exited but a background grandchild still holds stdout: the run
    // ends at the process exit with its result, not at a never-coming EOF.
    #[tokio::test]
    async fn pump_ends_when_claude_exits_even_if_a_grandchild_holds_stdout() {
        let script = format!("echo '{RESULT}'; sleep 30 & exit 0");
        let events = pump_script(&script, Duration::from_secs(60), Duration::from_secs(10)).await;
        assert!(final_done(&events), "{:?}", kinds(&events));
    }

    // A held result and then silence: after the grace the result is released
    // rather than left for the executor's watchdog to fail the run.
    #[tokio::test]
    async fn pump_releases_a_held_result_after_the_quiet_grace() {
        let script = format!("echo '{RESULT}'; exec sleep 30");
        let events =
            pump_script(&script, Duration::from_millis(300), Duration::from_secs(10)).await;
        assert!(final_done(&events), "{:?}", kinds(&events));
        assert!(events.iter().any(
            |e| matches!(e, AgentEvent::Progress { text } if text.contains("using that turn's result"))
        ));
    }

    // The grace only runs while a result is held: a slow first turn isn't cut
    // short.
    #[tokio::test]
    async fn pump_grace_does_not_apply_before_any_result() {
        let script = format!("sleep 1; echo '{RESULT}'");
        let events =
            pump_script(&script, Duration::from_millis(100), Duration::from_secs(10)).await;
        assert!(final_done(&events), "{:?}", kinds(&events));
    }
}
