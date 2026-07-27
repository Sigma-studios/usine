//! The pre-PR self-review pass: running a read-only review agent over the
//! committed diff and applying the fixes the user selects.

use super::*;

impl Executor {
    /// From `AwaitingReview(ReadyForReview)`: skip the self-review pass.
    /// Entering `ReadyForPr` runs the validation gate (a no-op without a
    /// validate command) — called inline rather than dispatched, so it can't be
    /// dropped by the in-flight claim this handler still holds.
    pub(super) async fn skip_review(&self, card_id: Uuid) -> Result<()> {
        self.apply(card_id, Transition::SkipReview)?;
        self.run_validation(card_id).await
    }

    /// Run a read-only self-review over the committed diff, guided by the
    /// project's `review.md` (or a default prompt). Auto-started when an
    /// implement run finalizes (see `start_self_review_direct`), and manually
    /// re-runnable from `ReadyForReview` or the fix picker. The agent emits
    /// structured verdicts parsed in `finalize_self_review`.
    ///
    /// The run is a fresh conversation whose only statement of intent is the card's
    /// description, so it's also handed the card's background (approved plan, the
    /// user's answers and change requests) — otherwise a deliberate mid-flight
    /// adjustment reads as a deviation from the spec and comes back as a finding.
    pub(super) async fn self_review(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let background = card_background(&self.store, &card);
        let guide = crate::agent::review::find_review_prompt(&project.path);
        let extra = join_sections(&[
            &background,
            &guide,
            crate::agent::review::SELF_REVIEW_INSTRUCTION,
        ]);
        let card = self.apply(card_id, Transition::StartSelfReview)?;
        self.launch(card, RunMode::Review, Some(extra), None).await
    }

    /// Create (fresh) a throwaway detached worktree at the card's branch HEAD for
    /// the read-only self-review to run in, so the pass gets a clean view of the
    /// committed diff and can't leave stray edits in the card's own worktree.
    /// Not persisted on the card — it's torn down as soon as the review finishes.
    pub(super) async fn setup_self_review_worktree(
        &self,
        card: &Card,
        project: &Project,
    ) -> Result<PathBuf> {
        let branch = card
            .branch
            .as_deref()
            .ok_or_else(|| CoreError::other("card has no branch for self-review"))?;
        let wt = self_review_worktree_path(&project.path, card.id);
        // Clear any stale worktree from a previous review pass.
        if wt.exists() {
            let _ = self.git.remove_worktree(&project.path, &wt).await;
            let _ = std::fs::remove_dir_all(&wt);
        }
        self.git
            .worktree_add_detached(&project.path, &wt, branch)
            .await?;
        Ok(wt)
    }

    /// Apply the self-review fixes the user checked — from the verdicts as the
    /// user left them (bodies may be edited), plus their free-form `note` if
    /// they typed one. With neither, just advance to the PR. Like every agent
    /// *write* run, the fix happens in the card's ISOLATED worktree — never the
    /// main working copy. Commits (no push, since there's no PR yet) and advances
    /// to `ReadyForPr`.
    pub(super) async fn apply_self_fixes(
        &self,
        card_id: Uuid,
        verdicts: Vec<FixVerdict>,
        note: String,
    ) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        if !matches!(
            &card.state,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        ) {
            return Err(CoreError::IllegalTransition(
                "can only apply self-review fixes while selecting fixes".into(),
            ));
        }
        let selected: Vec<FixVerdict> = verdicts.into_iter().filter(|v| v.selected).collect();
        let note = note.trim().to_string();
        if selected.is_empty() && note.is_empty() {
            // Nothing to fix — go straight to the PR (through the validation
            // gate, like every road to `ReadyForPr`).
            self.apply(card_id, Transition::SkipToPr)?;
            return self.run_validation(card_id).await;
        }
        // Keep the note so a later "back to start" folds it into the prompt.
        // The checked findings are only STASHED: their "Fix applied" lines go
        // on the log when the run lands its commit (see `finalize_run`) — a
        // cancelled or faulted run must not leave a durable claim it fixed
        // anything.
        if !note.is_empty() {
            self.record_qa(card_id, format!("Requested change: {note}"));
        }
        let fix_qa: Vec<String> = selected.iter().map(fixed_comment_qa).collect();
        self.store.set_pending_fix_qa(card_id, &fix_qa)?;
        let extra = fix_prompt(&selected, &note);
        // Isolate before the running-state transition, so a failure (e.g. a dirty
        // main repo) leaves the card recoverable in the fix picker.
        self.ensure_branch_worktree(card_id).await?;
        // Stash the task before entering the running state, so a retry of a
        // faulted run can restate it (see `relaunch`).
        self.store.set_fix_extra(card_id, Some(&extra))?;
        let card = self.apply(card_id, Transition::ApplySelfFixes)?;
        self.launch(card, RunMode::ApplyFixes, Some(extra), None)
            .await
    }

    /// From the self-review fix picker: skip applying fixes and open the PR.
    /// Runs the validation gate on the way (see `skip_review`).
    pub(super) async fn skip_to_pr(&self, card_id: Uuid) -> Result<()> {
        self.apply(card_id, Transition::SkipToPr)?;
        self.run_validation(card_id).await
    }
}
