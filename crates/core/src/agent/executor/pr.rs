//! Opening the card's own pull request and driving its review: creating the
//! PR, fetching + applying reviewer comments, marking ready, and merging.

use super::*;
use crate::infra::git::is_dirty;

/// Base for the synthetic ids given to review-*body* triage items. Far above
/// any real GitHub comment id so they can't collide, but well below 2^53 so
/// the id survives an f64 round-trip: the triage agent echoes ids back through
/// JSON, and an id up at `u64::MAX` comes back float-rounded to a number that
/// doesn't even fit a u64.
const SYNTHETIC_BODY_ID_BASE: u64 = 9_000_000_000_000_000;

/// Whether the CI poll should re-read this card's checks: it's parked at the PR
/// or merge gate with a build we believe is in flight. Everything else either
/// has no PR to read or has already settled.
fn ci_polled(card: &Card) -> bool {
    matches!(
        card.state,
        CardState::PrReview(PrReviewSub::Idle) | CardState::ReadyToMerge
    ) && card.checks == CheckStatus::Pending
}

/// Record that a push just (re)started this card's PR build. On a project whose
/// PRs get CI this is unconditional; on a project we believe has no CI, only a
/// card that has actually seen a status before is re-armed — a card that never
/// had one keeps its honest `None`.
///
/// Either way the re-arm stamps the registration grace, so the seconds before
/// GitHub registers the run read as "still running" rather than as "no checks"
/// — the empty rollup that used to turn a fresh PR green. Without the stamp on
/// the second branch the re-arm was worthless: the very next rollup read (empty,
/// because the run isn't registered yet) settled straight back to `None`, and
/// the card went green — badge and all — on a build that had just started.
pub(super) fn mark_ci_in_flight(card: &mut Card, expects_ci: bool) {
    if !expects_ci && card.checks == CheckStatus::None {
        return;
    }
    card.checks = CheckStatus::Pending;
    card.ci_awaited_since = Some(now_millis());
}

impl Executor {
    pub(super) async fn create_pr(
        &self,
        card_id: Uuid,
        branch: String,
        title: String,
        body: String,
        reviewer: Option<String>,
        draft: bool,
    ) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let current = card
            .branch
            .clone()
            .ok_or_else(|| CoreError::other("card has no branch to open a PR from"))?;
        // The branch was checked out in the main repo (its worktree torn down at
        // checkout-for-review), so git ops run in the project root; fall back to
        // the worktree if one somehow remains.
        let dir = card
            .worktree_path
            .clone()
            .unwrap_or_else(|| project.path.clone());

        // Rename to the user's chosen branch name before the first push, so the
        // PR opens from a meaningfully-named branch and no stale remote branch is
        // left behind (nothing has been pushed yet).
        //
        // The name is sanitised here rather than trusted from the UI: this is the
        // last point before it becomes a real ref, and a name git rejects (or
        // silently stores under a different one) strands the card mid-flow.
        let requested = sanitize_branch_name(&branch);
        if requested.is_empty() && !branch.trim().is_empty() {
            return Err(CoreError::other(format!(
                "`{}` has no usable branch name in it",
                branch.trim()
            )));
        }
        // On a case-insensitive filesystem git will fold `Fix/x` into an existing
        // `fix/` directory, so ask for the name it would actually create — see
        // `canonicalize_branch_case`. An empty listing means the backend doesn't
        // report branches (the simulator), and canonicalising is then a no-op.
        let existing = self.git.local_branches(&dir).await.unwrap_or_default();
        let target = canonicalize_branch_case(&requested, &existing);

        let head = if target.is_empty() || target == current {
            current.clone()
        } else {
            self.git.rename_branch(&dir, &current, &target).await?;
            // `git branch -m` exits 0 even when the filesystem stored the ref
            // under a name other than the one asked for, so confirm the branch
            // exists as named before recording it — otherwise the card would
            // point at a branch that can't be pushed, and the wrong name would
            // block every retry. (Canonicalising above handles the ASCII case;
            // this also covers Unicode folding, which it doesn't.)
            let after = self.git.local_branches(&dir).await.unwrap_or_default();
            if !after.is_empty() && !after.iter().any(|b| b == &target) {
                // Best-effort: name it back, which restores HEAD and leaves the
                // card exactly where it was so the user can pick another name.
                let _ = self.git.rename_branch(&dir, &target, &current).await;
                return Err(CoreError::other(format!(
                    "the repository stored `{target}` under a different name — its \
                     filesystem ignores capitalisation. Pick a branch name that \
                     doesn't differ only in case from an existing one; the card is \
                     still on `{current}`."
                )));
            }
            let updated = self.store.mutate_card(card_id, |c| {
                c.branch = Some(target.clone());
                c.updated_at = now_millis();
                Ok(())
            })?;
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
            // The name that lands can differ from what was typed (sanitising,
            // or adopting an existing directory's case). Say so rather than
            // letting the PR quietly open from an unexpected branch.
            if target != branch.trim() {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                    card_id,
                    Severity::Info,
                    format!("Branch named `{target}`"),
                ));
            }
            target.clone()
        };

        // First push: the pre-PR branch is kept local until now.
        self.git.push(&dir, &head).await?;

        // Draft PRs let the user add screenshots on GitHub (no API embeds images
        // in a PR body) then mark it ready; a non-draft opens straight for review.
        let pr = self
            .forge
            .create_pr(
                &project.path,
                &title,
                &body,
                project.config.effective_base_branch(),
                &head,
                reviewer.as_deref(),
                draft,
            )
            .await?;

        let number = pr.number;
        let expects_ci = project.expects_ci();
        self.store.mutate_card(card_id, |c| {
            c.pr = Some(pr.clone());
            // A brand-new PR has no feedback or check results yet — clear the
            // PR-derived caches so nothing from an earlier PR of this card
            // (however it was dropped) leaks onto this one.
            c.reviewer_comment_count = 0;
            c.comment_count = 0;
            c.unanswered_count = 0;
            c.reviews.clear();
            c.triaged_review_bodies.clear();
            c.checks = CheckStatus::None;
            c.ci_awaited_since = None;
            // …and on a project whose PRs get CI, say so *now*. This PR's build
            // is about to start, and GitHub needs a few seconds to register it;
            // reporting `None` in the meantime is what let a card that advances
            // straight to the merge gate (no reviewer) show a green Merge button
            // and light the dock badge before its build existed.
            mark_ci_in_flight(c, expects_ci);
            c.mergeable = Mergeable::Unknown;
            c.updated_at = now_millis();
            Ok(())
        })?;

        self.apply(card_id, Transition::CreatePr)?;
        // With no reviewer to wait on there is nothing the PR gate can ever
        // receive, so advance to the merge gate now rather than making the card
        // wait for the poll to notice. The PR was just created, so its recorded
        // reviewer is authoritative — an explicit "no reviewer" advances even
        // on a project with a configured one (see `PrInfo::effective_reviewer`).
        // A draft still advances: `ReadyToMerge` gates it behind "Mark ready".
        let card = self.store.get_card(card_id)?;
        let reviewer = pr.effective_reviewer(project.config.reviewer.as_deref());
        if card.no_reviewer_clears_merge(reviewer) {
            self.apply(card_id, Transition::ReviewApproved)?;
            self.progress(card_id, "✔ no reviewer assigned — ready to merge");
        }
        let msg = if draft {
            format!("Draft PR #{number} created — add screenshots on GitHub, then mark it ready.")
        } else {
            format!("PR #{number} created")
        };
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::toast(card_id, Severity::Success, msg));
        Ok(())
    }

    /// Fetch the PR's *unanswered* review threads and launch an agent to triage
    /// them (decide worth-fixing + draft a reply for the ones left unfixed).
    /// Threads an earlier pass fixed or replied to are excluded (see
    /// [`thread_triage_items`]), which is what makes this safe to re-enter from
    /// `ReadyToMerge` when comments land after a pass. The items are stashed so
    /// the triage run's completion can join verdicts back to them by id (see
    /// `finalize_triage`). With nothing to triage, park with an empty picker.
    pub(super) async fn fetch_comments(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let pr_number = card
            .pr
            .as_ref()
            .map(|p| p.number)
            .ok_or_else(|| CoreError::other("card has no PR to read comments from"))?;

        // Talk to the forge BEFORE the running-state transition, so a failed
        // fetch leaves the card where it was (the PR gate, the fix picker, or
        // the merge gate — all recoverable) instead of stranded mid-fetch.
        let comments = self.forge.fetch_comments(&project.path, pr_number).await?;
        let items = match self.forge.list_threads(&project.path, pr_number).await {
            Ok(threads) => {
                // Keep the freshly-learned unanswered count on the card, so the
                // merge gate's "reevaluate" offer tracks what this fetch saw.
                let unanswered = threads.iter().filter(|t| t.is_unanswered()).count();
                if unanswered != card.unanswered_count {
                    let updated = self.store.mutate_card(card_id, |c| {
                        c.unanswered_count = unanswered;
                        Ok(())
                    })?;
                    let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
                }
                thread_triage_items(&comments, &threads)
            }
            // No thread info (a GraphQL hiccup, or a forge that can't answer):
            // fall back to triaging the raw comment list — the pre-thread
            // behavior — rather than blocking triage entirely. Say so, since
            // this path can re-surface already-answered comments.
            Err(e) => {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                    card_id,
                    Severity::Warning,
                    format!("couldn't read review threads — triaging every comment: {e}"),
                ));
                comments.clone()
            }
        };

        // Review *bodies* ride along as synthetic items: refetch the submitted
        // reviews (still pre-transition) so a body that just landed is triaged
        // with the comments. Best-effort — a hiccup falls back to what the card
        // already knows rather than blocking the triage of inline comments.
        let reviews = match self
            .forge
            .list_submitted_reviews(&project.path, pr_number)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("comment triage: couldn't refresh reviews for #{pr_number}: {e}");
                card.reviews.clone()
            }
        };
        let card = if reviews != card.reviews {
            let updated = self.store.mutate_card(card_id, |c| {
                c.reviews = reviews;
                Ok(())
            })?;
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::updated(updated.clone()));
            updated
        } else {
            card
        };
        let mut items = items;
        // Synthetic ids count up from a base no real GitHub comment id reaches
        // (see [`SYNTHETIC_BODY_ID_BASE`]); `review_body_of` carries the body's
        // identity so applying the picker can record it handled.
        for (k, r) in card.pending_review_bodies().into_iter().enumerate() {
            items.push(ReviewComment {
                id: SYNTHETIC_BODY_ID_BASE + k as u64,
                author: r.author.clone(),
                path: String::new(),
                line: None,
                body: r.body.clone(),
                review_body_of: Some(r.body_key()),
            });
        }

        self.apply(card_id, Transition::FetchComments)?;
        if items.is_empty() {
            self.apply(card_id, Transition::CommentsFetched { verdicts: vec![] })?;
            return Ok(());
        }

        self.store.set_pending_comments(card_id, &items)?;
        // Like the self-review, triage is a fresh conversation that would otherwise
        // judge the comments against the card's original description alone.
        let background = card_background(&self.store, &card);
        let extra = join_sections(&[&background, &triage_prompt(&items)]);
        let card = self.store.get_card(card_id)?;
        self.launch(card, RunMode::Triage, Some(extra), None).await
    }

    /// Apply the checked PR-comment fixes and reply to the ignored ones — from
    /// the verdicts as the user left them, edits included. The checked comments
    /// — plus the user's free-form `note`, if any — go to an agent run (which
    /// commits + pushes and captures a fixes recap); each unchecked comment with
    /// a (possibly edited) reply gets that reply posted on GitHub. With neither
    /// a checked comment nor a note, records a recap and advances.
    pub(super) async fn apply_fixes(
        &self,
        card_id: Uuid,
        verdicts: Vec<FixVerdict>,
        note: String,
        prompt: Option<String>,
    ) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let pr_number = card.pr.as_ref().map(|p| p.number);
        if !matches!(
            &card.state,
            CardState::PrReview(PrReviewSub::SelectingFixes { .. })
        ) {
            return Err(CoreError::IllegalTransition(
                "can only apply fixes while selecting fixes".into(),
            ));
        }
        // Disposing of the picker is what handles the review *bodies* in it —
        // fixed or declined, the user has read them. Record their keys now so
        // they stop counting as pending feedback (a triage run cancelled
        // before this point leaves them pending, by design).
        let body_keys: Vec<String> = verdicts
            .iter()
            .filter_map(|v| v.comment.review_body_of.clone())
            .collect();
        if !body_keys.is_empty() {
            let updated = self.store.mutate_card(card_id, |c| {
                for k in body_keys {
                    if !c.triaged_review_bodies.contains(&k) {
                        c.triaged_review_bodies.push(k);
                    }
                }
                Ok(())
            })?;
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        }
        let (checked, ignored): (Vec<FixVerdict>, Vec<FixVerdict>) =
            verdicts.into_iter().partition(|v| v.selected);

        // Reply to the ignored comments with the agent's short explanation
        // (best-effort — a failed reply shouldn't block applying the fixes).
        // A review-body item never gets one: GitHub has no reply endpoint for
        // a review body (its synthetic id isn't a real comment id).
        if let Some(pr) = pr_number {
            for v in &ignored {
                if v.comment.review_body_of.is_some() || v.reply.trim().is_empty() {
                    continue;
                }
                match self
                    .forge
                    .reply_to_comment(&project.path, pr, v.comment.id, &v.reply)
                    .await
                {
                    // Keep the decision on the restart log: a later reset run
                    // must know the comment was declined, not overlooked.
                    Ok(()) => self.record_qa(
                        card_id,
                        format!(
                            "Declined review comment with reply: {}",
                            one_line_capped(&v.reply, 200)
                        ),
                    ),
                    Err(e) => {
                        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                            card_id,
                            Severity::Warning,
                            format!("couldn't reply to a comment: {e}"),
                        ));
                    }
                }
            }
        }

        let note = note.trim().to_string();
        // The user may have hand-edited the task in the picker; that text is
        // what the agent gets, verbatim. Its emptiness — not the checkboxes —
        // is what decides whether there is anything to run (with `None` this is
        // exactly the old "nothing checked and no note" condition). Computed
        // after the decline replies above: those follow the checkboxes and are
        // independent of the task text.
        let extra = match &prompt {
            Some(p) => p.trim().to_string(),
            None => fix_prompt(&checked, &note),
        };
        if extra.trim().is_empty() {
            // Nothing to fix — record a recap and advance straight to merge.
            self.apply(card_id, Transition::SelectFixes)?;
            let replied = ignored
                .iter()
                .filter(|v| !v.reply.trim().is_empty())
                .count();
            let recap = format!("No fixes applied; replied to {replied} comment(s).");
            self.store.set_review_recap(card_id, &recap)?;
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::recap_updated(card_id, recap));
            self.apply(card_id, Transition::AgentFixesDone)?;
            // No fix run means no `ResolveFixedComments` follow-up, so refresh
            // here: the replies above just answered threads, and the merge
            // gate's "reevaluate comments" offer must not still count them.
            if let Err(e) = self.list_reviews(card_id).await {
                tracing::warn!("post-reply review refresh failed: {e}");
            }
            return Ok(());
        }
        // Keep the note so a later "back to start" folds it into the prompt,
        // just like a post-PR change request — but only when it actually
        // reached the agent: an edited task replaces the composed text wholesale,
        // so the note never got sent and must not be logged as a request. The
        // checked comments are only STASHED here: their "Fix applied" lines go on
        // the log when the run lands its commit (see `finalize_run`), not at
        // launch — a cancelled or faulted run must not leave a durable claim that
        // it fixed anything.
        if prompt.is_none() && !note.is_empty() {
            self.record_qa(card_id, format!("Requested change: {note}"));
        }
        // The bookkeeping below deliberately follows the CHECKBOXES, not the
        // edited text: an edited task that drops a comment while its row stays
        // checked must not leave the log — or a resolved GitHub thread —
        // claiming otherwise than what the rows said.
        let mut fix_qa: Vec<String> = checked.iter().map(fixed_comment_qa).collect();
        if prompt.is_some() {
            fix_qa.push(format!(
                "Fix task as sent (edited by the user): {}",
                one_line_capped(&extra, 400)
            ));
        }
        self.store.set_pending_fix_qa(card_id, &fix_qa)?;
        // Remember which comments this run addresses so their GitHub review
        // threads can be marked resolved once the fix lands (see `finalize_run`,
        // which emits `ResolveFixedComments` on completion). A note-only run has
        // no comments to resolve, and a review-body item has no thread — its
        // synthetic id must not reach the resolve call.
        let fixed_ids: Vec<u64> = checked
            .iter()
            .filter(|v| v.comment.review_body_of.is_none())
            .map(|v| v.comment.id)
            .collect();
        self.store.set_pending_resolve(card_id, &fixed_ids)?;

        // Run the fix in an ISOLATED worktree on the card's branch — never in the
        // user's main working copy (the pre-PR checkout leaves the branch there).
        // Done before the running-state transition so a failure (e.g. a dirty main
        // repo) leaves the card in the recoverable fix picker.
        self.ensure_branch_worktree(card_id).await?;
        // Stash the task before entering the running state, so a retry of a
        // faulted run can restate it (see `relaunch`).
        self.store.set_fix_extra(card_id, Some(&extra))?;
        let card = self.apply(card_id, Transition::SelectFixes)?;
        self.launch(card, RunMode::ApplyFixes, Some(extra), None)
            .await
    }

    /// After a PR-comment fix run lands, mark the fixed comments' review threads
    /// resolved on GitHub, then refresh the card's review status. Best-effort: a
    /// failure just warns (the fix itself is already committed + pushed), and the
    /// stashed ids are consumed either way so a resolve is attempted only once
    /// per fix run.
    pub(super) async fn resolve_fixed_comments(&self, card_id: Uuid) -> Result<()> {
        let ids = self.store.take_pending_resolve(card_id)?;
        let card = self.store.get_card(card_id)?;
        let Some(pr) = card.pr.as_ref().map(|p| p.number) else {
            return Ok(());
        };
        if !ids.is_empty() {
            let project = self.store.get_project(card.project_id)?;
            match self.forge.resolve_threads(&project.path, pr, &ids).await {
                Ok(0) => {}
                Ok(n) => self.progress(card_id, &format!("✔ resolved {n} review thread(s)")),
                Err(e) => {
                    let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                        card_id,
                        Severity::Warning,
                        format!("couldn't resolve review threads: {e}"),
                    ));
                }
            }
        }
        // The run just resolved threads and/or posted replies (`apply_fixes`
        // replies to the unchecked comments before launching it), so the counts
        // the card last saw — from before the pass — are stale. Refresh now,
        // rather than waiting out the poll: the card typically just landed on
        // the merge gate, whose "reevaluate comments" offer reads
        // `unanswered_count` and must not reflect the pre-fix world.
        if let Err(e) = self.list_reviews(card_id).await {
            tracing::warn!("post-fix review refresh failed: {e}");
        }
        Ok(())
    }

    /// Guarantee the card has its isolated worktree, so any agent that *writes*
    /// (self-review fixes, PR-comment fixes, follow-up changes) runs — and
    /// commits/pushes — only there, never in the user's main working copy.
    ///
    /// The worktree normally persists for the card's whole life (implement →
    /// merge), so this is a no-op. It only rebuilds one that went missing (e.g. a
    /// crash or manual cleanup left the card without its worktree dir). The branch
    /// is never checked out in the main repo, so there is nothing to free there.
    pub(super) async fn ensure_branch_worktree(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        if let Some(wt) = &card.worktree_path {
            if wt.exists() {
                return Ok(());
            }
        }
        let project = self.store.get_project(card.project_id)?;
        let branch = card
            .branch
            .clone()
            .ok_or_else(|| CoreError::other("card has no branch to check out for the fix"))?;

        let wt = worktree_path(&project.path, card_id);
        // Clear any stale worktree/dir from a previous attempt.
        if wt.exists() {
            let _ = self.git.remove_worktree(&project.path, &wt).await;
            let _ = std::fs::remove_dir_all(&wt);
        }
        self.progress(card_id, "Setting up an isolated worktree for the fix…");
        self.git
            .worktree_add_existing(&project.path, &branch, &wt)
            .await?;
        let updated = self.store.mutate_card(card_id, |c| {
            c.worktree_path = Some(wt.clone());
            c.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        Ok(())
    }

    /// From "awaiting review", re-run the implement phase in the existing
    /// worktree to apply the reviewer's requested changes. The run is
    /// self-contained (plan + worktree note + the change request) so it stands on
    /// its own without relying on session resume — the worktree already holds the
    /// prior implementation for the agent to build on.
    pub(super) async fn revise(&self, card_id: Uuid, feedback: String) -> Result<()> {
        self.record_qa(card_id, format!("Requested change: {}", feedback.trim()));
        let plan = self.store.get_plan(card_id).unwrap_or(None);
        let extra = revise_extra(plan.as_deref(), &feedback);
        let card = self.apply(card_id, Transition::RequestChanges)?;
        self.launch(card, RunMode::Implement, Some(extra), None)
            .await
    }

    /// Ask the agent a question about the card's current work without sending
    /// it back for changes. Wraps the parked state in `CardState::Answering`
    /// and runs a strictly read-only [`RunMode::Question`] turn;
    /// `finalize_question` (see `actor.rs`) unwraps the card back where it
    /// started and records the prose answer. Self-contained like `revise` — no
    /// session resume.
    pub(super) async fn ask_question(&self, card_id: Uuid, question: String) -> Result<()> {
        let question = question.trim().to_string();
        let card = self.store.get_card(card_id)?;
        let stored_plan = self.store.get_plan(card_id).unwrap_or(None);
        let Some((stage, plan)) = question_context(&card.state, stored_plan) else {
            return Err(CoreError::IllegalTransition(
                "questions can only be asked while the work is parked for your review".into(),
            ));
        };
        if matches!(
            card.state,
            CardState::PrReview(PrReviewSub::Idle) | CardState::ReadyToMerge
        ) {
            // The question runs read-only, but make sure the branch is
            // checked out here so the agent looks at the PR's actual code
            // (mirrors `request_post_pr_change`).
            self.ensure_branch_worktree(card_id).await?;
        }
        let extra = question_extra(stage, plan.as_deref(), &question);
        let card = self.apply(
            card_id,
            Transition::AskQuestion {
                question: question.clone(),
            },
        )?;
        // Only now that the run is really happening, stash the question on the
        // answer record: `finalize_question` reads it back to render the
        // exchange and to log the answered Q&A pair on the restart log. A bare
        // question must never be logged up front — folded into a later prompt
        // it would read as a standing, unanswered directive.
        self.store.set_question(card_id, &question)?;
        self.launch(card, RunMode::Question, Some(extra), None)
            .await
    }

    /// Look up the project's reviewer candidates and hand them back to the UI.
    /// Project-scoped, so it emits a `Reviewers` event rather than a card update.
    pub(super) async fn list_reviewers(&self, project_id: Uuid) -> Result<()> {
        let project = self.store.get_project(project_id)?;
        let logins = self.forge.list_reviewers(&project.path).await?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::reviewers(project_id, logins));
        Ok(())
    }

    /// Look up the logins with an open PR on the project's repo — the
    /// contributor picker's suggestions, which cover the fork contributors the
    /// collaborator list can't. Project-scoped: emits a `PrAuthors` event.
    pub(super) async fn list_pr_authors(&self, project_id: Uuid) -> Result<()> {
        let project = self.store.get_project(project_id)?;
        let logins = self.forge.list_pr_authors(&project.path).await?;
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::pr_authors(project_id, logins));
        Ok(())
    }

    /// Read a PR's current review comments, submitted reviews, and unanswered-
    /// thread count together — the shared fetch behind both the background poll
    /// ([`Self::poll_pr_comments`]) and the panel's manual refresh
    /// ([`Self::list_reviews`]). Callers derive the two comment counts with
    /// [`comment_counts`]. The unanswered count is `None` when the thread
    /// listing failed — it rides on GraphQL, unlike the other two — so callers
    /// keep the card's previous value rather than guessing; the CI check status
    /// and the mergeability follow the same convention when their fetch failed
    /// (a fetched `Mergeable::Unknown` is a real answer and is stored as-is).
    /// The PR's live lifecycle state rides along under the same convention
    /// (`None` = can't tell), feeding [`Self::reconcile_pr_live_state`].
    pub(super) async fn fetch_review_status(
        &self,
        repo: &Path,
        pr_number: u64,
    ) -> Result<(
        Vec<ReviewComment>,
        Vec<ReviewSummary>,
        Option<usize>,
        Option<CheckStatus>,
        Option<Mergeable>,
        Option<LivePrState>,
    )> {
        let comments = self.forge.fetch_comments(repo, pr_number).await?;
        let reviews = self.forge.list_submitted_reviews(repo, pr_number).await?;
        let unanswered = match self.forge.list_threads(repo, pr_number).await {
            Ok(threads) => Some(threads.iter().filter(|t| t.is_unanswered()).count()),
            Err(e) => {
                tracing::warn!(
                    "review-status refresh: couldn't list threads for #{pr_number}: {e}"
                );
                None
            }
        };
        let checks = match self.forge.pr_checks(repo, pr_number).await {
            Ok((status, _)) => Some(status),
            Err(e) => {
                tracing::warn!("review-status refresh: couldn't read checks for #{pr_number}: {e}");
                None
            }
        };
        let mergeable = match self.forge.merge_status(repo, pr_number).await {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(
                    "review-status refresh: couldn't read mergeability of #{pr_number}: {e}"
                );
                None
            }
        };
        let live = match self.forge.pr_live_state(repo, pr_number).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "review-status refresh: couldn't read live state of #{pr_number}: {e}"
                );
                None
            }
        };
        Ok((comments, reviews, unanswered, checks, mergeable, live))
    }

    /// Refresh the submitted reviews *and* the comment counts on the card's PR.
    /// The user-triggered twin of what the background poll does on every tick —
    /// both land on the card, so the panel has one source of truth and the ↻
    /// button both skips the wait for the next tick and surfaces the triage button
    /// the instant a comment from any reviewer lands (without necessarily badging
    /// the card — that stays the assigned reviewer's job, see [`comment_counts`]).
    pub(super) async fn list_reviews(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let pr = card
            .pr
            .as_ref()
            .ok_or_else(|| CoreError::other("card has no PR to read reviews from"))?;
        // Owned so it outlives the refreshed `card` below.
        let reviewer = pr
            .effective_reviewer(project.config.reviewer.as_deref())
            .map(str::to_string);
        let (comments, reviews, unanswered, checks, mergeable, live) =
            self.fetch_review_status(&project.path, pr.number).await?;
        // A PR merged or closed on GitHub retires the card before any count
        // mutation or auto-advance — nothing below applies to a gone PR.
        if self.reconcile_pr_live_state(card_id, live).await? {
            return Ok(());
        }
        let (by_reviewer, total) = comment_counts(&comments, reviewer.as_deref());
        // A failed thread listing keeps the previous count (see fetch_review_status);
        // a failed checks or mergeability read likewise keeps the previous value.
        let unanswered = unanswered.unwrap_or(card.unanswered_count);
        let settled = checks.map(|read| {
            let settled = card.settle_checks(read, now_millis());
            self.learn_ci_on_prs(&project, read, settled);
            settled
        });
        let checks = settled.unwrap_or(card.checks);
        // The rollup answered (or the grace ran out): stop treating a future
        // empty read as "not registered yet", same as the polls and
        // [`Self::persist_checks`]. A failed read says nothing, so it leaves
        // the stamp alone.
        let clear_awaited =
            settled.is_some_and(|s| s != CheckStatus::Pending) && card.ci_awaited_since.is_some();
        let mergeable = mergeable.unwrap_or(card.mergeable);
        let card = if reviews != card.reviews
            || by_reviewer != card.reviewer_comment_count
            || total != card.comment_count
            || unanswered != card.unanswered_count
            || checks != card.checks
            || mergeable != card.mergeable
            || clear_awaited
        {
            let updated = self.store.mutate_card(card_id, |c| {
                c.reviews = reviews;
                c.reviewer_comment_count = by_reviewer;
                c.comment_count = total;
                c.unanswered_count = unanswered;
                c.checks = checks;
                c.mergeable = mergeable;
                if clear_awaited {
                    c.ci_awaited_since = None;
                }
                Ok(())
            })?;
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::updated(updated.clone()));
            updated
        } else {
            card
        };
        // Same auto-advance the poll does, so ↻ carries an approved-and-clear
        // card to the merge gate now instead of on the next tick.
        if card.approval_clears_merge() {
            self.apply(card_id, Transition::ReviewApproved)?;
            self.progress(card_id, "✔ approved with no comments — ready to merge");
        } else if card.no_reviewer_clears_merge(reviewer.as_deref()) {
            self.apply(card_id, Transition::ReviewApproved)?;
            self.progress(card_id, "✔ no reviewer assigned — ready to merge");
        }
        Ok(())
    }

    /// Mark every pending review *body* on the card handled — the panel's
    /// "Mark as read", for a body-only review (e.g. a bot's pass report) the
    /// user has read and doesn't need an agent run for. Purely local: nothing
    /// is posted to the forge. The next poll tick (or ↻) re-runs the
    /// auto-advance predicates, which no longer see the bodies as pending.
    pub(super) fn mark_review_bodies_read(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let keys: Vec<String> = card
            .pending_review_bodies()
            .into_iter()
            .map(|r| r.body_key())
            .collect();
        if keys.is_empty() {
            return Ok(());
        }
        let updated = self.store.mutate_card(card_id, |c| {
            for k in keys {
                if !c.triaged_review_bodies.contains(&k) {
                    c.triaged_review_bodies.push(k);
                }
            }
            c.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        Ok(())
    }

    /// Flip the card's draft PR to ready-for-review (after the user has added any
    /// screenshots and finished the description on GitHub).
    pub(super) async fn mark_pr_ready(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let pr = card
            .pr
            .as_ref()
            .map(|p| p.number)
            .ok_or_else(|| CoreError::other("card has no PR to mark ready"))?;
        self.forge.mark_ready(&project.path, pr).await?;
        let updated = self.store.mutate_card(card_id, |c| {
            if let Some(pr) = &mut c.pr {
                pr.state = "open".into();
            }
            c.updated_at = now_millis();
            Ok(())
        })?;
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            card_id,
            Severity::Success,
            "PR marked ready for review",
        ));
        Ok(())
    }

    /// Reprompt a card whose PR is already open with a free-form change, run in an
    /// isolated worktree on the card's branch (never the main working copy), then
    /// commit + push to update the PR. Legal from both `ReadyToMerge` (loops back
    /// to merge) and `PrReview(Idle)` (the freshly-opened PR, before any comment
    /// triage — loops back to the PR gate); the state machine routes each.
    pub(super) async fn request_post_pr_change(
        &self,
        card_id: Uuid,
        feedback: String,
    ) -> Result<()> {
        // Keep the request so a later "back to start" folds it into the prompt,
        // matching the pre-PR `revise`/`apply_fixes` paths.
        self.record_qa(card_id, format!("Requested change: {}", feedback.trim()));
        let extra = format!(
            "A follow-up change was requested on this pull request. Update the changes in \
             this branch accordingly:\n\n{}",
            feedback.trim()
        );
        // Isolate the fix before entering the running state (a failure here leaves
        // the card recoverable in its current state — ReadyToMerge or the PR gate).
        self.ensure_branch_worktree(card_id).await?;
        // Stash the task before entering the running state, so a retry of a
        // faulted run can restate it (see `relaunch`).
        self.store.set_fix_extra(card_id, Some(&extra))?;
        let card = self.apply(card_id, Transition::RequestPostPrChange)?;
        self.launch(card, RunMode::ApplyFixes, Some(extra), None)
            .await
    }

    /// Squash-merge the card's PR and tear down what it leaves behind.
    ///
    /// The merge is the only step allowed to fail the card. Everything after it
    /// is cleanup of local state, and once GitHub has merged the PR no local
    /// failure can un-merge it — so a failed cleanup must never keep the card
    /// out of `Done`, or it strands a card whose work is already on the base
    /// branch. (It used to: `gh pr merge --delete-branch` deletes the *local*
    /// branch itself, which git refuses while the card's worktree has it checked
    /// out, and gh's non-zero exit aborted both the transition and the worktree
    /// removal that would have unblocked the next attempt.)
    ///
    /// So: merge bare, transition, then remove the worktree to free the branch,
    /// and only then delete it locally and on the remote.
    ///
    /// Before any of that, the PR's CI checks are re-read from the forge — the
    /// authoritative gate, since the card's cached status can be a poll interval
    /// stale. A red or still-running build blocks the merge (leaving the card in
    /// `ReadyToMerge` with an offer: fix with an agent, or wait); `force` is the
    /// user's explicit "merge anyway" and skips only this pre-check. No checks
    /// configured, or an error *reading* them, doesn't block — a protected
    /// branch still guards server-side. An already-merged PR isn't gated
    /// either: red checks on a merged PR are moot, and the card must still
    /// reach `Done`.
    pub(super) async fn merge(
        &self,
        card_id: Uuid,
        delete_branch: bool,
        force: bool,
    ) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let pr_number = card
            .pr
            .as_ref()
            .map(|p| p.number)
            .ok_or_else(|| CoreError::other("card has no PR to merge"))?;

        // Both merge gates reach here: `ReadyToMerge` (the review cleared it)
        // and `PrReview(Idle)` (the last-resort merge without review). Anything
        // else is a stale panel — the card moved on under it — and merging
        // would land the PR on the forge before failing the transition, leaving
        // the card behind its own merged PR.
        if !matches!(
            card.state,
            CardState::ReadyToMerge | CardState::PrReview(PrReviewSub::Idle)
        ) {
            return Err(CoreError::other("the card is not at a merge gate"));
        }

        if !force {
            if let Ok((read, failed)) = self.forge.pr_checks(&project.path, pr_number).await {
                // An empty rollup inside the registration grace still means "the
                // build is starting", so a merge asked for in that window is
                // refused with the usual "still running" toast rather than
                // landing ahead of CI. `force` (Merge anyway) never gets here.
                let status = card.settle_checks(read, now_millis());
                self.persist_checks(card_id, status);
                self.learn_ci_on_prs(&project, read, status);
                // Red or pending checks gate the merge — but only a PR that
                // still NEEDS merging. One merged on GitHub directly while its
                // checks were red (or still running) must fall through to the
                // already-merged path below: gating it would offer a fix run
                // against a merged (possibly deleted) branch, or tell the user
                // to wait for a green that will never come, and the card could
                // never reach `Done` via Merge.
                if matches!(status, CheckStatus::Failing | CheckStatus::Pending)
                    && !self
                        .forge
                        .is_merged(&project.path, pr_number)
                        .await
                        .unwrap_or(false)
                {
                    if status == CheckStatus::Failing {
                        // Not an error: a fixable state, like a merge conflict.
                        // Offer the agent fix and leave the card at the gate.
                        let names = failed.iter().map(|f| f.name.clone()).collect();
                        let _ = self.evt_tx.unbounded_send(ExecutorEvent::checks_failed(
                            card_id, pr_number, names,
                        ));
                    } else {
                        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                            card_id,
                            Severity::Warning,
                            "CI checks are still running — merge once they're green, \
                             or use Merge anyway"
                                .to_string(),
                        ));
                    }
                    return Ok(());
                }
            }
        }

        // A PR merged by an earlier attempt (or on GitHub directly) can't be
        // merged again — but the card still needs to reach `Done` and shed its
        // worktree. Ask the forge rather than matching on gh's error text.
        if let Err(e) = self.forge.merge(&project.path, pr_number).await {
            if !self
                .forge
                .is_merged(&project.path, pr_number)
                .await
                .unwrap_or(false)
            {
                // A conflict with the base branch isn't a failure the user should
                // read as an error: it's a fixable state, and the agent that wrote
                // the branch can resolve it. Offer that instead, leaving the card
                // in `ReadyToMerge` to merge again once the branch is updated.
                if self.pr_conflicts(&project.path, pr_number).await {
                    // Gate the board's merge button right away — waiting for the
                    // next poll tick would leave it offering the merge that just
                    // failed.
                    self.persist_mergeable(card_id, Mergeable::Conflicting);
                    let _ = self.evt_tx.unbounded_send(ExecutorEvent::merge_conflict(
                        card_id,
                        pr_number,
                        project.config.effective_base_branch().to_string(),
                    ));
                    return Ok(());
                }
                return Err(e);
            }
        }
        // The PR is merged on the forge at this point; stamp the record so the
        // card's own panel doesn't read "open" forever (the poll never revisits
        // a `Done` card, so nothing else would). Before `apply` so the
        // transition's `CardUpdated` carries both changes in one event.
        self.store.mutate_card(card_id, |c| {
            if let Some(p) = &mut c.pr {
                p.state = "merged".into();
            }
            Ok(())
        })?;
        self.apply(card_id, Transition::Merge)?;

        // Past this point the PR is merged: report cleanup problems, never raise
        // them.
        let (worktree_gone, mut left_behind) =
            self.cleanup_terminal_pr_worktree(card_id, false).await;
        if delete_branch {
            if let Some(branch) = &card.branch {
                // Deleting the local branch can only work once nothing has it
                // checked out; don't even try if its worktree is still there.
                if worktree_gone {
                    if let Err(e) = self.git.delete_branch(&project.path, branch).await {
                        left_behind.push(format!("local branch ({e})"));
                    }
                } else {
                    left_behind.push("local branch (worktree still holds it)".to_string());
                }
                if let Err(e) = self.forge.delete_remote_branch(&project.path, branch).await {
                    left_behind.push(format!("remote branch ({e})"));
                }
            }
        }

        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            card_id,
            Severity::Success,
            "Merged 🎉".to_string(),
        ));
        if !left_behind.is_empty() {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Warning,
                format!("Merged, but couldn't clean up: {}", left_behind.join("; ")),
            ));
        }
        Ok(())
    }

    /// Local cleanup once a card's PR is terminal on the forge (merged there by
    /// us or by anyone, or closed): stop the preview, remove the worktree, and
    /// clear the card's `worktree_path` once it is actually gone. The ordering
    /// is load-bearing — a preview left running keeps writing into the worktree
    /// (vite's dep cache, the backend's `dist`), which races `git worktree
    /// remove` and leaves it behind as "Directory not empty"; and the worktree
    /// must go before any branch deletion, since it holds the branch. Branch
    /// deletion itself is deliberately NOT here: the reconciliation paths never
    /// delete branches. Best-effort throughout — returns whether the worktree
    /// is gone, plus what was left behind for the caller's warning.
    ///
    /// `keep_if_dirty` is for the background reconcile: the card menu offers
    /// opening the worktree in a terminal/editor while parked at the PR gates,
    /// so a poll tick noticing the PR went terminal on GitHub must not
    /// force-remove uncommitted edits the user may have in there. A dirty tree
    /// is then kept in place (path and all) and reported instead of destroyed.
    /// The user-initiated merge passes `false` — they asked for the teardown.
    pub(super) async fn cleanup_terminal_pr_worktree(
        &self,
        card_id: Uuid,
        keep_if_dirty: bool,
    ) -> (bool, Vec<String>) {
        let _ = self.stop_preview(card_id).await;
        let Ok(card) = self.store.get_card(card_id) else {
            return (false, Vec::new());
        };
        let Ok(project) = self.store.get_project(card.project_id) else {
            return (false, Vec::new());
        };
        let mut left_behind = Vec::new();
        let mut worktree_gone = true;
        if let Some(worktree) = &card.worktree_path {
            // An unreadable tree (already removed by hand, permissions) answers
            // "not dirty" and falls through to the removal attempt, which
            // reports its own failure.
            if keep_if_dirty && is_dirty(worktree).await.unwrap_or(false) {
                worktree_gone = false;
                left_behind.push("worktree (kept — it has uncommitted changes)".to_string());
            } else if let Err(e) = self.remove_worktree_retrying(&project.path, worktree).await {
                worktree_gone = false;
                left_behind.push(format!("worktree ({e})"));
            }
        }
        // Drop the path only once the worktree is actually gone, so a card that
        // still has one on disk keeps pointing at it.
        if worktree_gone && card.worktree_path.is_some() {
            if let Ok(updated) = self.store.mutate_card(card_id, |c| {
                c.worktree_path = None;
                c.updated_at = now_millis();
                Ok(())
            }) {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
            }
        }
        (worktree_gone, left_behind)
    }

    /// Reconcile a card's local PR state with what the forge reported live.
    /// Returns `true` when the card moved to a terminal column (the caller must
    /// stop processing it — its counts and auto-advances no longer apply).
    ///
    /// Runs *before* the count refresh and the `approval_clears_merge`
    /// auto-advance on every poll tick and manual ↻, so a merged PR can't ride
    /// `Idle → ReadyToMerge` on the same tick it should be retiring on.
    /// Only the two parked PR gates are reconciled; any other state (mid-triage,
    /// mid-fix) is left for its run to finish — the next tick catches it.
    /// `None` (the forge can't tell, or the read failed upstream) changes
    /// nothing: absence of an answer is never treated as "closed".
    pub(super) async fn reconcile_pr_live_state(
        &self,
        card_id: Uuid,
        live: Option<LivePrState>,
    ) -> Result<bool> {
        let Some(live) = live else {
            return Ok(false);
        };
        let card = self.store.get_card(card_id)?;
        let Some(pr) = card.pr.clone() else {
            return Ok(false);
        };
        let sync_pr_state = |target: &str| -> Result<()> {
            if pr.state != target {
                let updated = self.store.mutate_card(card_id, |c| {
                    if let Some(p) = &mut c.pr {
                        p.state = target.to_string();
                    }
                    Ok(())
                })?;
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
            }
            Ok(())
        };
        match live {
            // Bonus fix: a draft marked ready (or flipped back) on GitHub keeps
            // the card's badge honest without moving anything.
            LivePrState::Open { draft } => {
                sync_pr_state(if draft { "draft" } else { "open" })?;
                Ok(false)
            }
            LivePrState::Merged => {
                if !matches!(
                    card.state,
                    CardState::PrReview(PrReviewSub::Idle) | CardState::ReadyToMerge
                ) {
                    return Ok(false);
                }
                sync_pr_state("merged")?;
                let (message, transition) = if matches!(card.state, CardState::ReadyToMerge) {
                    // Review passed — the external merge finishes the card the
                    // same way our own merge would.
                    (
                        format!("PR #{} was merged on GitHub — marked done", pr.number),
                        Transition::Merge,
                    )
                } else {
                    (
                        format!(
                            "PR #{} was merged on GitHub before its review finished",
                            pr.number
                        ),
                        Transition::PrMergedExternally,
                    )
                };
                self.apply(card_id, transition)?;
                let (_, left_behind) = self.cleanup_terminal_pr_worktree(card_id, true).await;
                self.toast_reconciled(card_id, message, left_behind);
                Ok(true)
            }
            LivePrState::Closed => {
                if !matches!(
                    card.state,
                    CardState::PrReview(PrReviewSub::Idle) | CardState::ReadyToMerge
                ) {
                    return Ok(false);
                }
                sync_pr_state("closed")?;
                self.apply(card_id, Transition::PrClosedExternally)?;
                let (_, left_behind) = self.cleanup_terminal_pr_worktree(card_id, true).await;
                self.toast_reconciled(
                    card_id,
                    format!("PR #{} was closed on GitHub without merging", pr.number),
                    left_behind,
                );
                Ok(true)
            }
        }
    }

    /// The reconciliation's outcome toast, with any cleanup leftovers appended
    /// as a second warning (mirroring `merge`'s reporting).
    fn toast_reconciled(&self, card_id: Uuid, message: String, left_behind: Vec<String>) {
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::toast(card_id, Severity::Info, message));
        if !left_behind.is_empty() {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Warning,
                format!("Couldn't clean up: {}", left_behind.join("; ")),
            ));
        }
    }

    /// Whether the PR was refused because it conflicts with its base.
    ///
    /// GitHub recomputes mergeability asynchronously, so a PR pushed moments ago
    /// answers `UNKNOWN` for a beat; poll briefly rather than mistaking "not
    /// computed yet" for "no conflict". Only a definite `CONFLICTING` claims the
    /// conflict — every other answer (and any error reaching the forge) leaves
    /// the original merge error to surface as itself.
    async fn pr_conflicts(&self, repo: &Path, pr_number: u64) -> bool {
        for attempt in 0..MERGEABILITY_ATTEMPTS {
            match self.forge.merge_status(repo, pr_number).await {
                Ok(Mergeable::Conflicting) => return true,
                Ok(Mergeable::Unknown) => {}
                Ok(Mergeable::Clean) | Err(_) => return false,
            }
            if attempt + 1 < MERGEABILITY_ATTEMPTS {
                tokio::time::sleep(MERGEABILITY_POLL).await;
            }
        }
        false
    }

    /// Resolve the PR's conflicts with its base branch, with an agent.
    ///
    /// The base is merged into the card's branch inside the card's ISOLATED
    /// worktree — never the user's working copy — which leaves that worktree
    /// mid-merge with the conflict markers in place. The agent then resolves them
    /// exactly as a human would, and `finalize_run` commits (completing the merge)
    /// and pushes. The card loops back to `ReadyToMerge` for the user to merge
    /// again.
    pub(super) async fn resolve_conflicts(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let base = project.config.effective_base_branch().to_string();
        let branch = card
            .branch
            .clone()
            .ok_or_else(|| CoreError::other("card has no branch to resolve conflicts on"))?;
        let pr_number = card
            .pr
            .as_ref()
            .map(|p| p.number)
            .ok_or_else(|| CoreError::other("card has no PR to resolve conflicts on"))?;

        // Re-read rather than trusting the card's cached snapshot (mirrors
        // `fix_checks`): the base may have moved back, or a teammate updated the
        // branch, since the poll recorded `Conflicting`. A definite `Clean`
        // no-ops — merging the base in anyway would push a pointless merge
        // commit and re-trigger CI on a PR that was already mergeable. Anything
        // less definite (still conflicting, not yet computed, or a forge error)
        // proceeds: the local merge below finds the real answer either way.
        if let Ok(Mergeable::Clean) = self.forge.merge_status(&project.path, pr_number).await {
            self.persist_mergeable(card_id, Mergeable::Clean);
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Success,
                "No conflicts left — the PR merges cleanly. Try merging again.",
            ));
            return Ok(());
        }

        // Everything up to the transition is recoverable: a failure here leaves the
        // card sitting in `ReadyToMerge`, where the user can try again.
        self.ensure_branch_worktree(card_id).await?;
        let dir = self
            .store
            .get_card(card_id)?
            .worktree_path
            .ok_or_else(|| CoreError::other("card has no worktree to resolve conflicts in"))?;

        self.progress(card_id, &format!("Fetching origin and merging {base}…"));
        self.git.fetch(&dir, "origin").await?;
        let files = match self.git.merge_ref(&dir, &format!("origin/{base}")).await? {
            // The conflict resolved itself (the base moved again, or someone
            // updated the branch). Publish the merge and let the user retry.
            MergeOutcome::Clean => {
                self.git.push(&dir, &branch).await?;
                // The push invalidates the whole cached PR snapshot — this path
                // skips finalize_run, so mirror its post-push reset here. The
                // mergeability is stale (the merge commit just cured it), and so
                // are the checks: the push re-triggers CI, and a leftover
                // `Passing` would re-show Merge only for the executor to refuse
                // it with "CI checks are still running".
                let expects_ci = project.expects_ci();
                let has_checks = expects_ci || card.checks != CheckStatus::None;
                if let Ok(updated) = self.store.mutate_card(card_id, |c| {
                    mark_ci_in_flight(c, expects_ci);
                    c.mergeable = Mergeable::Unknown;
                    Ok(())
                }) {
                    let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
                }
                let message = if has_checks {
                    format!(
                        "No conflicts left — merged {base} into the branch. \
                         Merge again once CI passes."
                    )
                } else {
                    format!("No conflicts left — merged {base} into the branch. Try merging again.")
                };
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                    card_id,
                    Severity::Success,
                    message,
                ));
                return Ok(());
            }
            MergeOutcome::Conflicted(files) => files,
        };

        self.progress(
            card_id,
            &format!(
                "{} conflicted file(s) — asking the agent to resolve them…",
                files.len()
            ),
        );
        let extra = conflict_prompt(&base, &files);
        // Stash the task before entering the running state, so a retry of a
        // faulted run can restate it (see `relaunch`). Without this, a retried
        // conflict run has no idea a merge is in progress.
        self.store.set_fix_extra(card_id, Some(&extra))?;
        let card = self.apply(card_id, Transition::RequestPostPrChange)?;
        self.launch(card, RunMode::ApplyFixes, Some(extra), None)
            .await
    }

    /// Record a freshly-read check status on the card (best-effort — a persist
    /// failure must not derail the merge or fix flow that read it). Deliberately
    /// doesn't bump `updated_at`, matching the background poll: observing CI
    /// isn't a user-facing edit and shouldn't reorder the board.
    fn persist_checks(&self, card_id: Uuid, status: CheckStatus) {
        let Ok(card) = self.store.get_card(card_id) else {
            return;
        };
        if card.checks == status {
            return;
        }
        if let Ok(updated) = self.store.mutate_card(card_id, |c| {
            c.checks = status;
            if status != CheckStatus::Pending {
                // The rollup answered (or the grace ran out): stop treating a
                // future empty read as "not registered yet".
                c.ci_awaited_since = None;
            }
            Ok(())
        }) {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        }
    }

    /// Remember whether this project's PRs get CI checks (see
    /// [`ProjectConfig::ci_checks`](crate::domain::config::ProjectConfig::ci_checks)),
    /// from what a rollup read just showed. A reported status proves yes. An
    /// empty rollup that *settled* empty — i.e. survived the registration grace —
    /// proves no, but only while we had no better evidence: a project that has
    /// produced checks before stays `Some(true)`, since one PR touching no
    /// workflow's paths doesn't mean the project has no CI. Best-effort and
    /// silent: this only sharpens an optimistic guess.
    pub(super) fn learn_ci_on_prs(
        &self,
        project: &Project,
        read: CheckStatus,
        settled: CheckStatus,
    ) {
        let want = if read != CheckStatus::None {
            Some(true)
        } else if settled == CheckStatus::None && project.config.ci_checks.is_none() {
            Some(false)
        } else {
            return;
        };
        let Ok(mut stored) = self.store.get_project(project.id) else {
            return;
        };
        if stored.config.ci_checks == want {
            return;
        }
        stored.config.ci_checks = want;
        if self.store.upsert_project(&stored).is_ok() {
            let _ = self
                .evt_tx
                .unbounded_send(ExecutorEvent::project_upserted(stored));
        }
    }

    /// Background loop: every [`CI_POLL_INTERVAL`], re-read the rollup of every
    /// card whose build is in flight at the PR or merge gate. The 5-minute
    /// review poll already refreshes checks, but that is far too slow for the
    /// thing this gates — a card at the merge gate stays badge-less and
    /// Merge-button-less while `checks == Pending`, so the wait to learn the
    /// build went green *is* the wait to be told the card is ready.
    ///
    /// Costs nothing at rest: a tick with no in-flight card makes no `gh` call.
    /// The first tick fires immediately, so a card left `Pending` across a
    /// restart settles right away.
    pub(super) async fn ci_poll_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(CI_POLL_INTERVAL);
        loop {
            interval.tick().await;
            let projects = match self.store.list_projects() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("CI poll: could not list projects: {e}");
                    continue;
                }
            };
            for project in projects {
                if let Err(e) = self.poll_ci(&project).await {
                    tracing::warn!("CI poll for {} failed: {e}", project.name);
                }
            }
        }
    }

    /// One CI-poll tick for one project. Best-effort per card, like the
    /// comment poll: a forge error skips the card and keeps its last known
    /// status, so a broken `gh` can't flip a gate.
    async fn poll_ci(&self, project: &Project) -> Result<()> {
        for card in self.store.list_cards_for_project(project.id)? {
            if !ci_polled(&card) {
                continue;
            }
            let Some(pr_number) = card.pr.as_ref().map(|p| p.number) else {
                continue;
            };
            let read = match self.forge.pr_checks(&project.path, pr_number).await {
                Ok((status, _)) => status,
                Err(e) => {
                    tracing::warn!("CI poll: reading checks of #{pr_number} failed: {e}");
                    continue;
                }
            };
            let settled = card.settle_checks(read, now_millis());
            self.learn_ci_on_prs(project, read, settled);
            // Still in flight (or still inside the grace) — nothing to write.
            if settled == card.checks {
                continue;
            }
            // The read above ran unlocked, so the executor may have rewritten
            // the card meanwhile (a push re-armed `Pending`, Back to Start
            // cleared the PR). Same discipline as the comment poll: inside the
            // atomic mutate, skip a card that left the polled states, swapped
            // PRs, or had its checks touched since the snapshot — and don't bump
            // `updated_at`, observing CI isn't a user-facing edit.
            let mut changed = false;
            let updated = self.store.mutate_card(card.id, |c| {
                if c.pr.as_ref().map(|p| p.number) != Some(pr_number)
                    || !ci_polled(c)
                    || c.checks != card.checks
                    || c.ci_awaited_since != card.ci_awaited_since
                {
                    return Ok(());
                }
                changed = c.checks != settled;
                c.checks = settled;
                if settled != CheckStatus::Pending {
                    changed |= c.ci_awaited_since.is_some();
                    c.ci_awaited_since = None;
                }
                Ok(())
            })?;
            if changed {
                let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
            }
        }
        Ok(())
    }

    /// Record a freshly-learned mergeability on the card. Same contract as
    /// [`Self::persist_checks`]: best-effort, and no `updated_at` bump.
    fn persist_mergeable(&self, card_id: Uuid, mergeable: Mergeable) {
        let Ok(card) = self.store.get_card(card_id) else {
            return;
        };
        if card.mergeable == mergeable {
            return;
        }
        if let Ok(updated) = self.store.mutate_card(card_id, |c| {
            c.mergeable = mergeable;
            Ok(())
        }) {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        }
    }

    /// Fix the PR's failing CI checks with an agent, in the card's isolated
    /// worktree. Mirrors [`Self::resolve_conflicts`]: everything up to the
    /// transition is recoverable (the card stays in `ReadyToMerge`), and if the
    /// checks turned green in the meantime no run is spent. The failed runs'
    /// logs are fetched best-effort as prompt context; the run's completion
    /// commits + pushes, which re-triggers CI, and the card loops back to
    /// `ReadyToMerge`.
    pub(super) async fn fix_checks(&self, card_id: Uuid) -> Result<()> {
        let card = self.store.get_card(card_id)?;
        let project = self.store.get_project(card.project_id)?;
        let pr_number = card
            .pr
            .as_ref()
            .map(|p| p.number)
            .ok_or_else(|| CoreError::other("card has no PR to fix checks on"))?;

        // Re-read rather than trusting the dialog's snapshot: a re-run or a
        // teammate's push may have gone green since, and a run is not free.
        let (status, failed) = self.forge.pr_checks(&project.path, pr_number).await?;
        self.persist_checks(card_id, status);
        if status != CheckStatus::Failing {
            let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
                card_id,
                Severity::Success,
                format!(
                    "No failing checks left ({}) — try merging again.",
                    status.label()
                ),
            ));
            return Ok(());
        }

        self.ensure_branch_worktree(card_id).await?;

        // Pull the failed runs' logs for the prompt — best-effort: a check
        // whose URL isn't a GitHub Actions run (or a failed fetch) just means
        // less context, and the agent can dig with `gh` itself.
        self.progress(card_id, "Fetching failing checks' logs…");
        let mut logs: Vec<(String, String)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for check in &failed {
            let Some(run_id) = run_id_from_url(&check.url) else {
                continue;
            };
            if !seen.insert(run_id) {
                continue;
            }
            match self.forge.failed_run_log(&project.path, run_id).await {
                Ok(log) if !log.trim().is_empty() => {
                    let name = if check.workflow.is_empty() {
                        check.name.clone()
                    } else {
                        check.workflow.clone()
                    };
                    logs.push((name, log_tail(&log, 200, 16 * 1024)));
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("fix checks: couldn't fetch log of run {run_id}: {e}");
                }
            }
        }

        self.progress(
            card_id,
            &format!(
                "{} failing check(s) — asking the agent to fix them…",
                failed.len()
            ),
        );
        let extra = checks_fix_prompt(pr_number, &failed, &logs);
        // Stash the task before entering the running state, so a retry of a
        // faulted run can restate it (see `relaunch`).
        self.store.set_fix_extra(card_id, Some(&extra))?;
        let card = self.apply(card_id, Transition::RequestPostPrChange)?;
        self.launch(card, RunMode::ApplyFixes, Some(extra), None)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::CardConfig;
    use uuid::Uuid;

    fn card_with(checks: CheckStatus) -> Card {
        let mut card = Card::new(Uuid::new_v4(), "t", "d", CardConfig::default());
        card.state = CardState::ReadyToMerge;
        card.checks = checks;
        card
    }

    /// A push on a CI project always re-arms the gate, grace stamped.
    #[test]
    fn a_ci_project_is_always_re_armed_with_the_grace() {
        for seen in [
            CheckStatus::None,
            CheckStatus::Passing,
            CheckStatus::Failing,
        ] {
            let mut card = card_with(seen);
            mark_ci_in_flight(&mut card, true);
            assert_eq!(card.checks, CheckStatus::Pending, "from {seen:?}");
            assert!(card.ci_awaited_since.is_some(), "from {seen:?}");
        }
    }

    /// A project we believe has no CI, on a card that has nonetheless seen a
    /// status: the re-arm must stamp the grace too, or the next empty rollup
    /// settles it straight back to green while the build is still registering.
    #[test]
    fn a_re_armed_card_without_a_ci_project_still_gets_the_grace() {
        let mut card = card_with(CheckStatus::Passing);
        mark_ci_in_flight(&mut card, false);
        assert_eq!(card.checks, CheckStatus::Pending);
        let stamped = card.ci_awaited_since.expect("the grace must be stamped");
        assert_eq!(
            card.settle_checks(CheckStatus::None, stamped + 1),
            CheckStatus::Pending,
            "an empty rollup inside the grace still means the build is starting"
        );
        assert_eq!(
            card.settle_checks(
                CheckStatus::None,
                stamped + crate::CI_REGISTER_GRACE.as_millis() as i64 + 1
            ),
            CheckStatus::None,
            "…and past it, the honest answer returns"
        );
    }

    /// A card that has never seen a check on a project without CI keeps its
    /// honest `None`: nothing to wait for, so no grace either.
    #[test]
    fn a_card_that_never_had_checks_is_left_alone() {
        let mut card = card_with(CheckStatus::None);
        mark_ci_in_flight(&mut card, false);
        assert_eq!(card.checks, CheckStatus::None);
        assert_eq!(card.ci_awaited_since, None);
    }
}
