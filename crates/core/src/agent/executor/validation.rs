//! The pre-PR validation gate: prepare the card's worktree, run the project's
//! validate command in it, and loop failures through an agent fix run.
//!
//! The gate is two supervised steps sharing one deadline — the project's setup
//! command (skipped when it has none), then the check — because the check is
//! only meaningful in a worktree that was made runnable first: a fresh worktree
//! has no installed dependencies, no isolated database, and no allocated ports
//! until setup puts them there.
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

use super::actor::reap_idle_preview_direct;
use super::preview::kill_group;
use super::*;
use crate::domain::config::ProjectConfig;
use crate::domain::state_machine::MAX_VALIDATION_ATTEMPTS;

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
        self.run_validation_admitted(card_id, None).await
    }

    /// `run_validation` behind the concurrency gate. `admitted` is the queue
    /// pump's pre-claimed slot (see `launch_admitted`); every other caller
    /// passes `None` and admits here. Early back-offs drop the guard, which
    /// releases the slot.
    pub(super) async fn run_validation_admitted(
        &self,
        card_id: Uuid,
        admitted: Option<(Uuid, super::gate::SlotGuard)>,
    ) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let Some(cmd) = validate_command(&project.config) else {
            // No gate configured: every fresh entry to `ReadyForPr` funnels
            // through here, so this is where "parked with no gate" lands —
            // light-stop the preview the run left behind. The helper's
            // running-state guard makes it a no-op on any non-parked caller.
            self.reap_idle_preview(card_id).await;
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
        let timeout = project.config.validate_timeout();

        // The concurrency gate: either take a slot now or park in the queue —
        // the card stays `Validating` and the pump re-enters this function
        // (through its already-mid-gate arm) when a slot frees. The run id
        // doubles as the slot's admission generation (see `gate.rs`).
        let (run_id, slot) = match admitted {
            Some((run_id, guard)) => (run_id, guard),
            None => {
                let run_id = Uuid::new_v4();
                let entry = super::gate::QueuedRun::Validation { card_id };
                match self.admit_or_enqueue(run_id, entry) {
                    Some(guard) => (run_id, guard),
                    None => return Ok(()),
                }
            }
        };

        // The gate runs in the same prepared worktree a preview would: the setup
        // command owns "make this worktree runnable" (dependencies, an isolated
        // database, per-worktree ports), so without it the check reports on a
        // missing environment rather than on the work. Re-running it here is safe
        // by the same contract that lets every preview start re-run it.
        //
        // Except when the card's preview slot is taken: that preview's own setup
        // already prepared this worktree, and it is still live (the reap skips
        // running cards), so re-running setup underneath it would reset the very
        // database and ports the running app is using.
        let setup = if lock(&self.previews).contains_key(&card_id) {
            None
        } else {
            super::preview::resolve_script(
                &project.config.worktree_setup_script,
                &worktree,
                super::preview::SETUP_CANDIDATES,
            )
        };

        // Register the check as the card's active run: cancel, teardown, and
        // the supersede guard all work through this slot.
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
            setup,
            cmd,
            worktree,
            timeout,
            ctl_rx,
            slot,
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
                let demoted = apply_transition(
                    &store,
                    &evt_tx,
                    card_id,
                    Transition::AgentError {
                        message: e.to_string(),
                    },
                )
                .is_ok();
                // Failing to launch the fix run (e.g. the provider CLI is
                // missing) parks the card at `Failed` with no actor behind it
                // — light-stop the preview the pipeline left up.
                if demoted {
                    exec.reap_idle_preview(card_id).await;
                }
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

/// How one supervised step of the gate ended. Setup and the check run exactly
/// the same way, so they share this; what differs is how a failure is routed —
/// see `validation_actor`.
enum StepOutcome {
    /// Exited zero.
    Passed,
    /// Exited non-zero, carrying the rendered output tail (exit status included).
    Failed(String),
    /// Cancelled, or the control channel closed. `cancel()`/teardown has already
    /// moved the card, so the caller must return without applying anything.
    Cancelled,
    /// Ran past the gate's shared deadline.
    TimedOut,
    /// The command never started, or waiting on it failed — a broken environment
    /// rather than a verdict on the work.
    Broken(String),
}

/// Run one step of the gate to completion in the worktree: spawn it in its own
/// process group, stream its output to the transcript while collecting the
/// bounded tail, and supervise it against cancel and the gate's shared
/// `deadline`.
///
/// The pid is registered in `validations` for as long as the step runs, so app
/// shutdown can reap it. The two steps overwrite each other's entry there, which
/// is harmless: they never overlap, and the actor's `cleanup` clears the slot.
async fn run_step(
    evt_tx: &UnboundedSender<ExecutorEvent>,
    validations: &ValidationMap,
    card_id: Uuid,
    cmd: &str,
    worktree: &Path,
    ctl_rx: &mut UnboundedReceiver<RunControl>,
    deadline: tokio::time::Instant,
) -> StepOutcome {
    let mut command = Command::new("sh");
    command
        .arg("-lc")
        .arg(cmd)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group so cancel/shutdown can reap the command's whole tree.
    #[cfg(unix)]
    command.process_group(0);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return StepOutcome::Broken(format!("failed to run `{cmd}`: {e}")),
    };
    let pid = child.id();
    if let Some(pid) = pid {
        lock(validations).insert(card_id, pid);
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

    let timeout = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout);
    let status = loop {
        tokio::select! {
            ctl = ctl_rx.next() => match ctl {
                // Cancelled (or the executor dropped the channel): kill the
                // tree and let the caller exit without applying anything.
                Some(RunControl::Cancel) | None => {
                    if let Some(pid) = pid {
                        kill_group(pid).await;
                    }
                    let _ = child.wait().await;
                    return StepOutcome::Cancelled;
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
                return StepOutcome::TimedOut;
            }
        }
    };
    // Let the readers drain what the child wrote before we snapshot the tail.
    for r in readers {
        let _ = r.await;
    }
    match status {
        Ok(s) if s.success() => StepOutcome::Passed,
        Ok(s) => {
            let mut output = lock(&tail).render();
            if output.is_empty() {
                output = "(no output)".to_string();
            }
            output.push_str(&format!("\n[{s}]"));
            StepOutcome::Failed(output)
        }
        Err(e) => StepOutcome::Broken(format!("`{cmd}` failed to run: {e}")),
    }
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
    // The project's worktree setup command, when it has one. Runs first.
    setup: Option<String>,
    cmd: String,
    worktree: PathBuf,
    timeout: Duration,
    mut ctl_rx: UnboundedReceiver<RunControl>,
    // The check's concurrency slot; dropping it when the actor ends releases
    // the slot and pumps the run queue. The fix run a failing exit launches
    // re-admits under its own generation, so whichever of the two lands first
    // the card ends up holding exactly one slot (see `gate.rs`).
    _slot: super::gate::SlotGuard,
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

    // Park the card at `Failed` (retryable) and light-stop the preview the
    // pipeline left up. The state machine only accepts `AgentError` on a running
    // card, so a card that moved on absorbs this as a no-op.
    let abort = |message: String| {
        let _ = apply_transition(&store, &evt_tx, card_id, Transition::AgentError { message });
        reap_idle_preview_direct(&executor, card_id);
    };

    // One deadline for the whole gate rather than one per step: what the project
    // configures is how long a trip through the gate may take, and a setup that
    // ate the entire budget has already made the check pointless.
    let deadline = tokio::time::Instant::now() + timeout;
    let timed_out = || format!("validation timed out after {} min", timeout.as_secs() / 60);

    // 1. Prepare the worktree, when the project says how.
    if let Some(setup) = &setup {
        transcript(
            &store,
            &evt_tx,
            card_id,
            format!("▶ worktree setup: {setup}"),
        );
        match run_step(
            &evt_tx,
            &validations,
            card_id,
            setup,
            &worktree,
            &mut ctl_rx,
            deadline,
        )
        .await
        {
            StepOutcome::Passed => {}
            StepOutcome::Cancelled => {
                cleanup(&validations, &runs);
                return;
            }
            StepOutcome::TimedOut => {
                abort(format!("{} (in worktree setup)", timed_out()));
                cleanup(&validations, &runs);
                return;
            }
            // A setup that fails is an environment that couldn't be prepared, not
            // a verdict on the work: parking at `Failed` is honest and costs no
            // fix attempt, where feeding it to the agent would spend the budget on
            // something the code can't fix. This matches how a preview treats its
            // own setup failure.
            StepOutcome::Failed(output) => {
                abort(format!(
                    "worktree setup failed, so validation never ran:\n{output}"
                ));
                cleanup(&validations, &runs);
                return;
            }
            StepOutcome::Broken(message) => {
                abort(message);
                cleanup(&validations, &runs);
                return;
            }
        }
    }

    // 2. The check itself.
    let verdict = run_step(
        &evt_tx,
        &validations,
        card_id,
        &cmd,
        &worktree,
        &mut ctl_rx,
        deadline,
    )
    .await;
    // `None` passed; `Some(output)` failed with that captured tail.
    let failure = match verdict {
        StepOutcome::Passed => None,
        StepOutcome::Failed(output) => Some(output),
        StepOutcome::Cancelled => {
            cleanup(&validations, &runs);
            return;
        }
        StepOutcome::TimedOut => {
            abort(timed_out());
            cleanup(&validations, &runs);
            return;
        }
        StepOutcome::Broken(message) => {
            abort(format!("validation command failed to run: {message}"));
            cleanup(&validations, &runs);
            return;
        }
    };

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

    match failure {
        None => {
            let _ = apply_transition(&store, &evt_tx, card_id, Transition::ValidationPassed);
            transcript(&store, &evt_tx, card_id, "✔ validation passed".to_string());
            // The gate is done and the card parked at `ReadyForPr` —
            // light-stop the preview the pipeline brought up.
            reap_idle_preview_direct(&executor, card_id);
        }
        Some(output) => {
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
                        // Parked at `ValidationFailed` — light-stop the preview.
                        reap_idle_preview_direct(&executor, card_id);
                    }
                    _ => {}
                }
            }
        }
    }
    cleanup(&validations, &runs);
}
