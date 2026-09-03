//! Claude provider: drives the `claude` binary in headless stream-json mode.
//!
//! Each phase is a one-shot run: we pass the prompt as a positional arg, stream
//! the NDJSON stdout through [`super::stream::parse_claude_line`], and exit when
//! the process does. The plan phase runs read-only (`--permission-mode plan`);
//! implement/fix phases run autonomously (`--dangerously-skip-permissions`) in
//! the card's worktree. Relies on the user's existing `claude` auth.

use std::process::Stdio;

use async_trait::async_trait;
use futures::channel::mpsc;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::agent::events::{AgentEvent, RunControl};
use crate::domain::model::{supported_efforts, Provider};
use crate::error::{CoreError, Result};

use super::stream::{errored_tool_result_ids, exit_plan_mode_tool_id, parse_claude_line};
use super::{AgentProvider, RunConfig, RunHandle, RunMode};

/// Build the argv for a `claude` headless run, **excluding** the binary name and
/// the prompt (the prompt is appended as the final positional arg by `start`).
///
/// - Plan → `--permission-mode plan` (read-only) + a caller-supplied
///   `--session-id`.
/// - Implement / ApplyFixes → `--dangerously-skip-permissions` so the agent can
///   edit files and run commands autonomously inside the isolated worktree. The
///   approved plan / fix list rides along in the prompt, so no resume is needed.
///
/// Every mode also denies the background-orchestration tools (`Agent`, `Task`,
/// `ScheduleWakeup`, `Workflow`): a one-shot `claude -p` run is single-turn, but
/// those tools let the agent launch async sub-agents and end its turn early, so
/// the run reports that interim turn as its result instead of the real
/// plan/edits. Denying them forces the agent to finish in one turn.
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

    // Single-turn guard: deny the tools that let the agent spawn background work
    // or schedule a later turn. Pushed *before* the mode flag below so this
    // variadic option is bounded by the next flag and can't swallow the trailing
    // prompt positional that `start` appends.
    args.push("--disallowedTools".into());
    args.push("Agent,Task,ScheduleWakeup,Workflow".into());

    match cfg.mode {
        // Read-only phases (planning, self-review, comment triage, Q&A,
        // investigation): restrict to read-only tools so the agent can inspect
        // but never edit.
        RunMode::Plan
        | RunMode::Review
        | RunMode::Triage
        | RunMode::Question
        | RunMode::Investigate => {
            args.push("--permission-mode".into());
            args.push("plan".into());
        }
        RunMode::Implement | RunMode::ApplyFixes => {
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
        let mut args = build_args(&cfg);
        args.push(cfg.full_prompt());

        let mut child = Command::new("claude")
            .args(&args)
            .current_dir(&cfg.project_dir)
            .stdin(Stdio::null())
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

        let (evt_tx, evt_rx) = mpsc::unbounded::<AgentEvent>();
        let (ctl_tx, ctl_rx) = mpsc::unbounded::<RunControl>();

        tokio::spawn(pump(child, stdout, stderr, evt_tx, ctl_rx));

        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
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

/// Read the NDJSON stream to completion, forwarding normalized events. Reacts to
/// Cancel by killing the child. (One-shot runs can't inject answers mid-flight,
/// so `Answer` controls are ignored.)
async fn pump(
    mut child: Child,
    stdout: tokio::process::ChildStdout,
    stderr: Option<tokio::process::ChildStderr>,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    mut ctl_rx: mpsc::UnboundedReceiver<RunControl>,
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

    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    gate.observe(&line);
                    if let Ok(events) = parse_claude_line(&line) {
                        for evt in gate.gate(&line, events) {
                            if matches!(
                                evt,
                                AgentEvent::Done { .. } | AgentEvent::Error { .. } | AgentEvent::PlanReady { .. }
                            ) {
                                saw_terminal = true;
                            }
                            let _ = evt_tx.unbounded_send(evt);
                        }
                    }
                }
                _ => break, // EOF or read error
            },
            ctl = ctl_rx.next(), if ctl_open => match ctl {
                Some(RunControl::Cancel) | Some(RunControl::Interrupt) => {
                    cancelled = true;
                    let _ = child.start_kill();
                    break;
                }
                Some(RunControl::Answer { .. }) => { /* one-shot: no stdin to write to */ }
                None => ctl_open = false,
            },
        }
    }

    // End-of-stream: surface any plan still held by the gate (an un-rejected
    // plan whose run produced no trailing `result` line to resolve it at).
    if !cancelled && !saw_terminal {
        if let Some(evt) = gate.finish() {
            saw_terminal = true;
            let _ = evt_tx.unbounded_send(evt);
        }
    }

    let status = child.wait().await.ok();
    let stderr_text = stderr_task.await.unwrap_or_default();

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
        assert_eq!(
            pair(&build_args(&c), "--effort").as_deref(),
            Some("max")
        );
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

    /// Drive a `PlanGate` over the raw lines of a run, returning the forwarded
    /// events — mirrors exactly what `pump` does per line.
    fn drive_gate(lines: &[&str]) -> Vec<AgentEvent> {
        let mut gate = PlanGate::default();
        let mut out = Vec::new();
        for line in lines {
            gate.observe(line);
            out.extend(gate.gate(line, parse_claude_line(line).unwrap()));
        }
        out.extend(gate.finish());
        out
    }

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
}
