//! The in-app diff viewer's executor handler: compute a card's committed diff
//! off the async runtime and emit it. Read-only — it never touches the card's
//! lifecycle state, its worktree, or the forge (mirrors the preview handler's
//! shape, minus the long-lived process).

use super::*;
use crate::diff::{compute_branch_diff, compute_card_diff, DiffState};
use crate::infra::git::remote_tracking_base;

impl Executor {
    /// Compute the card's committed contribution over its fork point and emit it
    /// as a `DiffUpdated` event. A card with no branch (nothing committed) emits
    /// `Empty`. Runs the git2 walk + syntect highlighting on the blocking pool,
    /// since both are synchronous and CPU-bound.
    pub(super) async fn compute_diff(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let Some(branch) = card.branch.clone() else {
            self.emit_diff(card_id, DiffState::Empty);
            return Ok(());
        };
        let project = self.store.get_project(card.project_id)?;
        let repo = project.path.clone();
        let base = project.config.effective_base_branch().to_string();

        self.emit_diff(card_id, DiffState::Computing);

        let computed =
            tokio::task::spawn_blocking(move || compute_card_diff(&repo, &base, &branch))
                .await
                .map_err(|e| CoreError::other(format!("diff task panicked: {e}")))?;

        let state = match computed {
            Ok(data) if data.files.is_empty() => DiffState::Empty,
            Ok(data) => DiffState::Ready(data),
            Err(e) => DiffState::Failed(e.to_string()),
        };
        self.emit_diff(card_id, state);
        // Return Ok even on failure: the reason is surfaced in-panel via
        // `DiffState::Failed`, so we don't also raise the dispatcher's error toast.
        Ok(())
    }

    /// Compute the diff of a PR under review, against the branch *it* targets.
    ///
    /// Unlike a card's diff, the commits may not be local yet: a task sitting in
    /// `ToReview` has never been checked out. We fetch the PR head into its
    /// stable local branch first (idempotent, and the same branch the review run
    /// later checks out), so the diff can be read before deciding to spend a
    /// review pass on the PR at all — the walk reads the object DB directly.
    ///
    /// The fetch is best-effort: from review start until publish/dismiss the
    /// local branch is checked out in the review worktree, and git refuses to
    /// fetch into a checked-out branch (same constraint `anchoring_diff`
    /// documents). Falling back to the branch as-is is honest there — the
    /// drafted comments were written against that checkout — at the cost of not
    /// reflecting a push made after the review started. It also makes the diff
    /// readable offline once the PR has been fetched once.
    ///
    /// *Both* sides have to be fetched, and the base has to resolve remote-first
    /// (see [`remote_tracking_base`]): the fork point is only where the forge
    /// puts it if the base ref is current. Diffing against a stale local base
    /// silently attributes every commit merged into it since the last pull to
    /// the contributor — a 10-line PR reads as 7000 changed lines.
    pub(super) async fn compute_review_diff(&self, review_id: Uuid) -> Result<()> {
        let task = self.store.get_review_task(review_id)?;
        let project = self.store.get_project(task.project_id)?;
        let branch = task.local_branch();
        let base = task.diff_base(project.config.effective_base_branch());

        self.emit_diff(review_id, DiffState::Computing);

        // A PR whose review we published and are now fixing ourselves: the
        // question at the gate is "what did the agent change?", not "what does
        // this PR do". Diff the fix commits alone, over the PR head the fix was
        // based on. No fetch — the branch is checked out (git would refuse) and
        // its commits are ours, not the author's. A fix run still queued has no
        // base sha yet, and falls through to the PR's own diff.
        if let Some((_, base_sha)) = fix_diff_base(&task.status) {
            let repo = project.path.clone();
            let (base_sha, branch) = (base_sha.to_string(), branch.clone());
            let computed =
                tokio::task::spawn_blocking(move || compute_branch_diff(&repo, &base_sha, &branch))
                    .await
                    .map_err(|e| CoreError::other(format!("diff task panicked: {e}")))?;
            let state = match computed {
                Ok(data) if data.files.is_empty() => DiffState::Empty,
                Ok(data) => DiffState::Ready(data),
                Err(e) => DiffState::Failed(e.to_string()),
            };
            self.emit_diff(review_id, state);
            return Ok(());
        }

        // Refresh the branch even when it exists: the author may have pushed
        // since the last fetch, and a stale diff is worse than a slow one.
        // Non-fatal: while the branch is checked out in the review worktree the
        // fetch is refused, but the branch is right there to diff — only if the
        // diff itself then fails is this surfaced as the root cause.
        let fetch_err = self
            .git
            .fetch_pr(&project.path, task.pr_number, &branch)
            .await
            .err();
        if let Some(e) = &fetch_err {
            tracing::warn!(
                "review diff: fetching PR #{} failed, diffing the local branch as-is: {e}",
                task.pr_number
            );
        }

        // Refresh the base's remote-tracking ref too. Non-fatal: a fetch can fail
        // for reasons the PR fetch above survived, and a diff against a stale
        // base still beats no diff at all — it's just wider than the PR.
        if let Err(e) = self.git.fetch(&project.path, "origin").await {
            tracing::warn!(
                "review diff: refreshing origin for #{} failed: {e}",
                task.pr_number
            );
        }

        let repo = project.path.clone();
        let computed = tokio::task::spawn_blocking(move || {
            let base = remote_tracking_base(&repo, &base);
            compute_branch_diff(&repo, &base, &branch)
        })
        .await
        .map_err(|e| CoreError::other(format!("diff task panicked: {e}")))?;

        let state = match computed {
            Ok(data) if data.files.is_empty() => DiffState::Empty,
            Ok(data) => DiffState::Ready(data),
            // A failed walk after a failed fetch means the branch never made it
            // locally — the fetch error is the root cause worth showing.
            Err(_) if fetch_err.is_some() => DiffState::Failed(format!(
                "couldn't fetch PR #{}: {}",
                task.pr_number,
                fetch_err.unwrap()
            )),
            Err(e) => DiffState::Failed(e.to_string()),
        };
        self.emit_diff(review_id, state);
        Ok(())
    }

    fn emit_diff(&self, card_id: Uuid, state: DiffState) {
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::diff_updated(card_id, state));
    }
}

/// The base sha of a task whose fix is running or waiting at the gate, once one
/// has been recorded (a run still queued has none yet).
fn fix_diff_base(status: &crate::ReviewStatus) -> Option<(&[crate::DraftComment], &str)> {
    status.fix_gate().filter(|(_, sha)| !sha.is_empty())
}
