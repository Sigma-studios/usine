//! The pre-PR validation gate: run the project's validate command in the card's
//! worktree, and loop failures through an agent fix run.
//!
//! The executor applies `StartValidation` whenever a card reaches `ReadyForPr`
//! with a validate command configured (see the trigger sites in `self_review.rs`
//! and `actor.rs`), so `ReadyForPr` means "cleared for PR". A failing check
//! feeds its output tail to an `ApplyFixes` run and re-runs afterwards, bounded
//! by [`MAX_VALIDATION_ATTEMPTS`]; exhaustion parks the card with the output
//! and the user's options (re-run / one more fix / open the PR anyway).
//!
//! The check itself is not an agent turn, but it registers in the same `runs`
//! map, so cancel, card teardown, and the supersede guard all treat it exactly
//! like one.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;

use super::preview::kill_group;
use super::*;
use crate::domain::config::ProjectConfig;
use crate::domain::state_machine::MAX_VALIDATION_ATTEMPTS;

/// Wall-clock ceiling for one check run. A build-and-test can legitimately take
/// a while; a hang should fail the card (retryable), not burn a fix attempt on
/// feeding a timeout message to the agent.
const VALIDATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How much of the check's output tail is kept for the fix prompt and the
/// parked panel. The interesting part of a failing build/test run is almost
/// always the end.
const MAX_TAIL_LINES: usize = 200;
const MAX_TAIL_BYTES: usize = 16 * 1024;

/// The project's validate command, trimmed to `None` if unset/blank.
pub(super) fn validate_command(config: &ProjectConfig) -> Option<String> {
    config
        .validate_script
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Executor {
    /// Run the validation gate for a card. A no-op when the project has no
    /// validate command or the card isn't at the gate, so the internal eager
    /// dispatches and a user's stale click are both harmless.
    pub(super) async fn run_validation(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let Some(cmd) = validate_command(&project.config) else {
            return Ok(());
        };
        match &card.state {
            // Entering the gate (or the user re-running it from the parked
            // failure, which resets the attempt budget). Isolate before the
            // running-state transition, so a worktree failure leaves the card
            // recoverable where it stands.
            CardState::AwaitingReview(
                ReviewSub::ReadyForPr | ReviewSub::ValidationFailed { .. },
            ) => {
                self.ensure_branch_worktree(card_id).await?;
                self.apply(card_id, Transition::StartValidation)?;
            }
            // Already mid-gate: the post-fix re-check, or a retry after a crash
            // restored `Validating`. Just make sure the worktree is there.
            CardState::AwaitingReview(ReviewSub::Validating { .. }) => {
                self.ensure_branch_worktree(card_id).await?;
            }
            _ => return Ok(()),
        }
        let card = self.store.get_card(card_id)?;
        let CardState::AwaitingReview(ReviewSub::Validating { attempt }) = card.state else {
            return Ok(());
        };
        let worktree = card
            .worktree_path
            .clone()
            .ok_or_else(|| CoreError::other("card has no worktree to validate in"))?;

        // Register the check as the card's active run: cancel, teardown, and
        // the supersede guard all work through this slot.
        let run_id = Uuid::new_v4();
        let (ctl_tx, ctl_rx) = mpsc::unbounded::<RunControl>();
        lock(&self.runs).insert(card_id, (run_id, ctl_tx));

        self.progress(
            card_id,
            &format!(
                "▶ validation ({}/{MAX_VALIDATION_ATTEMPTS}): {cmd}",
                attempt.min(MAX_VALIDATION_ATTEMPTS)
            ),
        );
        tokio::spawn(validation_actor(
            self.store.clone(),
            self.evt_tx.clone(),
            self.self_ref.clone(),
            Arc::clone(&self.runs),
            Arc::clone(&self.validations),
            card_id,
            run_id,
            attempt,
            cmd,
            worktree,
            ctl_rx,
        ));
        Ok(())
    }

    /// Launch the agent run that fixes a validation failure. Dual-use: the
    /// internal path arrives already in `FixingValidation` (the state machine
    /// routed the failed check there); the user path arrives from the parked
    /// `ValidationFailed` and consumes one more fix cycle.
    pub(super) async fn fix_validation(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        match &card.state {
            CardState::AwaitingReview(ReviewSub::ValidationFailed { .. }) => {
                // Isolate before the running-state transition (see run_validation).
                self.ensure_branch_worktree(card_id).await?;
                self.apply(card_id, Transition::FixValidation)?;
            }
            CardState::AwaitingReview(ReviewSub::FixingValidation { .. }) => {
                self.ensure_branch_worktree(card_id).await?;
            }
            _ => return Ok(()),
        }
        let card = self.store.get_card(card_id)?;
        let CardState::AwaitingReview(ReviewSub::FixingValidation { attempt, output }) =
            &card.state
        else {
            return Ok(());
        };
        let project = self.store.get_project(card.project_id)?;
        // The command is only quoted in the prompt; an unset one (config edited
        // mid-gate) still lets the fix run proceed on the captured output.
        let cmd = validate_command(&project.config).unwrap_or_default();
        let extra = validation_fix_prompt(&cmd, *attempt, output);
        self.launch(card.clone(), RunMode::ApplyFixes, Some(extra), None)
            .await
    }

    /// From the parked validation failure: give up on the gate and open the PR
    /// form anyway. The transcript marker lands before the state flip so the
    /// `CardUpdated` is this handler's last word.
    pub(super) fn skip_validation(&self, card_id: Uuid) -> Result<()> {
        self.progress(card_id, "▷ validation skipped");
        self.apply(card_id, Transition::SkipValidation)?;
        Ok(())
    }
}

/// Launch the fix run from the check actor with a direct executor call (never
/// a re-dispatched command — see the call site). An error would strand the
/// card in `FixingValidation` (running, no run), so it demotes to `Failed`.
fn fix_validation_direct(
    executor: &Weak<Executor>,
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
) {
    let Some(exec) = executor.upgrade() else {
        return;
    };
    let store = store.clone();
    let evt_tx = evt_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = exec.fix_validation(card_id).await {
            let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Error,
                e.to_string(),
            ));
            if store
                .get_card(card_id)
                .map(|c| c.state.is_running())
                .unwrap_or(false)
            {
                let _ = apply_transition(
                    &store,
                    &evt_tx,
                    card_id,
                    Transition::AgentError {
                        message: e.to_string(),
                    },
                );
            }
        }
    });
}

/// A bounded tail of the check's output: the last [`MAX_TAIL_LINES`] lines
/// capped at [`MAX_TAIL_BYTES`], with a truncation marker once anything fell
/// off. What the fix prompt and the parked panel show.
#[derive(Default)]
struct OutputTail {
    lines: std::collections::VecDeque<String>,
    bytes: usize,
    truncated: bool,
}

impl OutputTail {
    fn push(&mut self, line: String) {
        self.bytes += line.len() + 1;
        self.lines.push_back(line);
        while self.lines.len() > MAX_TAIL_LINES || self.bytes > MAX_TAIL_BYTES {
            if let Some(dropped) = self.lines.pop_front() {
                self.bytes -= dropped.len() + 1;
                self.truncated = true;
            } else {
                break;
            }
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        if self.truncated {
            out.push_str("…(output truncated)\n");
        }
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.trim_end().to_string()
    }
}

type SharedTail = Arc<Mutex<OutputTail>>;

/// Forward a child stream's lines to the UI transcript live (not persisted —
/// the preview precedent) while also collecting the bounded tail.
fn spawn_capture_reader<S: AsyncRead + Unpin + Send + 'static>(
    evt_tx: UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    stream: S,
    tail: SharedTail,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            lock(&tail).push(line.clone());
            let _ = evt_tx.unbounded_send(ExecutorEvent::transcript(card_id, now_millis(), line));
        }
    })
}

/// The check run itself: spawn the command in the worktree, stream + capture
/// its output, and route the exit through the state machine. Mirrors
/// `run_actor`'s contract: it owns its `runs` slot until it exits, applies only
/// agent-driven transitions, and backs off silently when superseded.
#[allow(clippy::too_many_arguments)]
async fn validation_actor(
    store: Store,
    evt_tx: UnboundedSender<ExecutorEvent>,
    executor: Weak<Executor>,
    runs: RunMap,
    validations: ValidationMap,
    card_id: Uuid,
    run_id: Uuid,
    attempt: u32,
    cmd: String,
    worktree: PathBuf,
    mut ctl_rx: UnboundedReceiver<RunControl>,
) {
    let cleanup = |validations: &ValidationMap, runs: &RunMap| {
        lock(validations).remove(&card_id);
        // Only clear the runs slot if it's still ours — a newer run for this
        // card may have already replaced it.
        let mut map = lock(runs);
        if map
            .get(&card_id)
            .map(|(rid, _)| *rid == run_id)
            .unwrap_or(false)
        {
            map.remove(&card_id);
        }
    };

    let mut command = Command::new("sh");
    command
        .arg("-lc")
        .arg(&cmd)
        .current_dir(&worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so cancel/shutdown can reap the command's whole tree.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = apply_transition(
                &store,
                &evt_tx,
                card_id,
                Transition::AgentError {
                    message: format!("failed to run the validate command `{cmd}`: {e}"),
                },
            );
            cleanup(&validations, &runs);
            return;
        }
    };
    let pid = child.id();
    if let Some(pid) = pid {
        lock(&validations).insert(card_id, pid);
    }

    let tail: SharedTail = Arc::new(Mutex::new(OutputTail::default()));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_capture_reader(
            evt_tx.clone(),
            card_id,
            out,
            Arc::clone(&tail),
        ));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_capture_reader(
            evt_tx.clone(),
            card_id,
            err,
            Arc::clone(&tail),
        ));
    }

    let timeout = tokio::time::sleep(VALIDATION_TIMEOUT);
    tokio::pin!(timeout);
    let status = loop {
        tokio::select! {
            ctl = ctl_rx.next() => match ctl {
                // Cancelled (or the executor dropped the channel): kill the
                // tree and exit without applying anything — `cancel()` /
                // teardown already moved the card where it belongs.
                Some(RunControl::Cancel) | None => {
                    if let Some(pid) = pid {
                        kill_group(pid).await;
                    }
                    let _ = child.wait().await;
                    cleanup(&validations, &runs);
                    return;
                }
                // Answer/Interrupt are meaningless for a script.
                Some(_) => continue,
            },
            status = child.wait() => break status,
            _ = &mut timeout => {
                if let Some(pid) = pid {
                    kill_group(pid).await;
                }
                let _ = child.wait().await;
                let _ = apply_transition(&store, &evt_tx, card_id, Transition::AgentError {
                    message: format!(
                        "validation timed out after {} min",
                        VALIDATION_TIMEOUT.as_secs() / 60
                    ),
                });
                cleanup(&validations, &runs);
                return;
            }
        }
    };
    // Let the readers drain what the child wrote before we snapshot the tail.
    for r in readers {
        let _ = r.await;
    }

    // Only route the exit if the card is still waiting on THIS check — a card
    // that moved on (cancel racing the exit, delete, …) abandoned it.
    let still_ours = store.get_card(card_id).is_ok_and(|c| {
        matches!(
            c.state,
            CardState::AwaitingReview(ReviewSub::Validating { attempt: a }) if a == attempt
        )
    });
    if !still_ours {
        cleanup(&validations, &runs);
        return;
    }

    match status {
        Ok(s) if s.success() => {
            let _ = apply_transition(&store, &evt_tx, card_id, Transition::ValidationPassed);
            transcript(&store, &evt_tx, card_id, "✔ validation passed".to_string());
        }
        Ok(s) => {
            let mut output = lock(&tail).render();
            if output.is_empty() {
                output = "(no output)".to_string();
            }
            output.push_str(&format!("\n[{s}]"));
            // Effects are keyed off the resulting state (the module rule):
            // inside the budget the state machine routed to the fix run; past
            // it the card parked.
            if let Ok(card) = apply_transition(
                &store,
                &evt_tx,
                card_id,
                Transition::ValidationFailed { output },
            ) {
                match card.state {
                    CardState::AwaitingReview(ReviewSub::FixingValidation { .. }) => {
                        transcript(
                            &store,
                            &evt_tx,
                            card_id,
                            format!(
                                "✖ validation failed — sending the output to the agent \
                                 ({attempt}/{MAX_VALIDATION_ATTEMPTS})"
                            ),
                        );
                        // A DIRECT call: a re-dispatched FixValidation command
                        // can be dropped by the in-flight claim of the very
                        // handler that spawned this actor (a fast-failing check
                        // finishes before the claim is released), stranding the
                        // card in `FixingValidation` with no run behind it.
                        fix_validation_direct(&executor, &store, &evt_tx, card_id);
                    }
                    CardState::AwaitingReview(ReviewSub::ValidationFailed { .. }) => {
                        transcript(
                            &store,
                            &evt_tx,
                            card_id,
                            "✖ validation failed — attempt budget exhausted".to_string(),
                        );
                    }
                    _ => {}
                }
            }
        }
        Err(e) => {
            let _ = apply_transition(
                &store,
                &evt_tx,
                card_id,
                Transition::AgentError {
                    message: format!("validation command failed to run: {e}"),
                },
            );
        }
    }
    cleanup(&validations, &runs);
}
