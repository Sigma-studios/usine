//! Adoption: turning existing work into a first-class card.
//!
//! - A hand-made **branch** enters the pipeline at the self-review pass. The
//!   card gets its own `usine/<slug>` branch cut at the adopted tip, so the
//!   original branch stays exactly what it was and every downstream mechanism
//!   (self-review, fixes, validation, PR, teardown, back-to-start, delete)
//!   works unchanged — no "adopted" flag, no special cases.
//! - An open **PR** (say, one opened on another machine) enters at the
//!   PR-review stage. Here the card attaches to the PR's own head branch
//!   instead of cutting one: a card pushes `card.branch`, and the PR poll only
//!   needs `card.pr`, so a card whose branch *is* the PR head takes that PR
//!   over — every later push updates it, and no second PR is ever opened.
//!
//! Deliberately out of scope (v1):
//! - Stacked branches: the diff and the eventual PR always target the
//!   project's base branch, even if the adopted branch forked from another —
//!   and a PR targeting another base is refused.
//! - PRs from forks: the card couldn't push its fixes as its own branch.

use std::collections::HashSet;

use crate::agent::events::{AdoptProbe, DirtyAction};
use crate::domain::model::PrInfo;
use crate::infra::forge::OpenPr;
use crate::infra::git::{
    branch_relation, checkout_of_branch, commitish_exists, force_branch, is_dirty, log_subjects,
    remote_tracking_base, Relation,
};

use super::*;

impl Executor {
    /// List the project's branches and open PRs a card could adopt (the
    /// dialog's picker).
    pub(super) async fn list_adopt_sources(&self, project_id: Uuid) -> Result<()> {
        let project = self.store.get_project(project_id)?;
        let base = project.config.effective_base_branch();
        let all = self.git.list_all_branches(&project.path).await?;
        let owned = self.card_branches(project_id);
        let refs = adopt_source_refs(&all, base, &owned);
        // Best-effort: offline or unauthed, the branches still load.
        let prs = match self.forge.list_open_prs(&project.path).await {
            Ok(prs) => prs,
            Err(e) => {
                tracing::warn!("listing open PRs to adopt failed: {e}");
                Vec::new()
            }
        };
        let prs = adoptable_prs(prs, base, &owned, &self.card_pr_numbers(project_id));
        let refs = drop_pr_heads(refs, &prs);
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::adopt_sources(project_id, refs, prs));
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
            // Fetch so the remote-tracking base the count runs against is
            // current — the probe must predict what adoption will compute.
            // Non-fatal: offline, the tracking ref is merely stale.
            if let Err(e) = self.git.fetch(&project.path, "origin").await {
                tracing::warn!("probe {source_ref}: fetching origin failed: {e}");
            }
            // `origin/<base>` when it exists, never the user's local base: the
            // local one can sit far behind the remote, and counting against it
            // inflates commits-ahead with everything merged since their last
            // pull — the same rule the card's diff uses (see `diff.rs`).
            let base = remote_tracking_base(&project.path, project.config.effective_base_branch());
            subjects = log_subjects(&project.path, &base, &source_ref).unwrap_or_default();
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
        // Diffed against `origin/<base>` (freshly fetched above) when it
        // exists, matching the probe and the card's own diff: a stale local
        // base would let a branch fully merged into the remote base pass the
        // empty-adoption guard below.
        let base = remote_tracking_base(&project.path, project.config.effective_base_branch());
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
        let subjects = log_subjects(&project.path, &base, &cut_point).unwrap_or_default();
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
            self.store.settings()?.new_card_config(),
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

        // Start the self-review under the card's exclusive claim, exactly like
        // the post-implement auto-start (`start_self_review_direct`): this
        // project-scoped command never claimed the (brand-new) card, and the
        // claim guards the launch window against a concurrent user command.
        // The success toast waits until the launch outcome is known, so it
        // never claims "running" for a card that ends up parked or failed.
        let adopted = |detail: &str| {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card.id,
                Severity::Success,
                format!("Adopted `{source_ref}`{detail}"),
            ));
        };
        let Some(_guard) = claim(&self.in_flight, &self.evt_tx, card.id) else {
            adopted("");
            return Ok(());
        };
        match self.self_review(card.id).await {
            Ok(()) => {
                adopted(" — self-review running");
                Ok(())
            }
            // A user command got there first — their choice stands; the card
            // rests at `ReadyForReview` with the manual buttons.
            Err(CoreError::IllegalTransition(_)) => {
                adopted("");
                Ok(())
            }
            // A failed launch already demoted the card to Failed (retryable);
            // surface the error without undoing the adoption — the work is on
            // the card's branch now.
            Err(e) => {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                    card.id,
                    Severity::Error,
                    format!("Adopted `{source_ref}`, but self-review failed to start: {e}"),
                ));
                Ok(())
            }
        }
    }

    /// Adopt open PR `pr_number` into a new card that takes the PR over: the
    /// card attaches to the PR's head branch and lands at the PR-review stage,
    /// with its comments, checks and mergeability pulled right away. Validates
    /// everything before creating anything, like [`Self::adopt_branch`].
    pub(super) async fn adopt_pr(
        &self,
        project_id: Uuid,
        pr_number: u64,
        title: String,
        description: String,
    ) -> Result<()> {
        let project = self.store.get_project(project_id)?;
        // Refresh origin so the head's remote-tracking ref exists and is
        // current. Non-fatal, as in `adopt_branch`.
        if let Err(e) = self.git.fetch(&project.path, "origin").await {
            tracing::warn!("adopt PR #{pr_number}: fetching origin failed: {e}");
        }
        let target = self
            .forge
            .pr_push_target(&project.path, pr_number)
            .await?
            .ok_or_else(|| {
                CoreError::other(format!("couldn't read PR #{pr_number} from the forge"))
            })?;
        if target.cross_repo {
            return Err(CoreError::other(format!(
                "cannot adopt PR #{pr_number}: PRs from forks can't be adopted — review them \
                 from the PR-review board instead"
            )));
        }
        let base = project.config.effective_base_branch();
        if !target.base_ref.is_empty() && target.base_ref != base {
            return Err(CoreError::other(format!(
                "cannot adopt PR #{pr_number}: it targets `{}`, not the project's base `{base}`",
                target.base_ref
            )));
        }
        let pr = self
            .forge
            .pr_by_number(&project.path, pr_number)
            .await?
            .ok_or_else(|| CoreError::other(format!("PR #{pr_number} is no longer open")))?;
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err(CoreError::other("a title is required to adopt a PR"));
        }
        if description.trim().is_empty() {
            return Err(CoreError::other(
                "a task description is required to adopt a PR — it's what the \
                 triage and fix agents read as the statement of intent",
            ));
        }
        let head = target.head_ref;
        if let Some(refusal) = self.adopt_pr_refusal(&project, &head, pr_number) {
            return Err(CoreError::other(format!(
                "cannot adopt PR #{pr_number}: {refusal}"
            )));
        }
        // The card's worktree must own the head branch: git won't check one
        // branch out twice, and pushes from someone else's checkout would be
        // the cross-card contamination `finalize_run` guards against.
        if let Some(path) = checkout_of_branch(&project.path, &head) {
            return Err(CoreError::other(format!(
                "cannot adopt PR #{pr_number}: `{head}` is checked out at {} — switch that \
                 checkout to another branch first",
                path.display()
            )));
        }
        let remote = format!("refs/remotes/origin/{head}");
        let local_ref = format!("refs/heads/{head}");
        let mut card = Card::new(
            project_id,
            &title,
            &description,
            self.store.settings()?.new_card_config(),
        );
        let worktree = worktree_path(&project.path, card.id);
        match branch_relation(&project.path, &local_ref, &remote) {
            // No local branch yet: create it at the remote tip. `create_worktree`
            // cuts with `--no-track`; the card's first push `-u` sets upstream.
            None => {
                self.git
                    .create_worktree(&project.path, &head, &worktree, &format!("origin/{head}"))
                    .await?
            }
            Some(Relation::Diverged) => {
                return Err(CoreError::other(format!(
                    "cannot adopt PR #{pr_number}: your local `{head}` and `origin/{head}` have \
                     diverged — reconcile them (or delete the local branch) first"
                )));
            }
            Some(relation) => {
                // Behind: catch up to what the PR shows. Ahead or same: keep
                // it — unpushed commits go out with the card's next push.
                if relation == Relation::Behind {
                    force_branch(&project.path, &head, &remote)?;
                }
                self.git
                    .worktree_add_existing(&project.path, &head, &worktree)
                    .await?;
            }
        }

        // The work is already a PR, so the card enters where `create_pr`
        // leaves one — skipping self-review. A direct set, not a Transition,
        // for the same reason as in `adopt_branch`.
        card.state = CardState::PrReview(PrReviewSub::Idle);
        card.branch = Some(head);
        card.worktree_path = Some(worktree);
        // Read back from the forge, so its reviewer (possibly none) is
        // authoritative — the same stance as `create_pr`'s recovery.
        card.pr = Some(PrInfo {
            reviewer_recorded: true,
            ..pr
        });
        self.store.upsert_card(&card)?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::updated(card.clone()));
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            card.id,
            Severity::Success,
            format!("Adopted PR #{pr_number} — now tracking its review"),
        ));

        // Pull the PR's comments, reviews, checks and mergeability now rather
        // than on the next poll. This also runs the auto-advances — including
        // "no reviewer → ready to merge", which `create_pr` applies up front
        // but which here must wait for the comment counts: an adopted PR may
        // already carry feedback. Under the card's claim, since it can
        // transition; best-effort, since the poll catches up.
        if let Some(_guard) = claim(&self.in_flight, &self.evt_tx, card.id) {
            if let Err(e) = self.list_reviews(card.id).await {
                tracing::warn!("adopt PR #{pr_number}: first review refresh failed: {e}");
            }
        }
        Ok(())
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
        if !patch.iter().all(u8::is_ascii_whitespace) {
            self.git.apply_patch(worktree, &patch).await?;
        }
        for rel in self.git.untracked_files(checkout).await? {
            let src = checkout.join(&rel);
            let dest = worktree.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Recreate a symlink as a symlink: `fs::copy` follows links, which
            // would materialize the target as a file (or fail on a dangling
            // link) instead of carrying the link itself onto the card branch.
            if std::fs::symlink_metadata(&src)?.file_type().is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dest)?;
                #[cfg(not(unix))]
                return Err(CoreError::other(format!(
                    "cannot include symlink `{}` on this platform",
                    rel.display()
                )));
            } else {
                std::fs::copy(&src, &dest)?;
            }
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

    /// Why PR head `head` can't be adopted, or `None` when it can: the
    /// branch-adoption checks that still apply once the head is attached to
    /// rather than cut from — a `usine/` head is fine here (a card pushed from
    /// another machine is this feature's main case).
    fn adopt_pr_refusal(&self, project: &Project, head: &str, pr_number: u64) -> Option<String> {
        if head == project.config.effective_base_branch() {
            return Some(format!("its head `{head}` is the project's base branch"));
        }
        if let Ok(cards) = self.store.list_cards_for_project(project.id) {
            if let Some(card) = cards.iter().find(|c| {
                c.branch.as_deref() == Some(head)
                    || c.pr.as_ref().is_some_and(|p| p.number == pr_number)
            }) {
                return Some(format!("it already belongs to the card “{}”", card.title));
            }
        }
        if !commitish_exists(&project.path, &format!("refs/remotes/origin/{head}")) {
            return Some(format!(
                "`origin/{head}` does not resolve — is the PR's branch on `origin`?"
            ));
        }
        None
    }

    /// The PR numbers already owned by the project's cards.
    fn card_pr_numbers(&self, project_id: Uuid) -> HashSet<u64> {
        self.store
            .list_cards_for_project(project_id)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|c| c.pr.map(|p| p.number))
            .collect()
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

/// Filter the forge's open PRs down to adoptable ones: same-repo, targeting
/// the project base, and not already a card's (by branch or PR number). Keeps
/// the forge's order (the viewer's own PRs first).
fn adoptable_prs(
    prs: Vec<OpenPr>,
    base: &str,
    card_branches: &HashSet<String>,
    card_prs: &HashSet<u64>,
) -> Vec<OpenPr> {
    prs.into_iter()
        .filter(|p| {
            !p.cross_repo
                && p.base_ref == base
                && !card_branches.contains(&p.head_ref)
                && !card_prs.contains(&p.number)
        })
        .collect()
}

/// Drop every branch (local or `origin/…`) that is a listed PR's head, so each
/// piece of work shows up once — in the dialog's PR group, where picking it
/// adopts the PR itself rather than cutting a second branch off it.
fn drop_pr_heads(refs: Vec<String>, prs: &[OpenPr]) -> Vec<String> {
    let heads: HashSet<&str> = prs.iter().map(|p| p.head_ref.as_str()).collect();
    refs.into_iter()
        .filter(|r| !heads.contains(local_name(r)))
        .collect()
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

    fn open_pr(number: u64, head: &str) -> OpenPr {
        OpenPr {
            number,
            title: format!("PR {number}"),
            author: "me".into(),
            head_ref: head.into(),
            base_ref: "dev".into(),
            url: String::new(),
            body: String::new(),
            draft: false,
            cross_repo: false,
            mine: true,
        }
    }

    #[test]
    fn adoptable_prs_drop_forks_other_bases_and_card_owned() {
        let fork = OpenPr {
            cross_repo: true,
            ..open_pr(2, "patch-1")
        };
        let other_base = OpenPr {
            base_ref: "release".into(),
            ..open_pr(3, "hotfix")
        };
        let prs = vec![
            open_pr(1, "usine/from-laptop-1234"),
            fork,
            other_base,
            open_pr(4, "taken-branch"),
            open_pr(5, "taken-number"),
            open_pr(6, "feat/b"),
        ];
        let branches: HashSet<String> = ["taken-branch".to_string()].into();
        let numbers: HashSet<u64> = [5].into();
        let kept: Vec<u64> = adoptable_prs(prs, "dev", &branches, &numbers)
            .iter()
            .map(|p| p.number)
            .collect();
        assert_eq!(kept, vec![1, 6], "a usine/ head is adoptable as a PR");
    }

    #[test]
    fn pr_heads_leave_the_branch_list() {
        let refs = strs(&["feat/b", "origin/feat/c", "other", "origin/feat/b-2"]);
        let prs = [open_pr(1, "feat/b"), open_pr(2, "feat/c")];
        assert_eq!(
            drop_pr_heads(refs, &prs),
            strs(&["other", "origin/feat/b-2"])
        );
    }

    #[test]
    fn local_name_strips_only_origin() {
        assert_eq!(local_name("origin/feat/x"), "feat/x");
        assert_eq!(local_name("feat/origin-ish"), "feat/origin-ish");
    }
}
