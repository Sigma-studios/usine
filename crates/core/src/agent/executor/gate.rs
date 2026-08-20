//! The run gate: a global cap on concurrently running heavy work — card runs
//! in the capped modes (see `RunMode::is_capped`), contributor-PR review runs,
//! and validation checks — with a FIFO queue that drains as slots free.
//!
//! Slot accounting is gate-owned rather than derived from the runs maps: those
//! don't record a run's mode, so exempt runs (Agent Chat questions, PR-comment
//! triage) couldn't be filtered out of a map count. A slot is keyed by the
//! holder id (card id or review-task id — a card has at most one run or
//! validation check at a time) and owned by an admission generation (the run
//! id), the same owner-match idiom as `RunMap`: a pipeline successor (e.g. the
//! fix run a failing check launches) re-admits under the same id with a new
//! generation, so the predecessor's release can't free a slot the successor
//! now holds.
//!
//! The queue is in-memory only. Crash-safety is free: startup's
//! `reconcile_interrupted_runs` demotes running-with-no-run cards to `Failed`
//! with a Resume affordance.

use std::collections::VecDeque;

use super::*;

pub(super) struct RunGate {
    /// Slot-holder id → the admission generation (run id) that owns the slot.
    active: HashMap<Uuid, Uuid>,
    queue: VecDeque<QueuedRun>,
}

impl RunGate {
    pub(super) fn new() -> Self {
        RunGate {
            active: HashMap::new(),
            queue: VecDeque::new(),
        }
    }
}

/// One deferred launch, carrying exactly what re-entering its launch path
/// needs. Card state itself isn't snapshotted — the pump re-fetches the card
/// and backs off if it left its running state (e.g. a cancel parked it).
pub(super) enum QueuedRun {
    Card {
        card_id: Uuid,
        mode: RunMode,
        extra: Option<String>,
        resume_session: Option<String>,
    },
    Review {
        review_id: Uuid,
    },
    Validation {
        card_id: Uuid,
    },
}

impl QueuedRun {
    fn id(&self) -> Uuid {
        match self {
            QueuedRun::Card { card_id, .. } | QueuedRun::Validation { card_id } => *card_id,
            QueuedRun::Review { review_id } => *review_id,
        }
    }

    fn target(&self) -> QueuedTarget {
        match self {
            QueuedRun::Card { card_id, .. } | QueuedRun::Validation { card_id } => {
                QueuedTarget::Card(*card_id)
            }
            QueuedRun::Review { review_id } => QueuedTarget::Review(*review_id),
        }
    }
}

/// RAII ownership of one admitted slot. Handed to the run's actor task, which
/// holds it for the run's lifetime; dropping it releases the slot (owner-
/// matched) and pumps the queue. Early-failure paths between admission and the
/// actor spawn release automatically the same way, so a launch that dies
/// setting up can't leak its slot.
pub(super) struct SlotGuard {
    executor: Weak<Executor>,
    id: Uuid,
    generation: Uuid,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(exec) = self.executor.upgrade() {
            exec.release_slot(self.id, self.generation);
        }
    }
}

impl Executor {
    /// The cap, read live from settings so a save applies immediately. 0 =
    /// unlimited.
    fn run_cap(&self) -> u32 {
        self.store
            .settings()
            .map(|s| s.max_concurrent_runs)
            .unwrap_or_default()
    }

    /// Claim a slot for `id` (owned by `generation`, the run id), or `None` if
    /// the cap is saturated. An id that already holds a slot is re-admitted
    /// under the new generation — that's a pipeline successor (fix run after a
    /// failed check, resumed run after an answer) taking over the card's slot,
    /// never a second concurrent run.
    pub(super) fn try_admit(&self, id: Uuid, generation: Uuid) -> Option<SlotGuard> {
        let cap = self.run_cap();
        let mut gate = lock(&self.gate);
        if gate.active.contains_key(&id) || cap == 0 || (gate.active.len() as u32) < cap {
            gate.active.insert(id, generation);
            Some(SlotGuard {
                executor: self.self_ref.clone(),
                id,
                generation,
            })
        } else {
            None
        }
    }

    /// Release `id`'s slot iff `generation` still owns it, then pump. Idempotent
    /// and safe to call from exempt runs' teardown (no slot → no-op, no pump).
    pub(super) fn release_slot(&self, id: Uuid, generation: Uuid) {
        let released = {
            let mut gate = lock(&self.gate);
            if gate.active.get(&id) == Some(&generation) {
                gate.active.remove(&id);
                true
            } else {
                false
            }
        };
        if released {
            self.spawn_pump();
        }
    }

    /// Park a launch in the queue (replacing any previous entry for the same
    /// id), tell the UI, and note it on the transcript.
    pub(super) fn enqueue_run(&self, entry: QueuedRun) {
        let id = entry.id();
        let ahead = {
            let mut gate = lock(&self.gate);
            gate.queue.retain(|e| e.id() != id);
            gate.queue.push_back(entry);
            gate.queue.len() - 1
        };
        self.emit_queue_changed();
        transcript(
            &self.store,
            &self.evt_tx,
            id,
            format!("⏸ queued — waiting for a free run slot ({ahead} ahead)"),
        );
    }

    /// Drop `id`'s queued entry, if any (cancel/teardown/dismiss). Without this
    /// the pump would later launch a run for a card the user already parked.
    pub(super) fn purge_queued(&self, id: Uuid) {
        let changed = {
            let mut gate = lock(&self.gate);
            let before = gate.queue.len();
            gate.queue.retain(|e| e.id() != id);
            gate.queue.len() != before
        };
        if changed {
            self.emit_queue_changed();
        }
    }

    /// Kick the pump on its own task (callable from sync contexts, e.g. a
    /// `SlotGuard` drop or the SaveSettings handler).
    pub(super) fn spawn_pump(&self) {
        if let Some(exec) = self.self_ref.upgrade() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move { exec.pump_run_queue().await });
            }
        }
    }

    /// While a slot is free, pop the next queued launch and dispatch it through
    /// its normal launch path (which re-admits — a lost race simply re-queues).
    /// Dispatch errors demote the target out of its now-runnerless running
    /// state, so nothing strands; a target that left its running/reviewing
    /// state while queued (cancelled, dismissed) is skipped.
    async fn pump_run_queue(self: Arc<Self>) {
        loop {
            let cap = self.run_cap();
            let entry = {
                let mut gate = lock(&self.gate);
                if cap != 0 && gate.active.len() as u32 >= cap {
                    return;
                }
                match gate.queue.pop_front() {
                    Some(e) => e,
                    None => return,
                }
            };
            self.emit_queue_changed();
            match entry {
                QueuedRun::Card {
                    card_id,
                    mode,
                    extra,
                    resume_session,
                } => {
                    let Ok(card) = self.store.get_card(card_id) else {
                        continue;
                    };
                    if !card.state.is_running() {
                        continue;
                    }
                    if let Err(e) = self.launch(card, mode, extra, resume_session).await {
                        self.demote_stranded(card_id, &e);
                    }
                }
                QueuedRun::Review { review_id } => {
                    let reviewing = self
                        .store
                        .get_review_task(review_id)
                        .map(|t| t.status.is_running())
                        .unwrap_or(false);
                    if !reviewing {
                        continue;
                    }
                    if let Err(e) = self.begin_review_run(review_id).await {
                        // Same failure handling as `start_review`.
                        let msg = e.to_string();
                        self.review_progress(
                            review_id,
                            &format!("✗ review failed to start: {msg}"),
                        );
                        let _ = self.fail_review(review_id, msg.clone());
                        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                            Uuid::nil(),
                            Severity::Error,
                            msg,
                        ));
                    }
                }
                QueuedRun::Validation { card_id } => {
                    // `run_validation` itself backs off when the card left the
                    // gate, so a stale entry is harmless.
                    if let Err(e) = self.run_validation(card_id).await {
                        self.demote_stranded(card_id, &e);
                    }
                }
            }
        }
    }

    /// A dequeued launch failed: surface it and, if the card is still in a
    /// running state with no run behind it, demote to `Failed` so Retry/Resume
    /// can recover it. (`launch`'s own provider-failure arm already demoted —
    /// the guard makes this a no-op then.)
    fn demote_stranded(&self, card_id: Uuid, e: &CoreError) {
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            card_id,
            Severity::Error,
            e.to_string(),
        ));
        if self
            .store
            .get_card(card_id)
            .map(|c| c.state.is_running())
            .unwrap_or(false)
        {
            let _ = apply_transition(
                &self.store,
                &self.evt_tx,
                card_id,
                Transition::AgentError {
                    message: e.to_string(),
                },
            );
        }
    }

    /// Tell the UI the queue's current order (index = place in line).
    fn emit_queue_changed(&self) {
        let entries: Vec<QueuedTarget> =
            lock(&self.gate).queue.iter().map(|e| e.target()).collect();
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::run_queue_changed(entries));
    }
}
