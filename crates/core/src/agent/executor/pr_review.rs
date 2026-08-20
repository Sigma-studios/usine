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
                if !project.config.review_contributors.is_empty() {
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
            let (comments, reviews, unanswered, checks, live) =
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
            // a failed checks read likewise keeps the previous status.
            let unanswered = unanswered.unwrap_or(card.unanswered_count);
            let checks = checks.unwrap_or(card.checks);
            let card = if by_reviewer != card.reviewer_comment_count
                || total != card.comment_count
                || unanswered != card.unanswered_count
                || reviews != card.reviews
                || checks != card.checks
            {
                // Deliberately don't bump `updated_at`: a background refresh isn't
                // a user-facing edit and shouldn't reorder the board.
                let updated = self.store.mutate_card(card.id, |c| {
                    c.reviewer_comment_count = by_reviewer;
                    c.comment_count = total;
                    c.unanswered_count = unanswered;
                    c.reviews = reviews;
                    c.checks = checks;
                    Ok(())
                })?;
                let _ = self
                    .evt_tx
                    .unbounded_send(ExecutorEvent::updated(updated.clone()));
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
        let project = self.store.get_project(project_id)?;
        let existing = self.store.list_review_tasks_for_project(project_id)?;
        let dismissed = self.store.dismissed_reviews(project_id).unwrap_or_default();
        if !project.config.review_contributors.is_empty() {
            let prs = self
                .forge
                .list_review_prs(&project.path, &project.config.review_contributors)
                .await?;
            for pr in &prs {
                // Permanently dismissed by the user — never re-add it.
                if dismissed.contains(&pr.number) {
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
        let updated = self.store.mutate_review_task(task.id, |t| {
            t.status = ReviewStatus::MergedWithoutReview { merged };
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
        // The concurrency gate, before the (expensive) worktree prep: either
        // take a slot now or park in the queue — the task stays `Reviewing` and
        // the pump re-enters this function when a slot frees. The run id
        // doubles as the slot's admission generation (see `gate.rs`).
        let run_id = Uuid::new_v4();
        let Some(slot) = self.try_admit(review_id, run_id) else {
            self.enqueue_run(super::gate::QueuedRun::Review { review_id });
            return Ok(());
        };
        // A review run always starts from a clean checkout: a retry must not
        // inherit whatever a previous attempt (or a preview's setup script) left
        // behind in the worktree.
        let wt = self.prepare_review_worktree(review_id, true).await?;
        let task = self.store.get_review_task(review_id)?;
        let project = self.store.get_project(task.project_id)?;
        self.launch_review(&task, &project, wt, run_id, slot).await
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
        let cfg = RunConfig {
            provider: project.config.default_provider,
            project_dir: wt,
            spec: project.config.review_spec(),
            mode: RunMode::Review,
            session_id: task.session_id,
            prompt,
            extra_prompt: Some(extra),
            resume_session: None,
            attachments: Vec::new(),
        };
        self.review_progress(task.id, "Reviewing the changes…");
        let provider = self.providers.make(project.config.default_provider);
        let interactive = provider.interactive();
        let handle = provider.start(cfg).await?;
        lock(&self.review_runs).insert(task.id, (run_id, handle.control));
        tokio::spawn(run_review_actor(
            task.id,
            run_id,
            handle.events,
            self.store.clone(),
            self.evt_tx.clone(),
            self.review_runs.clone(),
            interactive,
            slot,
        ));
        Ok(())
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
        let n_selected = selected.len();
        // A draft-less publish — a direct approve from `ToReview` — has nothing
        // to anchor, and its PR may never have been fetched at all.
        let diff = if selected.is_empty() {
            None
        } else {
            self.anchoring_diff(&task, &project).await
        };
        let (inline, body) = fold_unanchorable(diff.as_ref(), selected, &body);
        let folded = n_selected - inline.len();
        self.forge
            .submit_review(&project.path, task.pr_number, event, &body, &inline)
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
            if let (Some(wt), Ok(project)) = (
                task.worktree_path.clone(),
                self.store.get_project(task.project_id),
            ) {
                // Same ordering as `publish_review`: reap the preview's process
                // tree before pulling the directory out from under it.
                let _ = self.stop_review_preview(review_id).await;
                let _ = self.git.remove_worktree(&project.path, &wt).await;
                let _ = std::fs::remove_dir_all(&wt);
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

/// Pump a read-only PR-review run: stream progress to the review transcript and,
/// on completion, parse the drafted comments and park the task for validation.
/// Errors mark the task `Failed` (retryable). Keyed by review id, not card id.
#[allow(clippy::too_many_arguments)]
async fn run_review_actor(
    review_id: Uuid,
    run_id: Uuid,
    mut events: BoxStream<'static, AgentEvent>,
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
        let result = match &evt {
            AgentEvent::Done { .. } => finalize_review(&store, &evt_tx, review_id, evt),
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
