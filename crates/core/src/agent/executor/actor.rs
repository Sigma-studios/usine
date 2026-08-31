//! The card run event loop: the actor task that pumps a provider's event
//! stream and the finalizers that commit/parse a completed run.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_actor(
    card_id: Uuid,
    run_id: Uuid,
    mode: RunMode,
    mut events: BoxStream<'static, AgentEvent>,
    store: Store,
    evt_tx: UnboundedSender<ExecutorEvent>,
    runs: RunMap,
    git: Arc<dyn GitOps>,
    is_resume: bool,
    interactive: bool,
    cmd_tx: UnboundedSender<ExecutorCommand>,
    executor: Weak<Executor>,
    // The run's concurrency slot (None for exempt modes). Held for the actor's
    // whole life; dropping it — however the run ends, including the mid-loop
    // NeedsInput teardown below — releases the slot and pumps the run queue.
    _slot: Option<gate::SlotGuard>,
) {
    loop {
        // One-shot runs get an idle watchdog: a hung child that never emits a
        // terminal event nor closes stdout would otherwise leak this actor + its
        // pump and park the card forever. Interactive (sim) runs idle on purpose
        // while parked on a question, so they're exempt.
        let evt = if interactive {
            match events.next().await {
                Some(evt) => evt,
                None => break,
            }
        } else {
            match tokio::time::timeout(RUN_IDLE_TIMEOUT, events.next()).await {
                Ok(Some(evt)) => evt,
                Ok(None) => break,
                Err(_) => {
                    cancel_run(&runs, card_id, run_id);
                    AgentEvent::Error {
                        message: format!(
                            "run timed out after {} min with no output",
                            RUN_IDLE_TIMEOUT.as_secs() / 60
                        ),
                    }
                }
            }
        };

        // A failed --resume run falls back to a fresh run: stay in the running
        // state (no Failed flicker), warn, and re-dispatch as RetryFresh.
        if is_resume && matches!(evt, AgentEvent::Error { .. }) {
            let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Warning,
                "Resume failed — retrying with a fresh run.",
            ));
            let _ = cmd_tx.unbounded_send(ExecutorCommand::RetryFresh { card_id });
            break;
        }

        // A mid-run question parks the card. An interactive run stays attached
        // (its answer arrives over the control channel and it continues); a
        // one-shot run can't be answered in place, so we tear it down here — the
        // user's eventual answer starts a fresh resumed run (Executor::answer).
        if let AgentEvent::NeedsInput {
            request_id,
            question,
            options,
        } = evt
        {
            let t = Transition::AgentNeedsInput(Intervention {
                request_id,
                question,
                options,
            });
            match apply_transition(&store, &evt_tx, card_id, t) {
                Ok(_) => {}
                Err(CoreError::NotFound(_)) => break,
                Err(e) => {
                    let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                        card_id,
                        Severity::Error,
                        e.to_string(),
                    ));
                }
            }
            if interactive {
                continue;
            }
            cancel_run(&runs, card_id, run_id);
            // Drop the runs-map entry now rather than at the end-of-loop
            // cleanup below: until it's gone, `Executor::answer`'s live-run
            // check would route the user's answer into this one-shot provider,
            // which ignores it — losing the answer.
            {
                let mut map = lock(&runs);
                if map
                    .get(&card_id)
                    .map(|(rid, _)| *rid == run_id)
                    .unwrap_or(false)
                {
                    map.remove(&card_id);
                }
            }
            break;
        }

        // Route a completion by run mode: code runs (implement / fix) need async
        // git and are finalized here; read-only review/triage runs parse their
        // structured verdicts; everything else (plan Done, progress, …) goes to
        // the synchronous `handle_event`.
        //
        // A `Done` in a finalized mode is terminal: the run is over and the
        // provider emits nothing after it. If *finalizing* that completion fails
        // (e.g. the post-commit transition doesn't land), the error arm below
        // must demote the card to Failed — otherwise it's stranded in its running
        // sub-state with no run behind it (plan Done is excluded: `handle_event`
        // owns its own Failed-demotion for an abandoned plan turn).
        let is_terminal_finalize =
            matches!(&evt, AgentEvent::Done { .. }) && !matches!(mode, RunMode::Plan);
        // A provider error can park the card at `Failed` (via `handle_event`'s
        // Error arm) — remembered so the reap below can key off it after `evt`
        // has been consumed.
        let was_error = matches!(&evt, AgentEvent::Error { .. });
        let result = match (&evt, mode) {
            (AgentEvent::Done { .. }, RunMode::Implement | RunMode::ApplyFixes) => {
                finalize_run(&store, &evt_tx, &git, &cmd_tx, &executor, card_id, evt).await
            }
            (AgentEvent::Done { .. }, RunMode::Review) => {
                finalize_self_review(&store, &evt_tx, &executor, card_id, evt).await
            }
            (AgentEvent::Done { .. }, RunMode::Triage) => {
                finalize_triage(&store, &evt_tx, card_id, evt)
            }
            (AgentEvent::Done { .. }, RunMode::Question) => {
                finalize_question(&store, &evt_tx, card_id, evt)
            }
            (AgentEvent::Done { .. }, RunMode::Investigate) => {
                finalize_investigation(&store, &evt_tx, card_id, evt)
            }
            // A stray PlanReady in investigate mode (an ExitPlanMode the
            // instruction failed to head off) is the agent's conclusion, not a
            // plan — `AgentPlanReady` would be an illegal move here anyway. A
            // draft under the conclusion floor (e.g. a trivial "done") is
            // ignored outright: the run continues past the rejected tool call,
            // and its final result goes through the same floor in
            // `finalize_investigation`, failing retryably if it's also trivial —
            // rather than parking the card on a useless conclusion here.
            (AgentEvent::PlanReady { .. }, RunMode::Investigate) => {
                let AgentEvent::PlanReady { plan } = evt else {
                    unreachable!()
                };
                if plan.trim().chars().count() < MIN_CONCLUSION_CHARS {
                    Ok(())
                } else {
                    conclude_investigation(&store, &evt_tx, card_id, plan)
                }
            }
            _ => handle_event(&store, &evt_tx, card_id, mode, evt),
        };
        let mut demoted = false;
        match result {
            Ok(()) => {}
            // The card was deleted mid-run — stop quietly, no error toast.
            Err(CoreError::NotFound(_)) => break,
            Err(e) => {
                let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                    card_id,
                    Severity::Error,
                    e.to_string(),
                ));
                // Finalizing a finished run failed (e.g. the commit landed but the
                // transition to AwaitingReview didn't). The run is over, so nothing
                // will re-drive the card: demote it from its now-runnerless running
                // sub-state to Failed so Retry/Resume can recover it. Guarded on
                // is_running so a card whose state already advanced is left alone
                // (AgentError is only a legal move from a running state anyway).
                if is_terminal_finalize
                    && store
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
                    demoted = true;
                }
            }
        }
        // An agent error (or the demotion above) parked the card — light-stop
        // its preview. Keyed on the *resulting* `Failed` state so a cancel the
        // sim surfaces as an Error event (the cancel handler already re-parked
        // the card elsewhere) stays a user-driven stop, out of scope here.
        if (was_error || demoted)
            && store
                .get_card(card_id)
                .map(|c| c.state.is_failed())
                .unwrap_or(false)
        {
            reap_idle_preview_direct(&executor, card_id);
        }
    }
    // The self-review runs in a throwaway detached scratch worktree that only this
    // actor knows the path to. Tear it down however the run ended (done, cancel,
    // error, timeout) so it can't leak — a no-op for every other mode.
    if matches!(mode, RunMode::Review) {
        cleanup_self_review_worktree(&store, &git, card_id).await;
    }
    // Only clear the slot if it's still ours — a newer run for this card may have
    // already replaced it.
    let mut map = lock(&runs);
    if map
        .get(&card_id)
        .map(|(rid, _)| *rid == run_id)
        .unwrap_or(false)
    {
        map.remove(&card_id);
    }
}

/// Finalize an implement/fix run: record cost, commit + push the worktree (so
/// the branch is PR-ready), then advance the card. Git failures are surfaced as
/// warnings but don't block the transition.
#[allow(clippy::too_many_arguments)]
async fn finalize_run(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    git: &Arc<dyn GitOps>,
    cmd_tx: &UnboundedSender<ExecutorCommand>,
    executor: &Weak<Executor>,
    card_id: Uuid,
    evt: AgentEvent,
) -> Result<()> {
    let (result_text, cost) = match evt {
        AgentEvent::Done {
            result, cost_usd, ..
        } => (result, cost_usd),
        _ => return Ok(()),
    };

    // Pick the next state from the card's *current* running state. This also
    // guards the cancel/supersede race: a card no longer in a code-run running
    // state lost the race (a Cancel landed just ahead of this Done), so we skip
    // committing/transitioning a run the user abandoned. Both PR-review fixes and
    // pre-PR self-review fixes are `ApplyFixes` runs but resolve differently.
    // `is_pr_fix_run` rides along because both PR sub-states resolve to
    // `AgentFixesDone`, and only the fix one can be a conflict resolution — the
    // mid-merge gate below needs to tell them apart.
    let (transition, is_pr_fix_run) = match store.get_card(card_id)?.state {
        CardState::Implementing(RunSub::Running) => (Transition::AgentImplementDone, false),
        CardState::AwaitingReview(ReviewSub::ApplyingFixes) => (Transition::SelfFixesDone, false),
        // A validation-fix run: committing routes back into the check
        // (`Validating{n+1}`), closing the gate's fix loop.
        CardState::AwaitingReview(ReviewSub::FixingValidation { .. }) => {
            (Transition::ValidationFixDone, false)
        }
        CardState::PrReview(PrReviewSub::ApplyingFixes) => (Transition::AgentFixesDone, true),
        // A reprompt on an open PR: same commit + push, but the state machine
        // routes `AgentFixesDone` back to `PrReview(Idle)` (not `ReadyToMerge`).
        CardState::PrReview(PrReviewSub::ApplyingChange) => (Transition::AgentFixesDone, false),
        _ => return Ok(()),
    };

    let card = store.mutate_card(card_id, |c| {
        c.cost += crate::Cost::from_usd(cost);
        c.updated_at = now_millis();
        Ok(())
    })?;

    // A conflict-resolution run — the only one that finds its worktree still
    // mid-merge — gets a gate before the commit, because here
    // committing *completes the merge* and pushes it to the open PR — there is
    // no later step that could take it back.
    //
    //  - It asked a question (`usine-questions`, the channel `conflict_prompt`
    //    hands it): park with the merge still in progress and nothing
    //    published, so the answering run inherits the half-resolved tree.
    //  - It left conflict markers behind: fault rather than commit them. `git
    //    add -A` would happily stage `<<<<<<<` into the PR.
    if is_pr_fix_run {
        let project = store.get_project(card.project_id)?;
        if let Some(dir) = card.worktree_path.clone().filter(|d| *d != project.path) {
            if git.merge_in_progress(&dir).await.unwrap_or(false) {
                let (clean, questions) = crate::agent::plan::parse_questions(&result_text);
                if !questions.is_empty() {
                    let recap = crate::agent::handoff::strip_handoff_block(
                        &crate::agent::commit::strip_commit_block(&clean),
                    );
                    if !recap.is_empty() {
                        let _ = store.set_review_recap(card_id, &recap);
                        let _ = evt_tx.unbounded_send(ExecutorEvent::recap_updated(card_id, recap));
                    }
                    // The stashed conflict brief and pending fix Q&A stay put:
                    // the answering run restates the brief, and the fixes it
                    // logs are only true once it commits.
                    apply_transition(
                        store,
                        evt_tx,
                        card_id,
                        Transition::AgentNeedsInput(super::conflict_intervention(&questions)),
                    )?;
                    reap_idle_preview_direct(executor, card_id);
                    return Ok(());
                }
                let unresolved = git.unresolved_conflicts(&dir).await.unwrap_or_default();
                if !unresolved.is_empty() {
                    let message = format!(
                        "The run left conflict markers in {} — nothing was committed or pushed. \
                         Retry to run it again, or cancel to abandon the merge and resolve it \
                         yourself.",
                        unresolved.join(", ")
                    );
                    apply_transition(
                        store,
                        evt_tx,
                        card_id,
                        Transition::AgentError {
                            message: message.clone(),
                        },
                    )?;
                    let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                        card_id,
                        Severity::Warning,
                        message,
                    ));
                    reap_idle_preview_direct(executor, card_id);
                    return Ok(());
                }
            }
        }
    }

    // Commit in the card's ISOLATED worktree, and ONLY there. A write run reaches
    // here via `launch`, which refuses to start without a worktree, so one is our
    // guarantee that `git commit_all` lands on this card's branch and sweeps up
    // only this card's changes. If it's somehow gone, REFUSE to commit in the
    // shared main working copy: `commit_all` (a `git add -A`) there would sweep
    // whatever the repo has drifted to — another card's checked-out branch and/or
    // its uncommitted work — into this card's commit. That is exactly the
    // cross-card contamination this isolation exists to prevent (a filter card's
    // finalize once committed a licensee-export card's tree onto its branch). The
    // branch is only *pushed* once a PR exists — before that the pre-PR branch
    // stays local for the user to review, edit, and rename.
    //
    // Did this run actually land a commit on the branch? A write run that reaches
    // review MUST have produced one; the guard below demotes it to Failed if not,
    // rather than advancing an empty branch that looks done. `None` means we
    // couldn't even attempt a commit (no branch, or the worktree was missing so
    // the contamination guard refused) — also "nothing landed".
    let project = store.get_project(card.project_id)?;
    let committed = if let Some(branch) = card.branch.clone() {
        match card.worktree_path.clone().filter(|d| *d != project.path) {
            Some(dir) => {
                // Prefer the agent's own message (authored in the repo's style).
                // Fall back to a Conventional-Commits-style default keyed to the
                // run (implement → `feat`, a fix run → `fix`) — never `usine:`.
                let kind = match &transition {
                    Transition::AgentImplementDone => "feat",
                    _ => "fix",
                };
                let message = crate::agent::commit::parse_commit_message(&result_text)
                    .unwrap_or_else(|| format!("{kind}: {}", card.title));
                match git.commit_all(&dir, &message).await {
                    Ok(true) => {
                        if card.pr.is_some() {
                            if let Err(e) = git.push(&dir, &branch).await {
                                let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                                    card_id,
                                    Severity::Warning,
                                    format!("push failed: {e}"),
                                ));
                            } else if project.expects_ci()
                                || card.checks != CheckStatus::None
                                || card.mergeable != Mergeable::Unknown
                            {
                                // The push re-triggers CI, so whatever status the
                                // card cached (say, the `Failing` a fix run set out
                                // to cure) is stale the moment it lands. Show
                                // `Pending` now instead of leaving the red layout —
                                // and its re-offered fix — up until the next poll.
                                // The cached mergeability is stale the same way (a
                                // conflict-resolve run's merge commit just cured
                                // the `Conflicting` the card shows), and needs the
                                // reset even on a no-CI repo.
                                let expects_ci = project.expects_ci();
                                if let Ok(updated) = store.mutate_card(card_id, |c| {
                                    super::pr::mark_ci_in_flight(c, expects_ci);
                                    c.mergeable = Mergeable::Unknown;
                                    Ok(())
                                }) {
                                    let _ = evt_tx.unbounded_send(ExecutorEvent::updated(updated));
                                }
                            }
                        }
                        true
                    }
                    // A clean tree — the run wrote nothing. Handled by the guard.
                    Ok(false) => false,
                    Err(e) => {
                        let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                            card_id,
                            Severity::Warning,
                            format!("commit failed: {e}"),
                        ));
                        false
                    }
                }
            }
            None => {
                let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                    card_id,
                    Severity::Error,
                    "Skipped auto-commit: this card has no isolated worktree, and committing in \
                     the shared working copy could sweep another card's changes onto this branch. \
                     Retry the action to re-establish an isolated worktree."
                        .to_string(),
                ));
                false
            }
        }
    } else {
        false
    };

    // Guard: an implement run that committed nothing onto an EMPTY branch produced
    // no work. Advancing it to review strands that empty branch — it looks done
    // but the diff is empty and any PR opened from it would be empty too. Demote
    // to Failed (a running state's only escape hatch here) with a clear reason so
    // Retry/Resume can recover it. This is the common "the agent answered in prose
    // without ever editing files" case: `git add -A` finds nothing to stage, `git
    // commit` reports nothing to commit, and without this the card would sail
    // through review to "ready for PR".
    //
    // A clean tree is not proof of that once work exists, though — demoting then
    // re-arms the same retry with the same restored context, an unwinnable loop:
    //  - An implement retry whose earlier attempt already committed finds the work
    //    finished and — exactly as `resume_extra` instructs — verifies and stops.
    //    The branch carrying commits beyond the base separates it from the empty
    //    case, so it advances.
    //  - A fix run's branch is never empty (the empty-PR disaster can't happen),
    //    and every fix transition lands at a gate that re-checks the result: the
    //    ReadyForPr diff, the re-run validation check, the PR the user merges by
    //    hand. A no-op fix run therefore advances with a warning — its recap says
    //    what the agent concluded — dropping the stashed "Fix applied" lines so
    //    it leaves no false claim on the log.
    if !committed {
        if matches!(transition, Transition::AgentImplementDone) {
            let prior_work = card.branch.is_some()
                && match card.worktree_path.clone().filter(|d| *d != project.path) {
                    Some(dir) => {
                        let base = crate::infra::git::remote_tracking_base(
                            &project.path,
                            project.config.effective_base_branch(),
                        );
                        git.branch_has_commits(&dir, &base).await.unwrap_or(false)
                    }
                    None => false,
                };
            if !prior_work {
                let message = "The run finished without changing any files, so nothing was \
                               committed to the branch. Retry to run it again."
                    .to_string();
                apply_transition(
                    store,
                    evt_tx,
                    card_id,
                    Transition::AgentError {
                        message: message.clone(),
                    },
                )?;
                let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                    card_id,
                    Severity::Warning,
                    message,
                ));
                // The card parked at `Failed` — light-stop the preview this run
                // brought up.
                reap_idle_preview_direct(executor, card_id);
                return Ok(());
            }
            let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Info,
                "The run changed no files, but the branch already carries this card's work \
                 from an earlier attempt — advancing it to review."
                    .to_string(),
            ));
        } else {
            let _ = store.take_pending_fix_qa(card_id);
            // An earlier attempt may have landed its commit and died before the
            // push (an app restart between the two) — idempotent when there is
            // nothing new to send.
            if card.pr.is_some() {
                if let (Some(branch), Some(dir)) = (
                    card.branch.clone(),
                    card.worktree_path.clone().filter(|d| *d != project.path),
                ) {
                    // Asked *before* the push, while the remote-tracking ref
                    // still describes what the forge has. `push` reports only
                    // success, and on this path it usually sends nothing at all
                    // — so without this a green card would be flipped to
                    // `Pending`, with a fresh registration grace, for a build
                    // that never starts.
                    let sends_commits = git
                        .branch_ahead_of_remote(&dir, &branch)
                        .await
                        .unwrap_or(false);
                    if let Err(e) = git.push(&dir, &branch).await {
                        let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                            card_id,
                            Severity::Warning,
                            format!("push failed: {e}"),
                        ));
                    } else if sends_commits {
                        // Commits did go out, so they re-trigger CI: mirror the
                        // committed path's reset rather than leaving this one
                        // catch-up path claiming a green the build has just
                        // invalidated.
                        let expects_ci = project.expects_ci();
                        if let Ok(updated) = store.mutate_card(card_id, |c| {
                            super::pr::mark_ci_in_flight(c, expects_ci);
                            Ok(())
                        }) {
                            let _ = evt_tx.unbounded_send(ExecutorEvent::updated(updated));
                        }
                    }
                }
            }
            let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Warning,
                "The fix run finished without changing any files — the agent judged nothing \
                 needed changing. Advancing so the next step can verify that."
                    .to_string(),
            ));
        }
    }

    // A fix run's commit is real, so the stashed "Fix applied per review
    // comment" lines are now true — put them on the restart log. Stashed (not
    // recorded) at launch by `apply_fixes` / `apply_self_fixes`, so a run that
    // never lands leaves no false claim behind; a faulted run keeps its stash
    // for a retry. Keyed off the transition so an implement run can't sweep up
    // a stale stash.
    if !matches!(transition, Transition::AgentImplementDone) {
        let fix_qa = store.take_pending_fix_qa(card_id).unwrap_or_default();
        if !fix_qa.is_empty() {
            let _ = store.mutate_card(card_id, |c| {
                c.qa_log.extend(fix_qa.iter().cloned());
                Ok(())
            });
        }
    }

    // For display, drop the machine-facing blocks the agent appended — including
    // a questions block that got this far (a fix run that asked but whose merge
    // had already been completed, say), which must never render raw as a recap.
    let summary =
        crate::agent::handoff::strip_handoff_block(&crate::agent::commit::strip_commit_block(
            &crate::agent::plan::parse_questions(&result_text).0,
        ));

    // An implement run hands off to whoever reviews it next. Store it (and emit it)
    // before the transition, so the card arrives at the awaiting-review gate with
    // its recap already in hand. A run that emitted no block clears the field
    // rather than leaving the previous attempt's recap describing this one.
    if matches!(transition, Transition::AgentImplementDone) {
        let handoff = crate::agent::handoff::parse_handoff(&result_text).unwrap_or_default();
        let _ = store.set_handoff(card_id, &handoff);
        let _ = evt_tx.unbounded_send(ExecutorEvent::handoff_updated(card_id, handoff));
    }

    // A PR fix run: keep the agent's summary as the fixes recap so the user can
    // react (e.g. if a fix was uncertain) before merging.
    let is_pr_fix = matches!(transition, Transition::AgentFixesDone);
    if is_pr_fix && !summary.is_empty() {
        let _ = store.set_review_recap(card_id, &summary);
        let _ = evt_tx.unbounded_send(ExecutorEvent::recap_updated(card_id, summary.clone()));
    }
    // Runs that land on (or head back toward) `ReadyForPr` go through the
    // validation gate: a finished self-review fix enters it, a finished
    // validation fix re-runs the check. A DIRECT call, not a re-dispatched
    // command: `ValidationFixDone` lands on `Validating` (a running state), and
    // a dispatched command can be dropped by a concurrently claimed in-flight
    // slot — which would strand the card running with no check behind it.
    let needs_validation = matches!(
        transition,
        Transition::SelfFixesDone | Transition::ValidationFixDone
    );
    let is_implement_done = matches!(transition, Transition::AgentImplementDone);
    // The fix run committed its task — drop the stashed copy so a later,
    // unrelated retry can't resurrect it (best-effort; a stale stash is
    // overwritten at the next fix launch anyway).
    if !is_implement_done {
        let _ = store.set_fix_extra(card_id, None);
    }
    apply_transition(store, evt_tx, card_id, transition)?;
    // The card just parked (`ReadyForReview`, `ReadyToMerge`, or PR-review
    // idle): light-stop the preview this run brought up. The validation routes
    // keep it — the gate continues and reaps at its own end.
    if !needs_validation {
        reap_idle_preview_direct(executor, card_id);
    }
    // The fix has committed + pushed above, so ask the executor to mark the fixed
    // comments' GitHub threads resolved. Routed back through the command channel
    // (rather than blocking here) since it's a best-effort forge call.
    if is_pr_fix {
        let _ = cmd_tx.unbounded_send(ExecutorCommand::ResolveFixedComments { card_id });
    }
    if needs_validation {
        run_validation_direct(executor, evt_tx, card_id);
    }
    // A finished implementation flows straight into the self-review pass — no
    // manual step between "implemented" and the fix picker. `ReadyForReview` is
    // now transient, but stays the parking spot for a cancelled or failed review,
    // and the resting place for cards whose per-card toggle opted out of the
    // auto-start (its manual Self-review / Skip review buttons still work).
    if is_implement_done && store.get_auto_review(card_id).unwrap_or(true) {
        start_self_review_direct(executor, evt_tx, card_id);
    }
    if !summary.is_empty() {
        transcript(store, evt_tx, card_id, format!("✔ {summary}"));
    }
    Ok(())
}

/// Finalize a read-only self-review run: record cost, parse the agent's
/// structured verdicts, and park the card for the user to pick which to apply.
/// A clean review (no findings) advances straight to `ReadyForPr`, which enters
/// the validation gate.
async fn finalize_self_review(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    executor: &Weak<Executor>,
    card_id: Uuid,
    evt: AgentEvent,
) -> Result<()> {
    let (result_text, cost) = match evt {
        AgentEvent::Done {
            result, cost_usd, ..
        } => (result, cost_usd),
        _ => return Ok(()),
    };
    let card = store.get_card(card_id)?;
    // Skip if the card left the reviewing state (cancel/supersede race).
    if !matches!(card.state, CardState::AwaitingReview(ReviewSub::Reviewing)) {
        return Ok(());
    }
    store.mutate_card(card_id, |c| {
        c.cost += crate::Cost::from_usd(cost);
        c.updated_at = now_millis();
        Ok(())
    })?;
    let verdicts = crate::agent::review::parse_self_review(&result_text);
    let n = verdicts.len();
    let card = apply_transition(
        store,
        evt_tx,
        card_id,
        Transition::SelfReviewReady { verdicts },
    )?;
    transcript(
        store,
        evt_tx,
        card_id,
        format!("✔ self-review: {n} finding(s)"),
    );
    // No findings → the card advanced to `ReadyForPr`: run the validation gate
    // (a no-op without a validate command).
    if matches!(card.state, CardState::AwaitingReview(ReviewSub::ReadyForPr)) {
        run_validation_direct(executor, evt_tx, card_id);
    }
    // Findings park the card at `SelectingFixes` with no reap: a review run
    // never dispatches `EnsurePreview`, and every route into `Reviewing` passed
    // through a park that already reaped the pipeline's preview — so anything
    // alive here was started by the user, who's about to pick fixes and wants
    // the app up.
    //
    // The throwaway detached scratch worktree is torn down when the actor exits
    // (see `run_actor`), which covers this happy path and every abnormal one.
    Ok(())
}

/// Light-stop a parked card's preview from an actor task that only holds a
/// `Weak<Executor>` (mirrors [`run_validation_direct`]). Best-effort: a dead
/// executor means shutdown, which reaps all previews itself.
pub(super) fn reap_idle_preview_direct(executor: &Weak<Executor>, card_id: Uuid) {
    if let Some(exec) = executor.upgrade() {
        tokio::spawn(async move { exec.reap_idle_preview(card_id).await });
    }
}

/// Continue the validation gate from an actor task with a direct executor call
/// (never a re-dispatched command — see `finalize_run`). An error here can
/// leave the card in a running state with nothing behind it, so it demotes to
/// `Failed` (retryable) rather than stranding.
pub(super) fn run_validation_direct(
    executor: &Weak<Executor>,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
) {
    let Some(exec) = executor.upgrade() else {
        return;
    };
    let evt_tx = evt_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = exec.run_validation(card_id).await {
            let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Error,
                e.to_string(),
            ));
            if exec
                .store
                .get_card(card_id)
                .map(|c| c.state.is_running())
                .unwrap_or(false)
            {
                let demoted = apply_transition(
                    &exec.store,
                    &evt_tx,
                    card_id,
                    Transition::AgentError {
                        message: e.to_string(),
                    },
                )
                .is_ok();
                // The demotion parked the card at `Failed` outside `run_actor`'s
                // reach — light-stop the preview the pipeline left up.
                if demoted {
                    exec.reap_idle_preview(card_id).await;
                }
            }
        }
    });
}

/// Auto-start the pre-PR self-review from the actor that just finalized an
/// implement run. A DIRECT executor call, never a re-dispatched command — a
/// command sent from here races the in-flight claim still held by whatever
/// drove the implement run and would be silently dropped (see `Executor::self_ref`).
fn start_self_review_direct(
    executor: &Weak<Executor>,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
) {
    let Some(exec) = executor.upgrade() else {
        return;
    };
    let evt_tx = evt_tx.clone();
    tokio::spawn(async move {
        // Hold the exclusive in-flight claim across the launch, exactly as a
        // dispatched `SelfReview` command would. Without it, the window between
        // the `StartSelfReview` transition and the runs-map insert (a git
        // worktree add + provider spawn) is unguarded: a Cancel/BackToStart/
        // MarkDone landing there finds no run to kill, so the state transition
        // sticks while the just-spawning review runs on as an orphan. The claim
        // makes the dispatcher drop concurrent exclusive commands and — via
        // `CardBusy` — has the UI disable the card's buttons for that window.
        let Some(_guard) = claim(&exec.in_flight, &evt_tx, card_id) else {
            // Another exclusive command owns the card right now (e.g. the user
            // hit "Back to start" just as the implementation finished) — let it
            // win; the card rests at `ReadyForReview` with the manual buttons.
            return;
        };
        match exec.self_review(card_id).await {
            Ok(()) => {}
            // The user beat the auto-start to the gate (Skip review, Back to
            // start, Mark done…) — their choice stands, nothing to report.
            Err(CoreError::IllegalTransition(_)) => {}
            // A failed launch already demoted the card to Failed (retryable);
            // an earlier failure leaves it parked at `ReadyForReview`, where the
            // manual Self-review / Skip review buttons remain. Just surface it.
            Err(e) => {
                let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                    card_id,
                    Severity::Error,
                    e.to_string(),
                ));
            }
        }
    });
}

/// Finalize a read-only PR-comment triage run: parse the agent's verdicts and
/// join them back to the stashed comments by id, then park for the user's picks.
fn finalize_triage(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    evt: AgentEvent,
) -> Result<()> {
    let (result_text, cost) = match evt {
        AgentEvent::Done {
            result, cost_usd, ..
        } => (result, cost_usd),
        _ => return Ok(()),
    };
    if !matches!(
        store.get_card(card_id)?.state,
        CardState::PrReview(PrReviewSub::FetchingComments)
    ) {
        return Ok(());
    }
    store.mutate_card(card_id, |c| {
        c.cost += crate::Cost::from_usd(cost);
        c.updated_at = now_millis();
        Ok(())
    })?;
    let comments = store.get_pending_comments(card_id).unwrap_or_default();
    let verdicts = crate::agent::review::parse_triage(&result_text, &comments);
    apply_transition(
        store,
        evt_tx,
        card_id,
        Transition::CommentsFetched { verdicts },
    )?;
    Ok(())
}

/// Finalize a read-only Agent Chat question run: record cost, persist + emit
/// the prose answer, and unwrap `Answering` back to the state the question was
/// asked from. Never touches git — a question run changes no files by
/// contract, so the no-commit guard in `finalize_run` must not see it — and
/// never touches the hand-off or the fixes recap.
fn finalize_question(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    evt: AgentEvent,
) -> Result<()> {
    let (answer, cost) = match evt {
        AgentEvent::Done {
            result, cost_usd, ..
        } => (result, cost_usd),
        _ => return Ok(()),
    };
    // Any state other than `Answering` means a cancel/supersede raced the
    // run's completion — skip quietly, the card already moved on.
    if !matches!(store.get_card(card_id)?.state, CardState::Answering { .. }) {
        return Ok(());
    }
    let question = store
        .get_question(card_id)
        .unwrap_or(None)
        .unwrap_or_default();
    store.mutate_card(card_id, |c| {
        c.cost += crate::Cost::from_usd(cost);
        c.updated_at = now_millis();
        // Only the *answered* exchange goes on the restart log — a bare
        // question folded into a later prompt would read as a standing
        // directive. Same "Q:/A:" shape the intervention answers use.
        if !question.is_empty() && !answer.is_empty() {
            c.qa_log.push(format!("Q: {question}\nA: {answer}"));
        }
        Ok(())
    })?;
    // An empty result is a run that produced no prose, not an exchange: record
    // nothing, and clear the stashed question so the next answer can't inherit
    // it. Same guard the `qa_log` push and the transcript line use.
    if answer.is_empty() {
        let _ = store.set_question(card_id, "");
    } else {
        let _ = store.set_answer(card_id, &answer);
    }
    if let Ok(answers) = store.get_answers(card_id) {
        let _ = evt_tx.unbounded_send(ExecutorEvent::answers_updated(card_id, answers));
    }
    apply_transition(store, evt_tx, card_id, Transition::QuestionAnswered)?;
    if !answer.is_empty() {
        transcript(store, evt_tx, card_id, format!("✔ {answer}"));
    }
    Ok(())
}

/// The floor under an investigation conclusion: anything shorter is the agent
/// bailing mid-thought on a dangling status line, not a verdict. Much lower
/// than a plan's, since a one-sentence verdict is a legitimate conclusion.
/// Shared by the run's final result and a stray in-run ExitPlanMode draft.
const MIN_CONCLUSION_CHARS: usize = 40;

/// Park an investigation on its conclusion, with the transcript line the
/// concluded panel's history keys off. Tolerates losing a race (a cancel landed
/// first, or a stray PlanReady already concluded the card).
fn conclude_investigation(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    conclusion: String,
) -> Result<()> {
    match apply_transition(
        store,
        evt_tx,
        card_id,
        Transition::AgentConcluded { conclusion },
    ) {
        Ok(_) => {
            transcript(store, evt_tx, card_id, "✔ investigation concluded".into());
            Ok(())
        }
        Err(CoreError::IllegalTransition(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Finalize a read-only investigation run: record cost and park the card on its
/// conclusion. A result under [`MIN_CONCLUSION_CHARS`] fails the run (retryable)
/// instead.
pub(super) fn finalize_investigation(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    evt: AgentEvent,
) -> Result<()> {
    let (result_text, cost) = match evt {
        AgentEvent::Done {
            result, cost_usd, ..
        } => (result, cost_usd),
        _ => return Ok(()),
    };
    let card = store.get_card(card_id)?;
    // Already concluded: a stray PlanReady carried the conclusion (see
    // `run_actor`); just record this run's cost. Any other state lost a
    // cancel/supersede race — skip entirely.
    let already_concluded = matches!(card.state, CardState::Concluded { .. });
    if !matches!(card.state, CardState::Investigating(_)) && !already_concluded {
        return Ok(());
    }
    store.mutate_card(card_id, |c| {
        c.cost += crate::Cost::from_usd(cost);
        c.updated_at = now_millis();
        Ok(())
    })?;
    if already_concluded {
        return Ok(());
    }
    if result_text.trim().chars().count() < MIN_CONCLUSION_CHARS {
        let message = "The investigation ended without producing a conclusion — the agent \
                       stopped mid-turn. Retry to investigate again."
            .to_string();
        apply_transition(
            store,
            evt_tx,
            card_id,
            Transition::AgentError {
                message: message.clone(),
            },
        )?;
        let _ = evt_tx.unbounded_send(ExecutorEvent::toast(card_id, Severity::Warning, message));
        return Ok(());
    }
    conclude_investigation(store, evt_tx, card_id, result_text)
}

/// Handle the non-finalizing events of a run. `NeedsInput` (parking) and
/// implement/fix `Done` (which need git side effects) are handled in `run_actor`
/// and `finalize_run` respectively, so the only `Done` that reaches here is a
/// plan-mode completion.
pub(super) fn handle_event(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    mode: RunMode,
    evt: AgentEvent,
) -> Result<()> {
    match evt {
        AgentEvent::Started { session_id } => {
            // Remember the provider session so a later Resume can --resume it —
            // but only for the modes a retry actually resumes (see `relaunch`).
            // The read-only side runs (question/review/triage) must not claim
            // `last_session`: they are stateless single turns that a retry
            // re-runs fresh, and a stale claim here would leak their read-only
            // conversation into a later write run's --resume.
            if matches!(
                mode,
                RunMode::Plan | RunMode::Implement | RunMode::ApplyFixes
            ) {
                let _ = store.mutate_card(card_id, |c| {
                    if c.last_session.as_deref() != Some(session_id.as_str()) {
                        c.last_session = Some(session_id.clone());
                        c.updated_at = now_millis();
                    }
                    Ok(())
                });
            }
            transcript(store, evt_tx, card_id, format!("● session {session_id}"));
        }
        AgentEvent::Progress { text } => {
            transcript(store, evt_tx, card_id, text);
        }
        AgentEvent::PlanReady { plan } => {
            apply_transition(store, evt_tx, card_id, Transition::AgentPlanReady { plan })?;
        }
        AgentEvent::Done {
            result, cost_usd, ..
        } => {
            // Only plan-mode completions reach here. Record cost first.
            store.mutate_card(card_id, |c| {
                c.cost += crate::Cost::from_usd(cost_usd);
                c.updated_at = now_millis();
                Ok(())
            })?;
            // Reaching here still in Designing(Running) is the *normal* ending
            // for a plan run: the headless CLI does not expose ExitPlanMode (it
            // is absent from the tool list and from ToolSearch's deferred
            // registry), so the agent writes its plan out as the run's final
            // result and no PlanReady ever fires. Promote that result. Only a
            // result too short to be a plan is treated as an abandoned turn —
            // the agent bailing mid-thought on a status line like "I'll wait for
            // the findings", which a one-shot run can never follow up on — and
            // that stays retryable rather than becoming an approvable plan.
            // (A run that *did* emit PlanReady already left Designing(Running),
            // so it skips this branch entirely.)
            const MIN_PLAN_CHARS: usize = 200;
            let fresh = store.get_card(card_id)?;
            if matches!(fresh.state, CardState::Designing(DesignSub::Running)) {
                let (_, questions) = crate::agent::plan::parse_plan(&result);
                let trimmed = result.trim();
                if questions.is_empty() && trimmed.chars().count() < MIN_PLAN_CHARS {
                    let message = "The plan phase ended without producing a plan — the agent \
                                   stopped mid-turn. Retry to re-plan."
                        .to_string();
                    apply_transition(
                        store,
                        evt_tx,
                        card_id,
                        Transition::AgentError {
                            message: message.clone(),
                        },
                    )?;
                    let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                        card_id,
                        Severity::Warning,
                        message,
                    ));
                } else {
                    apply_transition(
                        store,
                        evt_tx,
                        card_id,
                        Transition::AgentPlanReady { plan: result },
                    )?;
                }
            }
        }
        // NeedsInput is intercepted in run_actor; nothing to do here.
        AgentEvent::NeedsInput { .. } => {}
        AgentEvent::Error { message } => {
            apply_transition(
                store,
                evt_tx,
                card_id,
                Transition::AgentError {
                    message: message.clone(),
                },
            )?;
            let _ = evt_tx.unbounded_send(ExecutorEvent::toast(card_id, Severity::Error, message));
        }
    }
    Ok(())
}
