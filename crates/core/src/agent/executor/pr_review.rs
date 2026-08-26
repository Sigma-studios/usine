//! Reviewing other contributors' PRs: polling for new PRs, running the
//! read-only review agent in a worktree, and publishing the drafted review.

use super::*;
use crate::diff::{compute_branch_diff, fold_unanchorable, DiffData};
use crate::infra::git::remote_tracking_base;

impl Executor {
    /// Background loop: every [`REVIEW_POLL_INTERVAL`], for each project (a) scan
    /// its configured contributors for new PRs to review and (b) refresh the
    /// reviewer-comment counts on our own open PRs. The first tick fires
    /// immediately, so state populates shortly after launch.
    pub(super) async fn review_poll_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(REVIEW_POLL_INTERVAL);
        loop {
            interval.tick().await;
            let projects = match self.store.list_projects() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("review poll: could not list projects: {e}");
                    continue;
                }
            };
            for project in projects {
                if project.config.tracks_contributor_prs() {
                    if let Err(e) = self.scan_reviews(project.id).await {
                        tracing::warn!("review scan for {} failed: {e}", project.name);
                    }
                }
                if let Err(e) = self.poll_pr_comments(&project).await {
                    tracing::warn!("PR-comment poll for {} failed: {e}", project.name);
                }
            }
        }
    }

    /// Refresh the comment counts *and* the submitted-review list on each of this
    /// project's open PRs — cards parked in `PrReview(Idle)` *and* at the merge
    /// gate (`ReadyToMerge`), where a late reviewer comment would otherwise go
    /// unseen until after the merge. Two counts land on the card (see
    /// [`comment_counts`]): the assigned reviewer's comment count, which lights
    /// the dock badge so an open PR with no feedback *from them* yet doesn't nag;
    /// and the total comment count, which surfaces the panel's triage button so a
    /// comment from any *other* reviewer can still be read and addressed. The
    /// unanswered-thread count refreshes alongside them — it gates the merge
    /// gate's "reevaluate comments" offer. The reviews come from the same tick so
    /// the panel can't badge a review it doesn't yet list. Triage stays manual;
    /// this only refreshes what the card knows. Best-effort per card: a failed
    /// fetch is logged and skipped, and a `CardUpdated` is emitted only when
    /// something actually changed.
    pub(super) async fn poll_pr_comments(&self, project: &Project) -> Result<()> {
        for card in self.store.list_cards_for_project(project.id)? {
            if !matches!(
                card.state,
                CardState::PrReview(PrReviewSub::Idle) | CardState::ReadyToMerge
            ) {
                continue;
            }
            // Owned so it outlives the refreshed `card` below. An explicit
            // no-reviewer choice stays `None` — only legacy PRs that never
            // recorded one assume the project's configured reviewer (see
            // `PrInfo::effective_reviewer`).
            let Some((pr_number, reviewer)) = card.pr.as_ref().map(|p| {
                (
                    p.number,
                    p.effective_reviewer(project.config.reviewer.as_deref())
                        .map(str::to_string),
                )
            }) else {
                continue;
            };
            let reviewer = reviewer.as_deref();
            let (comments, reviews, unanswered, checks, mergeable, live) =
                match self.fetch_review_status(&project.path, pr_number).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("PR-comment poll: refresh for #{pr_number} failed: {e}");
                        continue;
                    }
                };
            // A PR merged or closed on GitHub retires the card before the count
            // refresh and the auto-advance below — a merged PR must not ride
            // `Idle → ReadyToMerge` on the tick that should retire it. Moving
            // is idempotent: the card leaves the polled states, so the next
            // tick skips it entirely. Best-effort, like the rest of the loop.
            match self.reconcile_pr_live_state(card.id, live).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("PR-comment poll: reconcile for #{pr_number} failed: {e}");
                    continue;
                }
            }
            let (by_reviewer, total) = comment_counts(&comments, reviewer);
            // A failed thread listing keeps the previous count (see fetch_review_status);
            // a failed checks or mergeability read likewise keeps the previous value.
            let unanswered = unanswered.unwrap_or(card.unanswered_count);
            let checks = checks.unwrap_or(card.checks);
            let mergeable = mergeable.unwrap_or(card.mergeable);
            let card = if by_reviewer != card.reviewer_comment_count
                || total != card.comment_count
                || unanswered != card.unanswered_count
                || reviews != card.reviews
                || checks != card.checks
                || mergeable != card.mergeable
            {
                // Deliberately don't bump `updated_at`: a background refresh isn't
                // a user-facing edit and shouldn't reorder the board. The fetch
                // above ran unlocked, so the executor may have rewritten the card
                // meanwhile — a resolve run's clean-merge push resets `mergeable`
                // and `checks`, Back to Start clears every PR-derived cache — and
                // this tick's values pre-date that write. Blindly writing them
                // back would resurrect exactly the stale gate those paths just
                // fixed, so inside the atomic mutate: skip a card that left the
                // polled states or swapped PRs, and per field only overwrite what
                // nobody touched since the snapshot. The concurrent writer's
                // fresher knowledge wins; the next tick refetches against it.
                let mut changed = false;
                let updated = self.store.mutate_card(card.id, |c| {
                    if c.pr.as_ref().map(|p| p.number) != Some(pr_number)
                        || !matches!(
                            c.state,
                            CardState::PrReview(PrReviewSub::Idle) | CardState::ReadyToMerge
                        )
                    {
                        return Ok(());
                    }
                    if c.reviewer_comment_count == card.reviewer_comment_count {
                        changed |= c.reviewer_comment_count != by_reviewer;
                        c.reviewer_comment_count = by_reviewer;
                    }
                    if c.comment_count == card.comment_count {
                        changed |= c.comment_count != total;
                        c.comment_count = total;
                    }
                    if c.unanswered_count == card.unanswered_count {
                        changed |= c.unanswered_count != unanswered;
                        c.unanswered_count = unanswered;
                    }
                    if c.reviews == card.reviews {
                        changed |= c.reviews != reviews;
                        c.reviews = reviews;
                    }
                    if c.checks == card.checks {
                        changed |= c.checks != checks;
                        c.checks = checks;
                    }
                    if c.mergeable == card.mergeable {
                        changed |= c.mergeable != mergeable;
                        c.mergeable = mergeable;
                    }
                    Ok(())
                })?;
                if changed {
                    let _ = self
                        .evt_tx
                        .unbounded_send(ExecutorEvent::updated(updated.clone()));
                }
                updated
            } else {
                card
            };
            // Checked on every tick rather than only when the refresh above saw a
            // change: a card whose approval landed before this auto-advance
            // existed has no *new* review for the poll to notice, but is exactly
            // the card stuck at the gate. Advancing is idempotent — it leaves
            // `Idle`, so the next tick skips the card entirely.
            if card.approval_clears_merge() {
                // Best-effort, like the refresh: one card that can't advance
                // shouldn't abort the poll for the rest of the project.
                if let Err(e) = self.apply(card.id, Transition::ReviewApproved) {
                    tracing::warn!("PR-comment poll: auto-advance for #{pr_number} failed: {e}");
                    continue;
                }
                self.progress(card.id, "✔ approved with no comments — ready to merge");
            } else if card.no_reviewer_clears_merge(reviewer) {
                // No reviewer was ever assigned, so no approval will ever come:
                // this is the only way such a card reaches the merge gate. Also
                // recovers cards stranded before create_pr advanced them eagerly.
                if let Err(e) = self.apply(card.id, Transition::ReviewApproved) {
                    tracing::warn!("PR-comment poll: auto-advance for #{pr_number} failed: {e}");
                    continue;
                }
                self.progress(card.id, "✔ no reviewer assigned — ready to merge");
            }
        }
        Ok(())
    }

    /// Poll a project's configured contributors for open, not-yet-reviewed PRs and
    /// create a `ToReview` task for each PR we don't already track. Emits the
    /// project's full task list so the UI can refresh its badges and board.
    pub(super) async fn scan_reviews(&self, project_id: Uuid) -> Result<()> {
        // One scan per project at a time. `ScanReviews` is project-scoped, so
        // the dispatcher's per-card exclusivity doesn't cover it, and the
        // manual scan fired when a contributor is added lands right on top of
        // the 5-minute poll. Two overlapping scans would each decide, from a
        // task listing taken before their own `gh` round trip, that the same
        // new PR is untracked — and create a task apiece for it.
        let gate = {
            let mut locks = lock(&self.scan_locks);
            Arc::clone(locks.entry(project_id).or_default())
        };
        let _scan = gate.lock().await;
        let project = self.store.get_project(project_id)?;
        if project.config.tracks_contributor_prs() {
            let scope = if project.config.review_all_contributors {
                ReviewScope::Everyone
            } else {
                ReviewScope::Authors(project.config.review_contributors.clone())
            };
            // PRs belonging to this project's own cards are never contributor
            // reviews. `-author:@me` covers the usual case, but usine's agents
            // may push under a different GitHub account — this is what keeps
            // "everyone" mode from pulling our own work onto the review board.
            let own_prs: HashSet<u64> = self
                .store
                .list_cards_for_project(project_id)?
                .iter()
                .filter_map(|c| c.pr.as_ref().map(|p| p.number))
                .collect();
            let prs = self.forge.list_review_prs(&project.path, scope).await?;
            // Read the board *after* the round trip: the listing is a second
            // old, and a task started or dismissed meanwhile must not be
            // duplicated or resurrected.
            let existing = self.store.list_review_tasks_for_project(project_id)?;
            let dismissed = self.store.dismissed_reviews(project_id).unwrap_or_default();
            for pr in &prs {
                // Permanently dismissed by the user — never re-add it.
                if dismissed.contains(&pr.number) || own_prs.contains(&pr.number) {
                    continue;
                }
                let base = if pr.base_ref.is_empty() {
                    project.config.effective_base_branch().to_string()
                } else {
                    pr.base_ref.clone()
                };
                // Already tracked: refresh the metadata that goes stale while the
                // task sits on the board — the author retitles the PR, edits the
                // description, pushes a fix that turns CI green, or the base moves
                // and it starts conflicting. The *status* is ours, not GitHub's, so
                // it's deliberately untouched: a scan must never resurrect a
                // reviewed PR or interrupt one mid-review.
                if let Some(tracked) = existing.iter().find(|t| t.pr_number == pr.number) {
                    // A task retired as merged/closed whose PR shows up open
                    // again was reopened on GitHub — heal it back to the queue.
                    let reopened =
                        matches!(tracked.status, ReviewStatus::MergedWithoutReview { .. });
                    let changed = reopened
                        || tracked.pr_title != pr.title
                        || tracked.body != pr.body
                        || tracked.checks != pr.checks
                        || tracked.mergeable != pr.mergeable
                        || tracked.base_ref != base;
                    if changed {
                        self.store.mutate_review_task(tracked.id, |t| {
                            t.pr_title = pr.title.clone();
                            t.body = pr.body.clone();
                            t.checks = pr.checks;
                            t.mergeable = pr.mergeable;
                            t.base_ref = base.clone();
                            if reopened {
                                t.status = ReviewStatus::ToReview;
                                t.updated_at = now_millis();
                            }
                            // Otherwise not a user-facing edit — same reasoning
                            // as the comment poll, don't bump `updated_at`.
                            Ok(())
                        })?;
                    }
                    continue;
                }
                let mut task = ReviewTask::new(
                    project_id,
                    pr.number,
                    &pr.title,
                    &pr.author,
                    &pr.url,
                    &pr.head_ref,
                    base,
                );
                task.body = pr.body.clone();
                task.checks = pr.checks;
                task.mergeable = pr.mergeable;
                self.store.upsert_review_task(&task)?;
            }
            // Reconcile tracked tasks whose PR left the open listing. Absence
            // alone does NOT mean the PR closed — the search also excludes
            // drafts and `-reviewed-by:@me` — so each one is confirmed
            // individually before anything moves. `Reviewed` tasks are history
            // and stay put either way.
            let open: HashSet<u64> = prs.iter().map(|p| p.number).collect();
            for task in &existing {
                if open.contains(&task.pr_number) || task.status.is_settled() {
                    continue;
                }
                match self
                    .forge
                    .pr_live_state(&project.path, task.pr_number)
                    .await
                {
                    Ok(Some(LivePrState::Merged)) => self.retire_review_task(task, true).await?,
                    Ok(Some(LivePrState::Closed)) => self.retire_review_task(task, false).await?,
                    // Still open (just filtered out of the search), or the
                    // forge can't tell — leave the task exactly where it is.
                    Ok(_) => {}
                    // A failed read must never tear anything down.
                    Err(e) => {
                        tracing::warn!("review scan: live state of #{} failed: {e}", task.pr_number)
                    }
                }
            }
        }
        let tasks = self.store.list_review_tasks_for_project(project_id)?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_tasks_updated(project_id, tasks));
        Ok(())
    }

    /// Move a review task whose PR terminated on GitHub to the "merged without
    /// review" column: cancel any live run, reap the preview and the worktree
    /// (same ordering as `dismiss_review` — the preview's processes must die
    /// before the directory goes), but KEEP the record and skip the permanent
    /// dismissed list, so the column shows what happened and a reopened PR can
    /// heal back to `ToReview` on a later scan. Any `AwaitingValidation` drafts
    /// are discarded with the status — the PR can no longer be affected.
    async fn retire_review_task(&self, task: &ReviewTask, merged: bool) -> Result<()> {
        if let Some((_, control)) = lock(&self.review_runs).get(&task.id) {
            let _ = control.unbounded_send(RunControl::Cancel);
        }
        if let (Some(wt), Ok(project)) = (
            task.worktree_path.clone(),
            self.store.get_project(task.project_id),
        ) {
            let _ = self.stop_review_preview(task.id).await;
            let _ = self.git.remove_worktree(&project.path, &wt).await;
            let _ = std::fs::remove_dir_all(&wt);
        }
        // A task in (or faulted out of) a fix state already has its review on
        // GitHub — "merged without review" would be a lie. It settles as
        // reviewed instead; only the pending fix is lost.
        let had_fix = task.status.fix_gate().is_some();
        if had_fix {
            self.review_progress(
                task.id,
                "The PR closed on GitHub before the fix was pushed — the fix was dropped.",
            );
        }
        let updated = self.store.mutate_review_task(task.id, |t| {
            t.status = if had_fix {
                ReviewStatus::Reviewed
            } else {
                ReviewStatus::MergedWithoutReview { merged }
            };
            t.worktree_path = None;
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        Ok(())
    }

    /// Start (or retry) reviewing a PR. Moves the task to `Reviewing`, then fetches
    /// its branch into a worktree and launches the read-only review agent. Any
    /// failure along the way marks the task `Failed` (retryable) rather than
    /// leaving it stuck mid-flight.
    ///
    /// `guidance` is the user's steering for this pass. It's recorded on the task
    /// (replacing whatever the last start used, so clearing the box clears the
    /// steering) and every caller sends what it wants the run to use — which is
    /// how a retry from the board keeps the steering the panel set.
    pub(super) async fn start_review(&self, review_id: Uuid, guidance: String) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        if !matches!(
            task.status,
            ReviewStatus::ToReview | ReviewStatus::Failed { .. }
        ) {
            return Err(CoreError::IllegalTransition(
                "this PR is already being (or has been) reviewed".into(),
            ));
        }
        // A fault carried over from a *fix* run is not a review to retry: the
        // review is already on GitHub, and re-running it would rebuild the
        // worktree from the PR head, throwing away the fix commit with it.
        if task.status.fix_gate().is_some() {
            return Err(CoreError::IllegalTransition(
                "this review is already published — retry the fix, or discard it".into(),
            ));
        }
        let updated = self.store.mutate_review_task(review_id, |t| {
            t.status = ReviewStatus::Reviewing;
            t.guidance = guidance.trim().to_string();
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));

        if let Err(e) = self.begin_review_run(review_id).await {
            let msg = e.to_string();
            self.review_progress(review_id, &format!("✗ review failed to start: {msg}"));
            let _ = self.fail_review(review_id, msg.clone());
            let _ =
                self.evt_tx
                    .unbounded_send(ExecutorEvent::toast(Uuid::nil(), Severity::Error, msg));
            return Err(e);
        }
        Ok(())
    }

    /// Fetch the PR head into a worktree and launch the review agent there.
    pub(super) async fn begin_review_run(&self, review_id: Uuid) -> Result<()> {
        self.begin_review_run_admitted(review_id, None).await
    }

    /// `begin_review_run` behind the concurrency gate. `admitted` is the queue
    /// pump's pre-claimed slot (see `launch_admitted`); every other caller
    /// passes `None` and admits here.
    pub(super) async fn begin_review_run_admitted(
        &self,
        review_id: Uuid,
        admitted: Option<(Uuid, super::gate::SlotGuard)>,
    ) -> Result<()> {
        // The concurrency gate, before the (expensive) worktree prep: either
        // take a slot now or park in the queue — the task stays `Reviewing` and
        // the pump re-enters this function when a slot frees. The run id
        // doubles as the slot's admission generation (see `gate.rs`).
        let (run_id, slot) = match admitted {
            Some((run_id, guard)) => (run_id, guard),
            None => {
                let run_id = Uuid::new_v4();
                let entry = super::gate::QueuedRun::Review { review_id };
                match self.admit_or_enqueue(run_id, entry) {
                    Some(guard) => (run_id, guard),
                    None => return Ok(()),
                }
            }
        };
        // A fix run (the second half of "publish & fix") is a *write* run in the
        // PR's own checkout, so it takes a different launch path — and, once it
        // has committed something, must not rebuild the worktree at all.
        if let ReviewStatus::Fixing { base_sha, note, .. } =
            self.store.get_review_task(review_id)?.status.clone()
        {
            let wt = if base_sha.is_empty() {
                // First pass: rebuild from the live PR head. The drafts may be
                // minutes old, and committing on a stale head means a rejected
                // push. Record the sha the fix is based on as we go.
                let _ = self.stop_review_preview(review_id).await;
                let wt = self.prepare_review_worktree(review_id, true).await?;
                let sha = self.git.head_sha(&wt).await.unwrap_or_default();
                self.set_fix_base(review_id, &sha)?;
                wt
            } else {
                // Redo or retry: the checkout already carries the fix commits,
                // and `fetch_pr` force-updates the branch — it would wipe them.
                // Reuse the tree, dropping only a crashed run's uncommitted
                // half-edits.
                // Deliberately NOT `prepare_review_worktree`: it re-fetches
                // when the directory is missing, and the fetch would force the
                // local branch back to the PR head.
                let wt = self
                    .store
                    .get_review_task(review_id)?
                    .worktree_path
                    .filter(|p| p.exists())
                    .ok_or_else(|| {
                        CoreError::other(
                            "the PR's checkout is gone — discard the fix and re-review the PR",
                        )
                    })?;
                let _ = self.git.discard_changes(&wt).await;
                wt
            };
            let task = self.store.get_review_task(review_id)?;
            let project = self.store.get_project(task.project_id)?;
            return self
                .launch_review_fix(&task, &project, wt, &note, run_id, slot)
                .await;
        }

        // A review run always starts from a clean checkout: a retry must not
        // inherit whatever a previous attempt (or a preview's setup script) left
        // behind in the worktree.
        let wt = self.prepare_review_worktree(review_id, true).await?;
        let task = self.store.get_review_task(review_id)?;
        let project = self.store.get_project(task.project_id)?;
        self.launch_review(&task, &project, wt, run_id, slot).await
    }

    /// Record the PR head a fix run is based on, once the worktree is built.
    fn set_fix_base(&self, review_id: Uuid, sha: &str) -> Result<()> {
        let updated = self.store.mutate_review_task(review_id, |t| {
            if let ReviewStatus::Fixing { base_sha, .. } = &mut t.status {
                *base_sha = sha.to_string();
                t.updated_at = now_millis();
            }
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        Ok(())
    }

    /// Materialize the PR's checkout, returning its path.
    ///
    /// Shared by the review run, the preview, and the open-in-editor/terminal
    /// actions, so all four agree on where a PR lives and on the branch name it
    /// occupies. `force` rebuilds an existing checkout from scratch (what a review
    /// run wants); otherwise an intact worktree is reused, which is what makes
    /// "open this PR in my editor" fast on the second click.
    ///
    /// The resulting path is persisted on the task, so the teardown in
    /// `publish_review` / `dismiss_review` reaps it even when it was created by a
    /// preview rather than by a review run.
    pub(super) async fn prepare_review_worktree(
        &self,
        review_id: Uuid,
        force: bool,
    ) -> Result<PathBuf> {
        let task = self.store.get_review_task(review_id)?;
        let project = self.store.get_project(task.project_id)?;
        let wt = worktree_path(&project.path, review_id);
        let local_branch = task.local_branch();

        if wt.exists() {
            if !force {
                // Already checked out and intact — make sure the task knows about
                // it (it may have been built by a previous session) and reuse it.
                if task.worktree_path.as_deref() != Some(wt.as_path()) {
                    let updated = self.store.mutate_review_task(review_id, |t| {
                        t.worktree_path = Some(wt.clone());
                        t.updated_at = now_millis();
                        Ok(())
                    })?;
                    let _ = self
                        .evt_tx
                        .unbounded_send(ExecutorEvent::review_task_updated(updated));
                }
                return Ok(wt);
            }
            // Clean up any leftover worktree/dir from a previous attempt.
            let _ = self.git.remove_worktree(&project.path, &wt).await;
            let _ = std::fs::remove_dir_all(&wt);
        }

        self.review_progress(review_id, &format!("Fetching PR #{}…", task.pr_number));
        self.git
            .fetch_pr(&project.path, task.pr_number, &local_branch)
            .await?;
        // Refresh the remote-tracking refs as well, so the `origin/<base>` the
        // review agent diffs against is the branch the PR actually targets
        // today. Non-fatal: a stale base widens the review, it doesn't break it.
        if let Err(e) = self.git.fetch(&project.path, "origin").await {
            tracing::warn!("review #{}: refreshing origin failed: {e}", task.pr_number);
        }
        self.review_progress(review_id, "Checking out the PR in a worktree…");
        self.git
            .worktree_add_existing(&project.path, &local_branch, &wt)
            .await?;

        let updated = self.store.mutate_review_task(review_id, |t| {
            t.worktree_path = Some(wt.clone());
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        Ok(wt)
    }

    /// Build the review run config and spawn the read-only review agent + actor.
    pub(super) async fn launch_review(
        &self,
        task: &ReviewTask,
        project: &Project,
        wt: PathBuf,
        run_id: Uuid,
        slot: super::gate::SlotGuard,
    ) -> Result<()> {
        let project_guide = crate::agent::review::find_review_prompt(&project.path);
        let base = if task.base_ref.is_empty() {
            project.config.effective_base_branch().to_string()
        } else {
            task.base_ref.clone()
        };
        let mut prompt = format!(
            "You are reviewing pull request #{} \"{}\" by @{}. The PR targets `{base}`; read its \
             changes with `git diff origin/{base}...HEAD` (three dots — the fork point). Use \
             `origin/{base}`, never a local `{base}` branch: the local one can be behind, which \
             would show other people's already-merged commits as part of this PR.",
            task.pr_number, task.pr_title, task.author
        );
        // The user's steering goes in the run's own turn, after the PR's identity
        // and before the standing guidance: it says what matters *on this PR*,
        // which the project's review.md can't know.
        if !task.guidance.trim().is_empty() {
            prompt.push_str(&format!(
                "\n\nThe user asked you to steer this review:\n\n{}\n\nTreat that as this pass's \
                 priority — it says what they care about here. It doesn't replace the review \
                 guidance you were given, and a serious problem elsewhere in the diff is still \
                 worth a comment.",
                task.guidance.trim()
            ));
        }
        let extra = format!(
            "{project_guide}\n\n{}",
            crate::agent::review::PR_REVIEW_INSTRUCTION
        );
        // Provider/spec come from the *current* global settings, not a snapshot
        // taken at project creation — a settings change applies to the next
        // review run in any project.
        let settings = self.store.settings()?;
        let cfg = RunConfig {
            provider: settings.default_provider,
            project_dir: wt,
            spec: settings.review_spec(),
            mode: RunMode::Review,
            session_id: task.session_id,
            prompt,
            extra_prompt: Some(extra),
            resume_session: None,
            attachments: Vec::new(),
        };
        self.review_progress(task.id, "Reviewing the changes…");
        let provider = self.providers.make(settings.default_provider);
        let interactive = provider.interactive();
        let handle = provider.start(cfg).await?;
        lock(&self.review_runs).insert(task.id, (run_id, handle.control));
        tokio::spawn(run_review_actor(
            task.id,
            run_id,
            handle.events,
            ReviewRunKind::Review,
            self.self_ref.clone(),
            self.store.clone(),
            self.evt_tx.clone(),
            self.review_runs.clone(),
            interactive,
            slot,
        ));
        Ok(())
    }

    /// Anchor the selected drafts against the PR's diff, fold whatever can't be
    /// placed inline into the body, and POST the review. Returns how many
    /// comments were folded, which the caller reports back to the user.
    ///
    /// The posting half of [`Self::publish_review`], shared with
    /// [`Self::publish_review_and_fix`] so both surfaces post identically.
    async fn submit_drafts(
        &self,
        task: &ReviewTask,
        project: &Project,
        selected: Vec<DraftComment>,
        event: ReviewEvent,
        body: &str,
    ) -> Result<usize> {
        let n_selected = selected.len();
        // A draft-less publish — a direct approve from `ToReview` — has nothing
        // to anchor, and its PR may never have been fetched at all.
        let diff = if selected.is_empty() {
            None
        } else {
            self.anchoring_diff(task, project).await
        };
        let (inline, body) = fold_unanchorable(diff.as_ref(), selected, body);
        let folded = n_selected - inline.len();
        self.forge
            .submit_review(&project.path, task.pr_number, event, &body, &inline)
            .await?;
        Ok(folded)
    }

    /// Publish the checked, edited drafted comments as a single GitHub review, tear
    /// down the review worktree, and move the task to `Reviewed`.
    ///
    /// GitHub validates the review atomically: one comment anchored to a line
    /// outside the PR's diff — an agent commenting on pre-existing code does
    /// this routinely — and the whole POST is refused with a 422, losing every
    /// other comment with it. So the selected drafts are re-anchored against
    /// the PR's diff first, and whatever can't be placed inline (out-of-diff
    /// lines, unknown paths, file-level drafts) is folded into the review body
    /// — keeping the promise the diff viewer's unplaced-comments banner makes.
    pub(super) async fn publish_review(
        &self,
        review_id: Uuid,
        drafts: Vec<DraftComment>,
        event: ReviewEvent,
        body: String,
    ) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        let project = self.store.get_project(task.project_id)?;
        let selected: Vec<DraftComment> = drafts.into_iter().filter(|d| d.selected).collect();
        let folded = self
            .submit_drafts(&task, &project, selected, event, &body)
            .await?;

        if let Some(wt) = task.worktree_path.clone() {
            // A preview started from this PR holds processes (and often a Docker
            // stack) inside the worktree; removing the directory out from under it
            // is the "Directory not empty" failure mode. Stop it first.
            let _ = self.stop_review_preview(review_id).await;
            let _ = self.git.remove_worktree(&project.path, &wt).await;
            let _ = std::fs::remove_dir_all(&wt);
        }
        let updated = self.store.mutate_review_task(review_id, |t| {
            t.status = ReviewStatus::Reviewed;
            t.worktree_path = None;
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        // Say when comments rode the summary instead of the diff — the confirm
        // dialog promised N inline comments, and silence would read as loss.
        let toast = if folded == 0 {
            format!("Review published on PR #{}", task.pr_number)
        } else {
            format!(
                "Review published on PR #{} ({folded} comment(s) folded into the summary)",
                task.pr_number
            )
        };
        let _ =
            self.evt_tx
                .unbounded_send(ExecutorEvent::toast(Uuid::nil(), Severity::Success, toast));
        Ok(())
    }

    /// Publish the drafted review *and* fix it ourselves: post every selected
    /// comment with the pledge that we'll handle it, then run a write agent in
    /// the PR's own checkout to implement exactly those comments.
    ///
    /// Every fallible step happens BEFORE the post, because the pledge can't be
    /// taken back once it's on the PR: the push target is resolved first (a fork
    /// whose author disabled maintainer edits can't be fixed by us at all), and
    /// the worktree is rebuilt on the live PR head, so the fix is based on what
    /// the branch actually is today.
    ///
    /// Nothing is pushed here. The run commits in the checkout and parks at the
    /// `FixReady` gate for the user to read the diff and approve.
    pub(super) async fn publish_review_and_fix(
        &self,
        review_id: Uuid,
        drafts: Vec<DraftComment>,
        event: ReviewEvent,
        body: String,
    ) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        if !matches!(task.status, ReviewStatus::AwaitingValidation { .. }) {
            return Err(CoreError::IllegalTransition(
                "this review isn't awaiting validation".into(),
            ));
        }
        let selected: Vec<DraftComment> = drafts.into_iter().filter(|d| d.selected).collect();
        // Nothing to fix — this is just a publish. Defensive: the UI hides the
        // button with no comments checked.
        if selected.is_empty() {
            return self.publish_review(review_id, selected, event, body).await;
        }
        let project = self.store.get_project(task.project_id)?;

        // Can we keep the promise? A `None` here is "the forge can't tell" (the
        // sim, a test double), which is not a reason to refuse.
        if let Some(target) = self
            .forge
            .pr_push_target(&project.path, task.pr_number)
            .await?
        {
            if !target.pushable() {
                return Err(CoreError::other(format!(
                    "@{} hasn't allowed maintainer edits on this fork, so the fix couldn't be \
                     pushed — publish the review without fixes, or ask them to enable it",
                    task.author
                )));
            }
        }

        // The drafts may be minutes old: rebuild the checkout on the live PR
        // head so the fix commits on top of what the branch is now.
        let _ = self.stop_review_preview(review_id).await;
        let wt = self.prepare_review_worktree(review_id, true).await?;
        let base_sha = self.git.head_sha(&wt).await.unwrap_or_default();

        let n = selected.len();
        let folded = self
            .submit_drafts(
                &task,
                &project,
                crate::agent::review::pledged_drafts(&selected),
                event,
                &format!("{body}{}", crate::agent::review::FIX_PLEDGE_SUMMARY),
            )
            .await?;

        let updated = self.store.mutate_review_task(review_id, |t| {
            t.status = ReviewStatus::Fixing {
                comments: selected.clone(),
                base_sha: base_sha.clone(),
                note: String::new(),
            };
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        self.review_progress(
            review_id,
            &format!(
                "✔ review published on #{} — fixing {n} comment(s) myself",
                task.pr_number
            ),
        );
        let toast = if folded == 0 {
            format!("Review published on PR #{} — fixing it now", task.pr_number)
        } else {
            format!(
                "Review published on PR #{} ({folded} comment(s) folded into the summary) — fixing it now",
                task.pr_number
            )
        };
        let _ =
            self.evt_tx
                .unbounded_send(ExecutorEvent::toast(Uuid::nil(), Severity::Success, toast));

        // The worktree is already built, so `begin_review_run_admitted` takes
        // its reuse branch (`base_sha` is set) — through the same concurrency
        // gate, queue badge and startup recovery as every other run.
        //
        // A failure to start is wrapped as a fault on the task, never left as a
        // bare toast: the review (and its pledge) is already on GitHub, and the
        // gate's actions — redo, discard — are the only way out of it.
        if let Err(e) = self.begin_review_run(review_id).await {
            let msg = e.to_string();
            self.review_progress(review_id, &format!("✗ the fix run failed to start: {msg}"));
            let _ = self.fail_review(review_id, msg);
            return Err(e);
        }
        Ok(())
    }

    /// Build the fix run's config and spawn the write agent + actor. Mirrors
    /// [`Self::launch_review`], but as an `ApplyFixes` write run: the comments
    /// ride in the prompt verbatim (no session to resume — the review pass was a
    /// different conversation, in a checkout that has since been rebuilt).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn launch_review_fix(
        &self,
        task: &ReviewTask,
        project: &Project,
        wt: PathBuf,
        note: &str,
        run_id: Uuid,
        slot: super::gate::SlotGuard,
    ) -> Result<()> {
        let Some((comments, _)) = task.status.fix_state() else {
            return Err(CoreError::IllegalTransition(
                "this review has no fix to run".into(),
            ));
        };
        let base = task.diff_base(project.config.effective_base_branch());
        let prompt = format!(
            "You are working on pull request #{} \"{}\" by @{}, checked out on its own branch. \
             The PR targets `{base}`; its changes are the commits on this branch beyond \
             `origin/{base}` (`git diff origin/{base}...HEAD`).",
            task.pr_number, task.pr_title, task.author
        );
        let extra = format!(
            "{}\n\n{}",
            crate::agent::review::review_fix_prompt(task.pr_number, &task.author, comments, note),
            crate::agent::commit::COMMIT_MESSAGE_INSTRUCTION
        );
        let settings = self.store.settings()?;
        let cfg = RunConfig {
            provider: settings.default_provider,
            project_dir: wt,
            spec: settings.default_implement.clone(),
            mode: RunMode::ApplyFixes,
            session_id: task.session_id,
            prompt,
            extra_prompt: Some(extra),
            resume_session: None,
            attachments: Vec::new(),
        };
        self.review_progress(task.id, "Fixing the comments you took on…");
        let provider = self.providers.make(settings.default_provider);
        let interactive = provider.interactive();
        let handle = provider.start(cfg).await?;
        lock(&self.review_runs).insert(task.id, (run_id, handle.control));
        tokio::spawn(run_review_actor(
            task.id,
            run_id,
            handle.events,
            ReviewRunKind::Fix,
            self.self_ref.clone(),
            self.store.clone(),
            self.evt_tx.clone(),
            self.review_runs.clone(),
            interactive,
            slot,
        ));
        Ok(())
    }

    /// A fix run finished: commit what it wrote in the PR's checkout and park at
    /// the gate. Deliberately stops there — the push is the user's call, made
    /// against the diff (see [`Self::push_review_fix`]).
    pub(super) async fn finish_review_fix(&self, review_id: Uuid, result_text: String, cost: f64) {
        let Ok(task) = self.store.get_review_task(review_id) else {
            return;
        };
        // The task left the fixing state (dismissed, or its PR was retired
        // mid-run) — the commit would land somewhere nobody is looking.
        let ReviewStatus::Fixing {
            comments, base_sha, ..
        } = task.status.clone()
        else {
            return;
        };
        let Some(wt) = task.worktree_path.clone() else {
            let _ = self.fail_review(review_id, "the PR's checkout is gone".into());
            return;
        };

        let message = crate::agent::commit::parse_commit_message(&result_text)
            .unwrap_or_else(|| format!("fix: address review comments on #{}", task.pr_number));
        let committed = match self.git.commit_all(&wt, &message).await {
            Ok(c) => c,
            Err(e) => {
                let _ = self.fail_review(review_id, format!("committing the fix failed: {e}"));
                return;
            }
        };
        if !committed {
            // Nothing new this pass. That's a failure on the first attempt —
            // the comments were published as ours to fix and nothing was done —
            // but a legitimate outcome on a redo whose earlier pass already
            // committed (the branch has moved off `base_sha`).
            let head = self.git.head_sha(&wt).await.unwrap_or_default();
            let already_fixed = !base_sha.is_empty() && !head.is_empty() && head != base_sha;
            if !already_fixed {
                let _ = self.fail_review(
                    review_id,
                    "the fix run changed nothing — the comments are published as yours to fix, \
                     so redo it with guidance or discard the fix"
                        .into(),
                );
                return;
            }
        }

        let summary = crate::agent::commit::strip_commit_block(&result_text);
        let updated = self.store.mutate_review_task(review_id, |t| {
            t.cost += crate::Cost::from_usd(cost);
            t.status = ReviewStatus::FixReady {
                comments: comments.clone(),
                base_sha: base_sha.clone(),
                summary: summary.trim().to_string(),
            };
            t.updated_at = now_millis();
            Ok(())
        });
        match updated {
            Ok(updated) => {
                let _ = self
                    .evt_tx
                    .unbounded_send(ExecutorEvent::review_task_updated(updated));
            }
            Err(e) => {
                tracing::warn!(
                    "review fix #{}: recording the gate failed: {e}",
                    task.pr_number
                );
                return;
            }
        }
        self.review_progress(
            review_id,
            "✔ fix committed in the PR's checkout — nothing pushed yet",
        );
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            Uuid::nil(),
            Severity::Success,
            format!(
                "Fix ready for PR #{} — review the diff before it's pushed",
                task.pr_number
            ),
        ));
    }

    /// The gate's exit: push the committed fix onto the PR's own head branch,
    /// tell the author, and settle the task. Accepted from `FixReady` and from
    /// a `Failed` wrapping it, so a rejected push retries in place with the
    /// commits intact.
    pub(super) async fn push_review_fix(&self, review_id: Uuid) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        let Some((comments, _)) = task
            .status
            .fix_gate()
            .filter(|_| task.status.fix_gate_ready())
        else {
            return Err(CoreError::IllegalTransition(
                "there's no committed fix waiting to be pushed".into(),
            ));
        };
        let n = comments.len();
        let project = self.store.get_project(task.project_id)?;
        let Some(wt) = task.worktree_path.clone().filter(|p| p.exists()) else {
            return Err(CoreError::other(
                "the PR's checkout is gone — the fix can't be pushed from here",
            ));
        };

        // Re-read the target: it's one cheap call, and the author may have
        // revoked maintainer edits (or renamed the branch) since the review. A
        // failure here is fatal on purpose — guessing "same repo" would push the
        // fix onto a branch of *our* repo, which is nobody's PR.
        let target = self
            .forge
            .pr_push_target(&project.path, task.pr_number)
            .await
            .map_err(|e| {
                CoreError::other(format!(
                    "couldn't re-read where PR #{} wants its push ({e}) — the fix is still \
                     committed in the checkout, so try again",
                    task.pr_number
                ))
            })?;
        let head_ref = target
            .as_ref()
            .map(|t| t.head_ref.clone())
            .unwrap_or_else(|| task.head_ref.clone());
        let remote = match &target {
            // A fork: never fall back to `origin` — that would push someone
            // else's branch name into the maintainer's own repo.
            Some(t) if t.cross_repo => {
                if !t.pushable() {
                    return Err(CoreError::other(format!(
                        "@{} has since disabled maintainer edits on this fork — the fix can't be \
                         pushed. Discard it and reply on the PR instead.",
                        task.author
                    )));
                }
                if t.head_repo.is_empty() {
                    return Err(CoreError::other(format!(
                        "PR #{}'s fork is gone from GitHub, so there's nowhere to push the fix. \
                         Discard it and reply on the PR instead.",
                        task.pr_number
                    )));
                }
                let origin = self
                    .git
                    .remote_url(&project.path, "origin")
                    .await
                    .unwrap_or_default();
                crate::infra::git::fork_push_url(&origin, &t.head_repo)
            }
            _ => "origin".to_string(),
        };

        let refspec = format!("{}:{head_ref}", task.local_branch());
        if let Err(e) = self.git.push_refspec(&wt, &remote, &refspec).await {
            // Usually a non-fast-forward: the author pushed while the fix sat at
            // the gate. The worktree and its commits stay put, so a redo (which
            // can rebase onto the new head) or a retried push both still work.
            let msg = format!(
                "pushing the fix to `{head_ref}` failed: {e}. Push again if that was transient — \
                 the commits are still in the checkout. If the author pushed meanwhile, redoing \
                 won't help (the checkout is deliberately never re-fetched, so the fix stays on \
                 the head it was written against): discard the fix, which retracts the pledge, \
                 and take it up on the PR."
            );
            self.review_progress(review_id, &format!("✗ {msg}"));
            let _ = self.fail_review(review_id, msg.clone());
            let _ =
                self.evt_tx
                    .unbounded_send(ExecutorEvent::toast(Uuid::nil(), Severity::Error, msg));
            return Ok(());
        }

        // Best-effort: the commits are already on the branch, so a failed
        // comment is a missing courtesy, not a failed push.
        let sha = self.git.head_sha(&wt).await.unwrap_or_default();
        let note = if sha.is_empty() {
            format!("Pushed a fix — I've addressed the {n} comment(s) I said I'd handle.")
        } else {
            format!("Pushed `{sha}` — I've addressed the {n} comment(s) I said I'd handle.")
        };
        if let Err(e) = self
            .forge
            .comment_on_pr(&project.path, task.pr_number, &note)
            .await
        {
            tracing::warn!(
                "review fix #{}: the follow-up comment failed: {e}",
                task.pr_number
            );
        }

        self.teardown_review_checkout(review_id, &task, &project)
            .await;
        let updated = self.store.mutate_review_task(review_id, |t| {
            t.status = ReviewStatus::Reviewed;
            t.worktree_path = None;
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        self.review_progress(review_id, &format!("✔ fix pushed to {head_ref}"));
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            Uuid::nil(),
            Severity::Success,
            format!("Fix pushed to {head_ref} on PR #{}", task.pr_number),
        ));
        Ok(())
    }

    /// Send the fix back to the agent with feedback, keeping the commits it has
    /// already made (so the gate's diff stays cumulative over the same base).
    pub(super) async fn revise_review_fix(&self, review_id: Uuid, note: String) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        let Some((comments, base_sha)) = task.status.fix_gate() else {
            return Err(CoreError::IllegalTransition(
                "there's no fix to redo on this review".into(),
            ));
        };
        let (comments, base_sha) = (comments.to_vec(), base_sha.to_string());
        let updated = self.store.mutate_review_task(review_id, |t| {
            t.status = ReviewStatus::Fixing {
                comments: comments.clone(),
                base_sha: base_sha.clone(),
                note: note.trim().to_string(),
            };
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        self.review_progress(review_id, "Redoing the fix with your feedback…");
        if let Err(e) = self.begin_review_run(review_id).await {
            let msg = e.to_string();
            self.review_progress(review_id, &format!("✗ the fix run failed to start: {msg}"));
            let _ = self.fail_review(review_id, msg.clone());
            return Err(e);
        }
        Ok(())
    }

    /// Abandon the fix: tear the checkout down and retract the pledge on the PR,
    /// so the author isn't left waiting for a change that isn't coming.
    ///
    /// Accepted while the fix run is still going too (that's the way out offered
    /// from the card menu, where "dismiss" would drop the pledge silently *and*
    /// blacklist the PR): the run is cancelled first, because the checkout it
    /// works in is about to be removed.
    pub(super) async fn discard_review_fix(&self, review_id: Uuid) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        if task.status.fix_gate().is_none() {
            return Err(CoreError::IllegalTransition(
                "there's no fix to discard on this review".into(),
            ));
        }
        self.purge_queued(review_id);
        if let Some((_, control)) = lock(&self.review_runs).get(&review_id) {
            let _ = control.unbounded_send(RunControl::Cancel);
        }
        let project = self.store.get_project(task.project_id)?;
        self.retract_fix_pledge(&task, &project).await;
        self.teardown_review_checkout(review_id, &task, &project)
            .await;
        let updated = self.store.mutate_review_task(review_id, |t| {
            t.status = ReviewStatus::Reviewed;
            t.worktree_path = None;
            t.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::review_task_updated(updated));
        self.review_progress(
            review_id,
            "✔ fix abandoned — the pledge was retracted on the PR",
        );
        Ok(())
    }

    /// Take back the "I'll fix this myself" promise on the PR, so the author
    /// isn't left waiting for a change that isn't coming. Best-effort: the
    /// abandonment is local and already decided; a failed comment is a missing
    /// courtesy, not a reason to keep the fix alive.
    async fn retract_fix_pledge(&self, task: &ReviewTask, project: &Project) {
        if let Err(e) = self
            .forge
            .comment_on_pr(
                &project.path,
                task.pr_number,
                "On reflection I'm leaving these to you — no fix is coming from my side.",
            )
            .await
        {
            tracing::warn!(
                "review fix #{}: the retraction comment failed: {e}",
                task.pr_number
            );
        }
    }

    /// Reap a review's checkout: stop its preview first (its processes live
    /// inside the directory — pulling the directory out from under them is the
    /// "Directory not empty" failure), then remove the worktree and the dir.
    async fn teardown_review_checkout(
        &self,
        review_id: Uuid,
        task: &ReviewTask,
        project: &Project,
    ) {
        if let Some(wt) = task.worktree_path.clone() {
            let _ = self.stop_review_preview(review_id).await;
            let _ = self.git.remove_worktree(&project.path, &wt).await;
            let _ = std::fs::remove_dir_all(&wt);
        }
    }

    /// The PR's diff, recomputed for anchoring at publish time. Best-effort by
    /// design: any failure degrades to `None` — the publish then goes ahead
    /// with every line-anchored comment inline and GitHub as the judge (whose
    /// verdict the forge error now carries in full).
    ///
    /// The PR head is *not* refetched here: its local branch is checked out in
    /// the review worktree until after the publish, so a fetch into it would be
    /// refused — and the drafts were written against that checkout anyway, so
    /// it's also the honest base for anchoring them. Only the base's
    /// remote-tracking ref is refreshed (harmless, and a stale base widens the
    /// diff into keeping anchors GitHub would refuse).
    async fn anchoring_diff(&self, task: &ReviewTask, project: &Project) -> Option<DiffData> {
        let branch = task.local_branch();
        let base = task.diff_base(project.config.effective_base_branch());
        if let Err(e) = self.git.fetch(&project.path, "origin").await {
            tracing::warn!(
                "publish #{}: refreshing origin before anchoring failed: {e}",
                task.pr_number
            );
        }
        let repo = project.path.clone();
        let computed = tokio::task::spawn_blocking(move || {
            let base = remote_tracking_base(&repo, &base);
            compute_branch_diff(&repo, &base, &branch)
        })
        .await;
        match computed {
            Ok(Ok(data)) => Some(data),
            Ok(Err(e)) => {
                tracing::warn!("publish #{}: anchoring diff failed: {e}", task.pr_number);
                None
            }
            Err(e) => {
                tracing::warn!("publish #{}: anchoring diff panicked: {e}", task.pr_number);
                None
            }
        }
    }

    /// Drop a review task entirely: cancel any run, remove its worktree, delete the
    /// record, and refresh the project's list. The PR number is recorded as
    /// permanently dismissed so the poll never re-adds it.
    pub(super) async fn dismiss_review(&self, review_id: Uuid) -> Result<()> {
        self.purge_queued(review_id);
        if let Some((_, control)) = lock(&self.review_runs).get(&review_id) {
            let _ = control.unbounded_send(RunControl::Cancel);
        }
        let task = self.store.get_review_task(review_id).ok();
        if let Some(task) = &task {
            // Remember the dismissal forever so the next scan skips this PR.
            let _ = self
                .store
                .add_dismissed_review(task.project_id, task.pr_number);
            if let Ok(project) = self.store.get_project(task.project_id) {
                // Dropping a task whose review pledged a fix leaves that promise
                // standing on someone else's PR — retract it, exactly as
                // `discard_review_fix` does. (The UI steers the user to the
                // discard instead; this is the same courtesy for the dismissal
                // that gets here anyway.)
                if task.status.fix_gate().is_some() {
                    self.retract_fix_pledge(task, &project).await;
                }
                if let Some(wt) = task.worktree_path.clone() {
                    // Same ordering as `publish_review`: reap the preview's
                    // process tree before pulling the directory out from under it.
                    let _ = self.stop_review_preview(review_id).await;
                    let _ = self.git.remove_worktree(&project.path, &wt).await;
                    let _ = std::fs::remove_dir_all(&wt);
                }
            }
        }
        self.store.delete_review_task(review_id)?;
        if let Some(task) = task {
            let tasks = self.store.list_review_tasks_for_project(task.project_id)?;
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::review_tasks_updated(task.project_id, tasks));
        }
        Ok(())
    }

    /// Mark a review task `Failed` (retryable), carrying the reason.
    pub(super) fn fail_review(&self, review_id: Uuid, message: String) -> Result<()> {
        fail_review_task(&self.store, &self.evt_tx, review_id, message)
    }

    /// Emit a progress line to a review task's activity transcript (keyed by the
    /// review id, reusing the same transcript store as cards).
    pub(super) fn review_progress(&self, review_id: Uuid, line: &str) {
        transcript(&self.store, &self.evt_tx, review_id, line.to_string());
    }
}

/// Which run an actor is pumping for a review task: the read-only review pass,
/// or the write run that fixes the comments the reviewer pledged to handle. The
/// two share every event but `Done`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReviewRunKind {
    Review,
    Fix,
}

/// Pump a PR-review run: stream progress to the review transcript and, on
/// completion, either park the drafted comments for validation (a review pass)
/// or commit the fix and park it at the push gate (a fix run). Errors mark the
/// task `Failed` (retryable). Keyed by review id, not card id.
#[allow(clippy::too_many_arguments)]
async fn run_review_actor(
    review_id: Uuid,
    run_id: Uuid,
    mut events: BoxStream<'static, AgentEvent>,
    kind: ReviewRunKind,
    // The executor, for the fix run's continuation. A `Weak` upgrade, never a
    // command through `cmd_tx`: the dispatcher's per-id exclusive claim is still
    // held by the command that started this run, so a queued command would be
    // dropped (see `finalize_run`).
    executor: Weak<Executor>,
    store: Store,
    evt_tx: UnboundedSender<ExecutorEvent>,
    review_runs: RunMap,
    interactive: bool,
    // The run's concurrency slot; dropping it when the actor ends releases the
    // slot and pumps the run queue.
    _slot: super::gate::SlotGuard,
) {
    loop {
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
                    cancel_run(&review_runs, review_id, run_id);
                    AgentEvent::Error {
                        message: format!(
                            "review timed out after {} min with no output",
                            RUN_IDLE_TIMEOUT.as_secs() / 60
                        ),
                    }
                }
            }
        };
        let result = match (&evt, kind) {
            (AgentEvent::Done { .. }, ReviewRunKind::Review) => {
                finalize_review(&store, &evt_tx, review_id, evt)
            }
            (
                AgentEvent::Done {
                    result, cost_usd, ..
                },
                ReviewRunKind::Fix,
            ) => {
                if let Some(exec) = executor.upgrade() {
                    exec.finish_review_fix(review_id, result.clone(), *cost_usd)
                        .await;
                }
                Ok(())
            }
            _ => handle_review_event(&store, &evt_tx, review_id, evt),
        };
        match result {
            Ok(()) => {}
            // The task was deleted mid-run — stop quietly.
            Err(CoreError::NotFound(_)) => break,
            Err(e) => {
                let _ = evt_tx.unbounded_send(ExecutorEvent::toast(
                    Uuid::nil(),
                    Severity::Error,
                    e.to_string(),
                ));
            }
        }
    }
    let mut map = lock(&review_runs);
    if map
        .get(&review_id)
        .map(|(rid, _)| *rid == run_id)
        .unwrap_or(false)
    {
        map.remove(&review_id);
    }
}

/// Finalize a review run: record cost, parse the drafted comments + verdict, and
/// park the task in `AwaitingValidation` for the user to pick/edit and publish.
fn finalize_review(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    review_id: Uuid,
    evt: AgentEvent,
) -> Result<()> {
    let (result_text, cost) = match evt {
        AgentEvent::Done {
            result, cost_usd, ..
        } => (result, cost_usd),
        _ => return Ok(()),
    };
    // Skip if the task left the reviewing state (dismiss/supersede race).
    if !matches!(
        store.get_review_task(review_id)?.status,
        ReviewStatus::Reviewing
    ) {
        return Ok(());
    }
    let (drafts, summary, event) = crate::agent::review::parse_pr_review(&result_text);
    let n = drafts.len();
    let updated = store.mutate_review_task(review_id, |t| {
        t.cost += crate::Cost::from_usd(cost);
        t.status = ReviewStatus::AwaitingValidation {
            drafts,
            summary,
            event,
        };
        t.updated_at = now_millis();
        Ok(())
    })?;
    let _ = evt_tx.unbounded_send(ExecutorEvent::review_task_updated(updated));
    transcript(
        store,
        evt_tx,
        review_id,
        format!("✔ review: {n} comment(s) drafted"),
    );
    Ok(())
}

/// Handle a review run's non-completion events (session id, progress, errors).
fn handle_review_event(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    review_id: Uuid,
    evt: AgentEvent,
) -> Result<()> {
    match evt {
        AgentEvent::Started { session_id } => {
            let _ = store.mutate_review_task(review_id, |t| {
                if t.last_session.as_deref() != Some(session_id.as_str()) {
                    t.last_session = Some(session_id.clone());
                    t.updated_at = now_millis();
                }
                Ok(())
            });
            transcript(store, evt_tx, review_id, format!("● session {session_id}"));
        }
        AgentEvent::Progress { text } => transcript(store, evt_tx, review_id, text),
        AgentEvent::NeedsInput { .. } => {
            // A read-only review shouldn't ask for input; treat it as a failure.
            let msg = "the review agent unexpectedly asked for input".to_string();
            fail_review_task(store, evt_tx, review_id, msg.clone())?;
            let _ = evt_tx.unbounded_send(ExecutorEvent::toast(Uuid::nil(), Severity::Error, msg));
        }
        // Plan-ready is not applicable to a review run.
        AgentEvent::PlanReady { .. } => {}
        // Done is handled by finalize_review.
        AgentEvent::Done { .. } => {}
        AgentEvent::Error { message } => {
            fail_review_task(store, evt_tx, review_id, message.clone())?;
            let _ =
                evt_tx.unbounded_send(ExecutorEvent::toast(Uuid::nil(), Severity::Error, message));
        }
    }
    Ok(())
}

/// Mark a review task `Failed` (retryable), wrapping its current status as
/// `previous`. Idempotent if the task is already failed, and a no-op on a
/// settled task: a straggler `Error` from the cancelled run (or its idle-
/// timeout synthetic error) landing just after `retire_review_task` must not
/// wrap `MergedWithoutReview`/`Reviewed` into `Failed` and pull a dead PR back
/// onto a retryable column.
fn fail_review_task(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    review_id: Uuid,
    message: String,
) -> Result<()> {
    let updated = store.mutate_review_task(review_id, |t| {
        if !t.status.is_failed() && !t.status.is_settled() {
            let prev = std::mem::replace(&mut t.status, ReviewStatus::ToReview);
            t.status = ReviewStatus::Failed {
                previous: Box::new(prev),
                message: message.clone(),
            };
        }
        t.updated_at = now_millis();
        Ok(())
    })?;
    let _ = evt_tx.unbounded_send(ExecutorEvent::review_task_updated(updated));
    Ok(())
}
