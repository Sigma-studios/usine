//! Card run lifecycle: starting a card, launching provider runs, answering
//! mid-run questions, revising, retrying/relaunching, and resetting to start.

use super::actor::run_actor;
use super::*;

impl Executor {
    /// Start a card. An investigation card runs its read-only investigate phase;
    /// a task enters the design/plan phase by default, or goes straight to
    /// implementing from its description if it is marked "skip plan".
    pub(super) async fn start(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        if card.config.kind == CardKind::Investigation {
            return self.start_investigate(card_id, None).await;
        }
        if self.store.get_skip_plan(card_id).unwrap_or(false) {
            self.start_implement(card_id).await
        } else {
            self.start_run(card_id, RunMode::Plan, Transition::StartPlan, None)
                .await
        }
    }

    /// Launch a read-only investigation run: no worktree, no branch — it reads
    /// the main checkout like a plan run. Shared by the initial start (from the
    /// starting block) and the follow-up loop (from `Concluded`, with the prior
    /// rounds riding in `extra`).
    pub(super) async fn start_investigate(
        &self,
        card_id: Uuid,
        extra: Option<String>,
    ) -> Result<()> {
        // Stash this round's context so a retry of a faulted run re-launches
        // the SAME round (`relaunch` reads it back) instead of silently
        // re-answering the original description. The initial round (`None`)
        // clears any stale stash from an earlier life of the card.
        self.store
            .set_investigation_extra(card_id, extra.as_deref())?;
        self.start_run(
            card_id,
            RunMode::Investigate,
            Transition::StartInvestigate,
            extra,
        )
        .await
    }

    /// From `Concluded`: re-run the investigation with the prior conclusion, all
    /// earlier rounds, and the user's follow-up as context. Mirrors the
    /// reject-plan loop: the re-run is a fresh conversation, so without this it
    /// has no memory of what it concluded or what the user already asked.
    pub(super) async fn follow_up_investigation(
        &self,
        card_id: Uuid,
        feedback: String,
    ) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let prev = match &card.state {
            CardState::Concluded { conclusion } => Some(conclusion.clone()),
            _ => None,
        };
        // Build the extra from the qa_log BEFORE recording this round, so it
        // holds exactly the earlier rounds (same ordering as RejectPlan).
        let extra = followup_extra(prev.as_deref(), &card.qa_log, &feedback);
        self.record_qa(card_id, format!("Follow-up: {}", feedback.trim()));
        self.start_investigate(card_id, Some(extra)).await
    }

    /// From `Concluded`: convert the card, in place, into an implementation. A
    /// promotion, not a discard — unlike `back_to_start` there is no worktree,
    /// branch, PR, plan, or hand-off to tear down (investigations own none), the
    /// cost is kept (the investigation was real spend on this card's life), and
    /// the conclusion is folded into the description under a marked findings
    /// section (plain text afterward, so the user can trim it in the editor).
    pub(super) async fn convert_to_implementation(&self, card_id: Uuid) -> Result<()> {
        let mut converted = false;
        let updated = self.store.mutate_card(card_id, |c| {
            // Tolerate a double-click race: only a concluded card converts.
            let CardState::Concluded { conclusion } = c.state.clone() else {
                return Ok(());
            };
            c.description = fold_findings(&c.description, &conclusion);
            if !c.qa_log.is_empty() {
                c.description = fold_qa(&c.description, &c.qa_log);
                c.qa_log.clear();
            }
            c.config.kind = CardKind::Task;
            c.state = transition(&c.state, Transition::ResetToStart)?;
            // The next run is a fresh conversation with the findings in-prompt.
            c.last_session = None;
            c.updated_at = now_millis();
            converted = true;
            Ok(())
        })?;
        if converted {
            // The stashed round context described the investigation being
            // promoted away from; the findings now live in the description.
            let _ = self.store.set_investigation_extra(card_id, None);
            // Land on the default "design + implement" mode; the selector stays
            // editable in the starting block, including back to Investigate.
            self.store.set_skip_plan(card_id, false)?;
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::skip_plan_changed(card_id, false));
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        }
        Ok(())
    }

    /// The "no plan" path: create the worktree + branch up front (normally done
    /// at plan-approval) and launch the implement run straight from the
    /// description, with no plan to inject.
    pub(super) async fn start_implement(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        if !matches!(card.state, CardState::StartingBlock) {
            return Err(CoreError::IllegalTransition(
                "can only start implementing from the starting block".into(),
            ));
        }
        self.ensure_worktree(card_id).await?;
        let card = self.apply(card_id, Transition::StartImplement)?;
        self.launch(card, RunMode::Implement, None, None).await
    }

    pub(super) async fn start_run(
        &self,
        card_id: Uuid,
        mode: RunMode,
        t: Transition,
        extra: Option<String>,
    ) -> Result<()> {
        let card = self.apply(card_id, t)?;
        self.launch(card, mode, extra, None).await
    }

    /// Build the run config, start the provider, register its control channel,
    /// and spawn the actor that pumps its events.
    pub(super) async fn launch(
        &self,
        card: Card,
        mode: RunMode,
        extra: Option<String>,
        resume_session: Option<String>,
    ) -> Result<()> {
        let project = self.store.get_project(card.project_id)?;
        let spec = match mode {
            // Investigations run on the plan spec — the "thinking" phase's model.
            RunMode::Plan | RunMode::Investigate => card.config.plan.clone(),
            // The read-only phases share the review spec, which falls back to
            // `implement` when the card has no override.
            RunMode::Review | RunMode::Triage | RunMode::Question => card.config.review_spec(),
            RunMode::Implement | RunMode::ApplyFixes => card.config.implement.clone(),
        };
        let project_dir = match mode {
            // Read-only pre-work runs (plan, investigate) read the main checkout.
            RunMode::Plan | RunMode::Investigate => project.path.clone(),
            // Self-review is read-only, so run it in a throwaway DETACHED worktree
            // at the branch's committed HEAD. The card's own worktree is a valid
            // place too, but the detached scratch gives a clean view of exactly
            // what's committed (ignoring any stray edits) and can't leave anything
            // behind in it. Falls back to the card's worktree if it can't be created.
            RunMode::Review => self
                .setup_self_review_worktree(&card, &project)
                .await
                .unwrap_or_else(|_| {
                    card.worktree_path
                        .clone()
                        .unwrap_or_else(|| project.path.clone())
                }),
            // Triage and Q&A are read-only — running in the main tree is
            // harmless (a plan-stage question has no worktree yet).
            RunMode::Triage | RunMode::Question => card
                .worktree_path
                .clone()
                .unwrap_or_else(|| project.path.clone()),
            // Write agents (implement / all fixes) must NEVER run in the user's main
            // working tree — they'd clobber it. Callers guarantee an isolated
            // worktree via `ensure_branch_worktree`; if one is somehow missing,
            // REFUSE rather than silently fall back to `project.path`. This is the
            // hard guarantee that a fix can't mangle the working copy. (A stale
            // worktree path is fine here — the run just fails to start cleanly, and
            // `relaunch` re-establishes it on retry.)
            RunMode::Implement | RunMode::ApplyFixes => {
                let dir = card.worktree_path.clone().ok_or_else(|| {
                    CoreError::other(
                        "refusing to run a write agent in the main working tree — this card has \
                         no isolated worktree. Retry the action to set one up.",
                    )
                })?;
                if dir == project.path {
                    return Err(CoreError::other(
                        "refusing to run a write agent in the main working tree",
                    ));
                }
                dir
            }
        };
        // Always send the full task description. `--resume` adds conversation
        // continuity when the session has saved state, but claude only persists
        // at turn boundaries — a run killed mid-turn resumes with an empty
        // context, so the run must be able to stand on its own from the prompt.
        // Plan runs also get the instruction to surface decisions as structured
        // questions.
        // Point the agent at the managed attachment copies: the prompt lists
        // the paths for both providers (Claude's Read tool is vision-capable;
        // Codex reads text files with its shell tools), and the provider layer
        // additionally attaches images via `codex exec -i` for Codex.
        let mut base = card.description.clone();
        let attachments = self.store.get_attachments(card.id).unwrap_or_default();
        if !attachments.is_empty() {
            base = append_attachments(base, &attachments);
        }
        let prompt = match mode {
            RunMode::Plan => format!(
                "{}\n\n{}",
                base,
                crate::agent::plan::plan_instruction(card.config.provider)
            ),
            RunMode::Investigate => format!(
                "{}\n\n{}",
                base,
                crate::agent::investigate::investigate_instruction(card.config.provider)
            ),
            _ => base,
        };
        // Write runs author their own commit message in the repo's own style; the
        // executor uses it when committing the worktree (see `finalize_run`), so
        // commits aren't a repeated generic `usine: <title>`. When the project can
        // run (a `run_script` is set), they are also told to verify their work
        // against the app the executor brings up in this worktree (see the
        // `EnsurePreview` below) — same condition the preview itself keys on, so
        // the agent is never told to test against an app that can't exist. An
        // implement run also hands off to the human who reviews it next — a
        // recap, its open questions, and what to test — which the
        // awaiting-review panel renders. Fix runs report through their own recap
        // and don't need one.
        let extra = match mode {
            RunMode::Implement | RunMode::ApplyFixes => {
                let mut tail = String::new();
                if let Some(run) = super::preview::run_command(&project.config) {
                    tail.push_str(&crate::agent::testing::testing_instruction(&run));
                    tail.push_str("\n\n");
                }
                tail.push_str(crate::agent::commit::COMMIT_MESSAGE_INSTRUCTION);
                if mode == RunMode::Implement {
                    tail.push_str("\n\n");
                    tail.push_str(crate::agent::handoff::HANDOFF_INSTRUCTION);
                }
                Some(match extra {
                    Some(e) => format!("{e}\n\n{tail}"),
                    None => tail,
                })
            }
            _ => extra,
        };
        let is_resume = resume_session.is_some();
        let cfg = RunConfig {
            provider: card.config.provider,
            project_dir,
            spec,
            mode,
            session_id: card.session_id,
            prompt,
            extra_prompt: extra,
            resume_session,
            attachments,
        };

        let provider = self.providers.make(card.config.provider);
        let interactive = provider.interactive();
        let handle = match provider.start(cfg).await {
            Ok(handle) => handle,
            Err(e) => {
                // The caller already moved the card into a running state, but no
                // run backs it (e.g. the CLI isn't installed). Mark it Failed so
                // it's recoverable instead of stranded mid-column, then surface
                // the error.
                let demoted = apply_transition(
                    &self.store,
                    &self.evt_tx,
                    card.id,
                    Transition::AgentError {
                        message: format!("failed to start run: {e}"),
                    },
                )
                .is_ok();
                // A `Failed` park `run_actor` never sees: a mid-gate launch
                // (e.g. a validation fix run) deliberately kept the previous
                // run's preview alive, so light-stop it here.
                if demoted {
                    self.reap_idle_preview(card.id).await;
                }
                return Err(e);
            }
        };
        let run_id = Uuid::new_v4();
        lock(&self.runs).insert(card.id, (run_id, handle.control));
        // Any run that can change the work supersedes the last Agent Chat
        // exchange — drop it so the panel doesn't come back showing an answer
        // about work that no longer exists. Cleared only once the run really
        // started; the read-only runs (question/review/triage) leave it alone
        // (a question replaces it itself in `finalize_question`).
        if matches!(
            mode,
            RunMode::Plan | RunMode::Implement | RunMode::ApplyFixes
        ) {
            let _ = self.store.delete_answer(card.id);
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::answer_updated(card.id, "", ""));
        }

        tokio::spawn(run_actor(
            card.id,
            run_id,
            mode,
            handle.events,
            self.store.clone(),
            self.evt_tx.clone(),
            self.runs.clone(),
            self.git.clone(),
            is_resume,
            interactive,
            self.cmd_tx.clone(),
            self.self_ref.clone(),
        ));
        // Bring the worktree's app up alongside every write run (setup script,
        // then `run_script`, executor-owned like any preview) so the agent can
        // verify its work in the running app. It lives for the automated
        // pipeline: the finalizers light-stop it when the card parks
        // (`reap_idle_preview`), leaving the worktree's infra warm for a fast
        // manual restart. Routed through the command channel — this
        // launch usually runs inside an exclusive command, and a slow setup
        // (deps, docker) must not hold the card busy or delay the agent.
        if matches!(mode, RunMode::Implement | RunMode::ApplyFixes) {
            let _ = self
                .cmd_tx
                .unbounded_send(ExecutorCommand::EnsurePreview { card_id: card.id });
        }
        Ok(())
    }

    pub(super) async fn approve_plan(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        // Capture the plan text before transitioning away from AwaitingApproval.
        let plan = match &card.state {
            CardState::Designing(DesignSub::AwaitingApproval { plan }) => plan.clone(),
            _ => {
                return Err(CoreError::IllegalTransition(
                    "can only approve a plan that is awaiting approval".into(),
                ))
            }
        };
        // Refuse approval while the agent still has unanswered questions: the user
        // must answer them (which re-plans into a question-free plan) first. This
        // is the real invariant behind the UI's disabled "Approve" button, and it
        // also keeps the raw `usine-questions` block from leaking into the
        // implementer's plan context below.
        let (plan, questions) = crate::agent::plan::parse_plan(&plan);
        if !questions.is_empty() {
            return Err(CoreError::other(
                "This plan still has unanswered questions — answer them to refine the plan before approving.",
            ));
        }
        // Persist the plan so a later Resume can re-inject it.
        self.store.save_plan(card_id, &plan)?;

        // Create the worktree + branch before entering Implementing.
        self.ensure_worktree(card_id).await?;

        let card = self.apply(card_id, Transition::ApprovePlan)?;
        // Label the plan the same way the resume/revise/answer paths do
        // (`resume_extra` & co.), so the agent can tell where the task
        // description ends and the plan it should follow begins.
        self.launch(
            card,
            RunMode::Implement,
            Some(format!("Approved plan:\n{plan}")),
            None,
        )
        .await
    }

    /// One-time startup reconciliation: re-establish the isolated worktree for any
    /// post-implement card (review / PR / ready-to-merge) whose worktree is gone,
    /// healing a crash that removed the worktree dir. Best-effort: a card that
    /// can't be reconciled (e.g. its branch is checked out elsewhere) is left
    /// untouched — the main working copy is never modified.
    pub(super) async fn reconcile_worktrees(&self) {
        let Ok(cards) = self.store.list_cards() else {
            return;
        };
        for card in cards {
            if !matches!(
                card.state,
                CardState::AwaitingReview(_) | CardState::PrReview(_) | CardState::ReadyToMerge
            ) {
                continue;
            }
            let Some(branch) = card.branch.clone() else {
                continue;
            };
            // A live worktree already? Nothing to do (the common case).
            if card.worktree_path.as_ref().is_some_and(|p| p.exists()) {
                continue;
            }
            let Ok(project) = self.store.get_project(card.project_id) else {
                continue;
            };
            let wt = worktree_path(&project.path, card.id);
            if wt.exists() {
                let _ = self.git.remove_worktree(&project.path, &wt).await;
                let _ = std::fs::remove_dir_all(&wt);
            }
            if self
                .git
                .worktree_add_existing(&project.path, &branch, &wt)
                .await
                .is_ok()
            {
                if let Ok(updated) = self.store.mutate_card(card.id, |c| {
                    c.worktree_path = Some(wt.clone());
                    c.updated_at = now_millis();
                    Ok(())
                }) {
                    let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
                }
            }
        }
    }

    /// Create the card's isolated worktree + branch and persist them onto the
    /// card. Shared by plan-approval and the "no plan" start path.
    ///
    /// The branch is cut from `origin/<base>` (freshly fetched) when that
    /// remote-tracking ref exists, not from the local base: the local checkout
    /// can sit arbitrarily far behind the remote, and the PR this card becomes
    /// targets the remote base. Only a repo with no `origin/<base>` at all
    /// falls back to the local branch name.
    pub(super) async fn ensure_worktree(&self, card_id: Uuid) -> Result<Card> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let branch = format!("usine/{}", slug(&card.title, card.id));
        let worktree = worktree_path(&project.path, card.id);
        // Pre-clean any stale worktree/dir and lingering same-named branch from a
        // discarded attempt (e.g. a `back_to_start` whose worktree resisted
        // removal). The branch name is derived deterministically from the card, so
        // without this the `git worktree add -b <branch> <path>` below would fail
        // with "branch already exists" / "path already exists" and wedge the card
        // in the starting block. The branch is freshly cut from base here, so
        // nothing of value is discarded.
        if worktree.exists() {
            let _ = self
                .remove_worktree_retrying(&project.path, &worktree)
                .await;
            let _ = std::fs::remove_dir_all(&worktree);
        }
        let _ = self.git.delete_branch(&project.path, &branch).await;
        // Refresh origin so the cut point is the *current* remote base, not
        // wherever it sat at the last pull. Non-fatal: offline, the last-fetched
        // remote-tracking ref (or the local branch) still lets work start.
        if let Err(e) = self.git.fetch(&project.path, "origin").await {
            tracing::warn!("worktree for card {card_id}: fetching origin failed: {e}");
        }
        let base = crate::infra::git::remote_tracking_base(
            &project.path,
            project.config.effective_base_branch(),
        );
        self.git
            .create_worktree(&project.path, &branch, &worktree, &base)
            .await?;
        self.store.mutate_card(card_id, |c| {
            c.branch = Some(branch.clone());
            c.worktree_path = Some(worktree.clone());
            c.updated_at = now_millis();
            Ok(())
        })
    }

    /// Answer a pending intervention. An interactive run (the simulator) gets the
    /// answer forwarded over its live control channel and continues in place. A
    /// one-shot real run is already gone by the time the user answers, so we
    /// instead resume its session (`claude --resume` / `codex exec resume`) —
    /// or relaunch with the answer in-context (no session) — as a fresh turn.
    pub(super) async fn answer(&self, card_id: Uuid, text: String) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let pending = card.state.intervention().cloned();
        // Record the clarifying exchange for a possible later "back to start".
        if let Some(iv) = &pending {
            self.record_qa(card_id, format!("Q: {}\nA: {}", iv.question, text.trim()));
        }
        let has_live = lock(&self.runs).contains_key(&card_id);
        // Move the card back to Running.
        let card = self.apply(card_id, Transition::AnswerIntervention)?;

        if has_live {
            if let Some((_, control)) = lock(&self.runs).get(&card_id) {
                let _ = control.unbounded_send(RunControl::Answer { text });
            }
            return Ok(());
        }

        // One-shot provider: resume with the answer as the next turn.
        let mode = match &card.state {
            CardState::Designing(_) => RunMode::Plan,
            CardState::Investigating(_) => RunMode::Investigate,
            CardState::Implementing(_) => RunMode::Implement,
            _ => return Ok(()),
        };
        let resume = card.last_session.clone();
        let plan = if mode == RunMode::Implement {
            self.store.get_plan(card_id).unwrap_or(None)
        } else {
            None
        };
        let extra = answer_extra(pending.as_ref(), &text, plan.as_deref(), mode);
        self.launch(card, mode, Some(extra), resume).await
    }

    pub(super) fn cancel(&self, card_id: Uuid) -> Result<()> {
        if let Some((_, control)) = lock(&self.runs).get(&card_id) {
            let _ = control.unbounded_send(RunControl::Cancel);
        }
        // Tolerate the race where the run finished (and transitioned) a beat
        // before the cancel landed: there's simply nothing to cancel.
        match self.apply(card_id, Transition::Cancel) {
            Ok(_) | Err(CoreError::IllegalTransition(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Send a card back to the starting block for a clean re-run. Any live run is
    /// cancelled and the isolated worktree torn down so a fresh start can't
    /// collide with the existing branch; the captured clarifying Q&A and change
    /// requests are folded into the description so the re-run keeps that context.
    pub(super) async fn back_to_start(&self, card_id: Uuid) -> Result<()> {
        // Stop any active run + preview so nothing keeps mutating (or writing into)
        // the card underneath us — the preview first, so its dev server stops
        // writing into the worktree we're about to discard (else git can leave it
        // "Directory not empty").
        self.teardown_card_runtime(card_id).await;
        let card = self.store.get_card(card_id)?;
        // Best-effort teardown of the discarded attempt's git artifacts (worktree,
        // then branch — git refuses to delete a branch still checked out). If the
        // worktree resisted removal, keep the card pointing at it and its branch
        // rather than orphaning them: `ensure_worktree` pre-cleans the stale
        // worktree on the next start, and keeping the reference lets a later
        // delete/merge find it. On success we clear both below.
        let worktree_gone = match self.store.get_project(card.project_id) {
            Ok(project) => {
                self.teardown_card_worktrees(&project.path, &card, true)
                    .await
            }
            // No project to remove anything in — treat as nothing to keep.
            Err(_) => true,
        };
        // The plan describes the attempt being discarded. Dropping it with the rest
        // of the run artifacts keeps a stale plan out of the next run's prompt (a
        // re-plan saves a fresh one; a "skip plan" re-run rightly has none). The
        // hand-off recaps that same discarded attempt, so it goes too.
        let _ = self.store.delete_plan(card_id);
        // The Agent Chat answer describes the discarded attempt too.
        let _ = self.store.delete_answer(card_id);
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::answer_updated(card_id, "", ""));
        // Same for a discarded investigation's stashed round context.
        let _ = self.store.set_investigation_extra(card_id, None);
        let _ = self.store.set_handoff(card_id, &Handoff::default());
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::handoff_updated(card_id, Handoff::default()));
        let updated = self.store.mutate_card(card_id, |c| {
            if !c.qa_log.is_empty() {
                c.description = fold_qa(&c.description, &c.qa_log);
                c.qa_log.clear();
            }
            c.state = transition(&c.state, Transition::ResetToStart)?;
            // A genuine do-over: drop the run artifacts so the next Start is fresh.
            c.last_session = None;
            c.pr = None;
            c.cost = crate::Cost::ZERO;
            // Only forget the worktree/branch once they're actually gone, so a
            // worktree that resisted removal isn't leaked out of the card's memory.
            if worktree_gone {
                c.worktree_path = None;
                c.branch = None;
            }
            c.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        if !worktree_gone {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Warning,
                "Reset to the starting block, but the previous worktree couldn't be removed — \
                 it will be cleared out on the next start.",
            ));
        }
        Ok(())
    }

    /// Stop a card's live activity ahead of tearing down (or discarding) its
    /// artifacts: cancel any active agent run and reap any running preview (and its
    /// isolated infra). Shared by `back_to_start`, `mark_done`, and card deletion.
    pub(super) async fn teardown_card_runtime(&self, card_id: Uuid) {
        if let Some((_, control)) = lock(&self.runs).get(&card_id) {
            let _ = control.unbounded_send(RunControl::Cancel);
        }
        let _ = self.stop_preview(card_id).await;
    }

    /// Remove a card's git worktrees — its isolated one plus any leftover detached
    /// self-review scratch tree — and, when `delete_branch` is set and the isolated
    /// worktree is actually gone, its branch. Returns whether the isolated worktree
    /// is now removed, so callers can decide whether to keep pointing at it.
    /// Best-effort throughout; keyed off deterministic paths so it finds the
    /// scratch tree even though it's never persisted on the card.
    pub(super) async fn teardown_card_worktrees(
        &self,
        repo: &Path,
        card: &Card,
        delete_branch: bool,
    ) -> bool {
        // A self-review interrupted by a cancel/crash/quit can strand its detached
        // scratch tree; nothing references it, so clean it by its known path.
        let scratch = self_review_worktree_path(repo, card.id);
        if scratch.exists() {
            let _ = self.git.remove_worktree(repo, &scratch).await;
            let _ = std::fs::remove_dir_all(&scratch);
        }
        let mut worktree_gone = true;
        if let Some(worktree) = card.worktree_path.clone() {
            if self
                .remove_worktree_retrying(repo, &worktree)
                .await
                .is_err()
            {
                worktree_gone = false;
            }
        }
        // The branch can only be deleted once nothing has it checked out.
        if delete_branch && worktree_gone {
            if let Some(branch) = card.branch.clone() {
                let _ = self.git.delete_branch(repo, &branch).await;
            }
        }
        worktree_gone
    }

    /// Remove a card's worktree, retrying once after a short pause. Even with the
    /// preview reaped first, a dev server can have in-flight writes (or a stray
    /// watcher) that momentarily re-populate a directory git just emptied, which
    /// surfaces as "Directory not empty"; a second attempt once the writes settle
    /// clears it. Callers stop the preview first — this only covers the tail.
    pub(super) async fn remove_worktree_retrying(
        &self,
        repo: &Path,
        worktree: &Path,
    ) -> Result<()> {
        if self.git.remove_worktree(repo, worktree).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.git.remove_worktree(repo, worktree).await
    }

    /// Mark a card done (terminal) from any state. Cancels any active run and reaps
    /// any running preview; the card's branch/worktree/PR are left intact (the work
    /// may still be wanted). The preview must be stopped here: once the card is
    /// `Done` its inline preview controls disappear, so a dev server left running
    /// would have no stop path (and would keep holding ports + infra).
    pub(super) async fn mark_done(&self, card_id: Uuid) -> Result<()> {
        self.teardown_card_runtime(card_id).await;
        self.apply(card_id, Transition::MarkDone)?;
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            card_id,
            Severity::Success,
            "Marked done",
        ));
        Ok(())
    }

    /// Append a clarifying exchange to the card's restart log (best-effort; the
    /// log only feeds the optional "back to start" fold, so a failure is benign).
    pub(super) fn record_qa(&self, card_id: Uuid, entry: String) {
        let _ = self.store.mutate_card(card_id, |c| {
            c.qa_log.push(entry.clone());
            Ok(())
        });
    }

    /// Record the user's answers to the hand-off's open questions: each
    /// non-empty pair goes on the restart log (so later revise/fix/reset runs
    /// see the decision via the qa fold) and onto the stored hand-off (so the
    /// panel shows the question answered). Launches nothing.
    pub(super) fn record_handoff_answers(
        &self,
        card_id: Uuid,
        answers: Vec<(String, String)>,
    ) -> Result<()> {
        let Some(mut handoff) = self.store.get_handoff(card_id)? else {
            return Ok(());
        };
        handoff.answers.resize(handoff.questions.len(), String::new());
        let mut changed = false;
        for (question, answer) in answers {
            let answer = answer.trim();
            if answer.is_empty() {
                continue;
            }
            let Some(idx) = handoff.questions.iter().position(|q| *q == question) else {
                continue;
            };
            handoff.answers[idx] = answer.to_string();
            changed = true;
            self.record_qa(
                card_id,
                format!("Handoff question: {question}\nAnswer: {answer}"),
            );
        }
        if changed {
            self.store.set_handoff(card_id, &handoff)?;
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::handoff_updated(card_id, handoff));
        }
        Ok(())
    }

    pub(super) async fn retry(&self, card_id: Uuid) -> Result<()> {
        let card = self.apply(card_id, Transition::Retry)?;
        // Both real CLIs can resume the actual conversation via the session /
        // thread id captured at Started (`claude --resume` / `codex exec
        // resume`); with no session this stays None and the run simply
        // re-launches with full context. A resume that errors falls back to a
        // fresh run (see `run_actor`'s is_resume arm).
        let resume = card.last_session.clone();
        self.relaunch(card, resume).await
    }

    /// Re-launch the card's current (running) phase, optionally `--resume`-ing
    /// the agent session. The run always carries full context (plan + a
    /// continue-from-worktree note) so it's correct even when the killed session
    /// saved nothing. Read-only phases (plan / self-review / triage) are stateless
    /// single turns, so they re-run fresh.
    pub(super) async fn relaunch(&self, mut card: Card, resume: Option<String>) -> Result<()> {
        let card_id = card.id;
        // The validation check is a script, not an agent turn: re-run it
        // directly (the handler continues the gate at the restored attempt).
        if matches!(
            card.state,
            CardState::AwaitingReview(ReviewSub::Validating { .. })
        ) {
            return self.run_validation(card_id).await;
        }
        // An interrupted question run: the payload (question + the state it was
        // asked from) lives in `Answering`, so rebuild the exact prompt
        // `ask_question` built and re-run it fresh — read-only single turn, no
        // resume, same policy as the other read-only phases below.
        if let CardState::Answering { previous, question } = card.state.clone() {
            if matches!(
                *previous,
                CardState::PrReview(PrReviewSub::Idle) | CardState::ReadyToMerge
            ) {
                // Mirror `ask_question`: the agent must look at the PR's
                // actual branch, re-established here in case it went missing.
                self.ensure_branch_worktree(card_id).await?;
                card = self.store.get_card(card_id)?;
            }
            let stored_plan = self.store.get_plan(card_id).unwrap_or(None);
            let Some((stage, plan)) = question_context(&previous, stored_plan) else {
                return Ok(());
            };
            let extra = question_extra(stage, plan.as_deref(), &question);
            return self.launch(card, RunMode::Question, Some(extra), None).await;
        }
        let mode = match &card.state {
            CardState::Designing(_) => RunMode::Plan,
            CardState::Investigating(_) => RunMode::Investigate,
            CardState::Implementing(_) => RunMode::Implement,
            CardState::AwaitingReview(ReviewSub::Reviewing) => RunMode::Review,
            CardState::AwaitingReview(ReviewSub::ApplyingFixes) => RunMode::ApplyFixes,
            // The interrupted validation fix re-launches through the same
            // handler that started it, which rebuilds the fix prompt from the
            // output stored in the state.
            CardState::AwaitingReview(ReviewSub::FixingValidation { .. }) => {
                return self.fix_validation(card_id).await;
            }
            CardState::PrReview(PrReviewSub::FetchingComments) => RunMode::Triage,
            CardState::PrReview(PrReviewSub::ApplyingFixes) => RunMode::ApplyFixes,
            CardState::PrReview(PrReviewSub::ApplyingChange) => RunMode::ApplyFixes,
            _ => return Ok(()),
        };
        // Write runs must have an isolated worktree (the launch tripwire refuses to
        // run them in the main tree). Re-establish it before retrying/resuming in
        // case it went missing; a no-op when one already exists.
        if matches!(mode, RunMode::Implement | RunMode::ApplyFixes) {
            self.ensure_branch_worktree(card_id).await?;
            card = self.store.get_card(card_id)?;
        }
        let resume = if matches!(
            mode,
            RunMode::Plan
                | RunMode::Review
                | RunMode::Triage
                | RunMode::Investigate
                | RunMode::Question
        ) {
            None
        } else {
            resume
        };
        let extra = match mode {
            RunMode::Plan => None,
            // Re-run the round that faulted, not just the original question: a
            // follow-up round's context (prior conclusion + earlier rounds +
            // the user's ask) was stashed at launch, so a retried follow-up
            // doesn't silently overwrite it with a re-answer of round one.
            RunMode::Investigate => self.store.get_investigation_extra(card_id).unwrap_or(None),
            RunMode::Implement => {
                let plan = self.store.get_plan(card_id).unwrap_or(None);
                Some(resume_extra(plan.as_deref()))
            }
            RunMode::ApplyFixes => Some(resume_extra(None)),
            RunMode::Review => {
                let project = self.store.get_project(card.project_id)?;
                let guide = crate::agent::review::find_review_prompt(&project.path);
                Some(format!(
                    "{guide}\n\n{}",
                    crate::agent::review::SELF_REVIEW_INSTRUCTION
                ))
            }
            RunMode::Triage => {
                let comments = self.store.get_pending_comments(card_id).unwrap_or_default();
                Some(triage_prompt(&comments))
            }
            // Unreachable: an interrupted question (`Answering`) is handled by
            // the early return above, which rebuilds its extra itself.
            RunMode::Question => None,
        };
        self.launch(card, mode, extra, resume).await
    }
}
