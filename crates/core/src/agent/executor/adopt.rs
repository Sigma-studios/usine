//! Branch adoption: turning an existing hand-made branch into a first-class
//! card that enters the pipeline at the self-review pass.
//!
//! The card gets its own `usine/<slug>` branch cut at the adopted tip, so the
//! original branch stays exactly what it was and every downstream mechanism
//! (self-review, fixes, validation, PR, teardown, back-to-start, delete) works
//! unchanged — no "adopted" flag, no special cases.
//!
//! Deliberately out of scope (v1):
//! - Stacked branches: the diff and the eventual PR always target the
//!   project's base branch, even if the adopted branch forked from another.
//! - Adopting a branch that already has an open PR into `PrReview`: the probe
//!   warns about the PR, and proceeding eventually opens a second one.

use std::collections::HashSet;

use crate::agent::events::{AdoptProbe, DirtyAction};
use crate::infra::git::{checkout_of_branch, commitish_exists, is_dirty, log_subjects};

use super::*;

impl Executor {
    /// List the project's branches a card could adopt (the dialog's picker).
    pub(super) async fn list_adopt_sources(&self, project_id: Uuid) -> Result<()> {
        let project = self.store.get_project(project_id)?;
        let all = self.git.list_all_branches(&project.path).await?;
        let refs = adopt_source_refs(
            &all,
            project.config.effective_base_branch(),
            &self.card_branches(project_id),
        );
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::adopt_sources(project_id, refs));
        Ok(())
    }

    /// Inspect one candidate branch for the adopt dialog. Only the refusal is
    /// authoritative; everything else degrades per-field (forge offline → no
    /// PR info) rather than erroring — the dialog wants warnings, not walls.
    pub(super) async fn probe_adopt_source(
        &self,
        project_id: Uuid,
        source_ref: String,
    ) -> Result<()> {
        let project = self.store.get_project(project_id)?;
        let refusal = self.adopt_refusal(&project, &source_ref);
        let mut subjects = Vec::new();
        let mut dirty_checkout = None;
        let mut open_pr = None;
        if refusal.is_none() {
            let base = project.config.effective_base_branch();
            subjects = log_subjects(&project.path, base, &source_ref).unwrap_or_default();
            let local = local_name(&source_ref);
            if let Some(path) = checkout_of_branch(&project.path, local) {
                if is_dirty(&path).await.unwrap_or(false) {
                    dirty_checkout = Some(path);
                }
            }
            open_pr = self
                .forge
                .pr_for_head(&project.path, local)
                .await
                .unwrap_or(None);
        }
        let probe = AdoptProbe {
            source_ref,
            refusal,
            commits_ahead: subjects.len(),
            subjects,
            dirty_checkout,
            open_pr,
        };
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::adopt_probe(project_id, probe));
        Ok(())
    }

    /// Adopt `source_ref` into a new card and drop it straight into the
    /// self-review pipeline. Validates everything before creating anything, so
    /// a refusal leaves no card and no git artifacts behind.
    pub(super) async fn adopt_branch(
        &self,
        project_id: Uuid,
        source_ref: String,
        title: String,
        description: String,
        retire_original: bool,
        dirty_action: DirtyAction,
    ) -> Result<()> {
        let project = self.store.get_project(project_id)?;
        // Refresh origin first so a remote-only ref (or a stale remote-tracking
        // one) is adopted at its current tip. Non-fatal, as in `ensure_worktree`.
        if let Err(e) = self.git.fetch(&project.path, "origin").await {
            tracing::warn!("adopt {source_ref}: fetching origin failed: {e}");
        }
        if let Some(refusal) = self.adopt_refusal(&project, &source_ref) {
            return Err(CoreError::other(format!("cannot adopt: {refusal}")));
        }
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err(CoreError::other("a title is required to adopt a branch"));
        }
        if description.trim().is_empty() {
            return Err(CoreError::other(
                "a task description is required to adopt a branch — it's what the \
                 review and fix agents read as the statement of intent",
            ));
        }
        let base = project.config.effective_base_branch();
        let local = local_name(&source_ref).to_string();
        let local_exists = self
            .git
            .local_branches(&project.path)
            .await
            .unwrap_or_default()
            .contains(&local);
        // Adopt the local tip when the branch exists locally (it may be ahead
        // of — or not on — the remote); a remote-only pick adopts the
        // remote-tracking tip.
        let cut_point = if local_exists {
            local.clone()
        } else {
            format!("origin/{local}")
        };
        let dirty_checkout = match checkout_of_branch(&project.path, &local) {
            Some(path) if is_dirty(&path).await.unwrap_or(false) => Some(path),
            _ => None,
        };
        let include_dirty = dirty_action == DirtyAction::Include && dirty_checkout.is_some();
        // Refuse an empty adoption: with nothing beyond the base and nothing
        // dirty to fold in, the card's diff — what self-review reviews and the
        // PR ships — would be empty.
        let subjects = log_subjects(&project.path, base, &cut_point).unwrap_or_default();
        if subjects.is_empty() && !include_dirty {
            return Err(CoreError::other(format!(
                "`{source_ref}` has no commits beyond `{base}` and no uncommitted changes \
                 to include — there is nothing to adopt"
            )));
        }

        let mut card = Card::new(
            project_id,
            &title,
            &description,
            project.config.new_card_config(),
        );
        // Adoption enters the pipeline where an implement run exits: parked at
        // the self-review gate. A direct set, not a Transition — this creation
        // path never goes through the state machine, exactly like tests seeding
        // post-implement cards.
        card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
        let (branch, worktree) = self.cut_card_worktree(&project, &card, &cut_point).await?;
        if include_dirty {
            let checkout = dirty_checkout
                .as_deref()
                .expect("include_dirty implies a checkout");
            if let Err(e) = self
                .copy_dirty_snapshot(checkout, &worktree, &source_ref)
                .await
            {
                // Nothing has been persisted yet — undo the git artifacts so a
                // failed include leaves no half-adopted card behind.
                let _ = self
                    .remove_worktree_retrying(&project.path, &worktree)
                    .await;
                let _ = std::fs::remove_dir_all(&worktree);
                let _ = self.git.delete_branch(&project.path, &branch).await;
                return Err(CoreError::other(format!(
                    "could not include the uncommitted changes from `{source_ref}`: {e}"
                )));
            }
        }
        card.branch = Some(branch);
        card.worktree_path = Some(worktree);
        self.store.upsert_card(&card)?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::updated(card.clone()));

        // Retire the original LOCAL branch (a remote-only ref has nothing local
        // to retire; the remote branch is never touched). Best-effort: the
        // card's own branch carries the work either way, so a failure only
        // means the original sticks around.
        if retire_original && local_exists {
            if checkout_of_branch(&project.path, &local).is_some() {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                    card.id,
                    Severity::Warning,
                    format!(
                        "`{local}` is checked out in a working tree, so it was left in place \
                         — the card carries the work on its own branch."
                    ),
                ));
            } else if let Err(e) = self.git.delete_branch(&project.path, &local).await {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                    card.id,
                    Severity::Warning,
                    format!("could not retire `{local}`: {e}"),
                ));
            }
        }

        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            card.id,
            Severity::Success,
            format!("Adopted `{source_ref}` — self-review running"),
        ));
        // Start the self-review under the card's exclusive claim, exactly like
        // the post-implement auto-start (`start_self_review_direct`): this
        // project-scoped command never claimed the (brand-new) card, and the
        // claim guards the launch window against a concurrent user command.
        let Some(_guard) = claim(&self.in_flight, &self.evt_tx, card.id) else {
            return Ok(());
        };
        match self.self_review(card.id).await {
            Ok(()) => Ok(()),
            // A user command got there first — their choice stands; the card
            // rests at `ReadyForReview` with the manual buttons.
            Err(CoreError::IllegalTransition(_)) => Ok(()),
            // A failed launch already demoted the card to Failed (retryable);
            // surface the error without undoing the adoption — the work is on
            // the card's branch now.
            Err(e) => {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                    card.id,
                    Severity::Error,
                    e.to_string(),
                ));
                Ok(())
            }
        }
    }

    /// Copy `checkout`'s uncommitted state — the tracked diff plus untracked
    /// files — into the card's worktree and commit it there. Strictly a copy:
    /// the user's checkout is read, never written.
    async fn copy_dirty_snapshot(
        &self,
        checkout: &Path,
        worktree: &Path,
        source_ref: &str,
    ) -> Result<()> {
        let patch = self.git.uncommitted_patch(checkout).await?;
        if !patch.trim().is_empty() {
            self.git.apply_patch(worktree, &patch).await?;
        }
        for rel in self.git.untracked_files(checkout).await? {
            let dest = worktree.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(checkout.join(&rel), &dest)?;
        }
        self.git
            .commit_all(
                worktree,
                &format!("Adopt uncommitted changes from {source_ref}"),
            )
            .await?;
        Ok(())
    }

    /// Why `source_ref` can't be adopted, or `None` when it can. Shared by the
    /// probe (which surfaces it in the dialog) and the adopt handler (which
    /// enforces it).
    fn adopt_refusal(&self, project: &Project, source_ref: &str) -> Option<String> {
        let local = local_name(source_ref);
        if local == project.config.effective_base_branch() {
            return Some(format!("`{source_ref}` is the project's base branch"));
        }
        if local.starts_with("usine/") || local.starts_with("usine-review/") {
            return Some(format!("`{source_ref}` is a Usine-owned branch"));
        }
        if let Ok(cards) = self.store.list_cards_for_project(project.id) {
            if let Some(card) = cards.iter().find(|c| {
                c.branch.as_deref() == Some(local) || c.branch.as_deref() == Some(source_ref)
            }) {
                return Some(format!(
                    "`{source_ref}` already belongs to the card “{}”",
                    card.title
                ));
            }
        }
        if !commitish_exists(&project.path, source_ref) {
            return Some(format!("`{source_ref}` does not resolve to a commit"));
        }
        None
    }

    /// The branches already owned by the project's cards.
    fn card_branches(&self, project_id: Uuid) -> HashSet<String> {
        self.store
            .list_cards_for_project(project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| c.branch)
            .collect()
    }
}

/// The name a ref is adopted under locally: `origin/x` and `x` are the same
/// branch, and everything (dirty probe, retire, card ownership) keys off the
/// local name.
fn local_name(source_ref: &str) -> &str {
    source_ref.strip_prefix("origin/").unwrap_or(source_ref)
}

/// Filter the raw branch listing down to adoptable refs: drop the base (both
/// sides), Usine-owned refs, branches already belonging to cards, `origin/HEAD`
/// (a symref, not a branch), and remote-tracking duplicates of a local branch
/// (deduped toward the local name, which is the tip that gets adopted).
fn adopt_source_refs(all: &[String], base: &str, card_owned: &HashSet<String>) -> Vec<String> {
    let locals: HashSet<&str> = all
        .iter()
        .map(String::as_str)
        .filter(|b| !b.starts_with("origin/"))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for b in all {
        if b == "origin/HEAD" {
            continue;
        }
        let local = local_name(b);
        if local == base
            || local.starts_with("usine/")
            || local.starts_with("usine-review/")
            || card_owned.contains(local)
            || card_owned.contains(b.as_str())
        {
            continue;
        }
        if b.starts_with("origin/") && locals.contains(local) {
            continue;
        }
        if !out.contains(b) {
            out.push(b.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn adopt_sources_filter_bases_usine_and_card_refs() {
        let all = strs(&[
            "dev",
            "feature",
            "usine/other-card-abc",
            "usine-review/42",
            "origin/HEAD",
            "origin/dev",
            "origin/feature",
            "origin/remote-only",
            "origin/usine/pushed-card",
        ]);
        let owned: HashSet<String> = ["taken".to_string()].into();
        let refs = adopt_source_refs(&all, "dev", &owned);
        assert_eq!(refs, strs(&["feature", "origin/remote-only"]));
    }

    /// A branch a card owns is excluded whether the listing shows it locally
    /// or as its remote-tracking twin.
    #[test]
    fn adopt_sources_exclude_card_owned_branches() {
        let all = strs(&["mine", "origin/mine"]);
        let owned: HashSet<String> = ["mine".to_string()].into();
        assert!(adopt_source_refs(&all, "dev", &owned).is_empty());
    }

    #[test]
    fn local_name_strips_only_origin() {
        assert_eq!(local_name("origin/feat/x"), "feat/x");
        assert_eq!(local_name("feat/origin-ish"), "feat/origin-ish");
    }
}
