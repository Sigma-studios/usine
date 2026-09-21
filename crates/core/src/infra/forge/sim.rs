//! A simulated forge: canned PRs, comments and reviews, so the PR-review
//! column and the review board are fully navigable in demo mode and tests
//! without any code host.

use std::path::Path;

use async_trait::async_trait;

use super::{normalize_reviewer, Forge, PrPushTarget, PrSummary, ReviewScope};
use crate::domain::model::{
    CheckStatus, DraftComment, Mergeable, PrInfo, PrState, ReviewComment, ReviewEvent,
    ReviewSummary, ReviewThread,
};
use crate::error::Result;

/// Simulated forge for Phase A: canned PR + review comments so the PR-review
/// column is fully navigable without GitHub.
pub struct SimForge;

#[async_trait]
impl Forge for SimForge {
    async fn create_pr(
        &self,
        _repo: &Path,
        title: &str,
        _body: &str,
        _base: &str,
        _head: &str,
        reviewer: Option<&str>,
        draft: bool,
    ) -> Result<PrInfo> {
        Ok(PrInfo {
            number: 42,
            url: "https://github.com/example/repo/pull/42".to_string(),
            title: title.to_string(),
            state: PrState::open(draft),
            reviewer: normalize_reviewer(reviewer),
            reviewer_recorded: true,
        })
    }

    async fn fetch_comments(&self, _repo: &Path, _pr_number: u64) -> Result<Vec<ReviewComment>> {
        Ok(vec![
            ReviewComment {
                id: 1,
                author: "reviewer".into(),
                path: "src/lib.rs".into(),
                line: Some(12),
                body: "Consider extracting this into a helper function.".into(),
                review_body_of: None,
            },
            ReviewComment {
                id: 2,
                author: "reviewer".into(),
                path: "src/main.rs".into(),
                line: Some(48),
                body: "Nit: typo in this comment.".into(),
                review_body_of: None,
            },
            ReviewComment {
                id: 3,
                author: "reviewer".into(),
                path: "src/db.rs".into(),
                line: Some(5),
                body: "This `unwrap()` could panic on malformed input.".into(),
                review_body_of: None,
            },
        ])
    }

    async fn list_review_prs(&self, _repo: &Path, scope: ReviewScope) -> Result<Vec<PrSummary>> {
        let mut prs = vec![
            PrSummary {
                number: 101,
                title: "Add caching layer".into(),
                author: "octocat".into(),
                head_ref: "feat/cache".into(),
                base_ref: "main".into(),
                url: "https://github.com/example/repo/pull/101".into(),
                body: "Adds an LRU in front of the resolver so repeated lookups \
                       stop hitting the database.\n\n- bounded at 10k entries\n\
                       - invalidated on write"
                    .into(),
                checks: CheckStatus::Passing,
                mergeable: Mergeable::Clean,
            },
            PrSummary {
                number: 102,
                title: "Fix flaky integration test".into(),
                author: "hubot".into(),
                head_ref: "fix/flaky".into(),
                base_ref: "main".into(),
                url: "https://github.com/example/repo/pull/102".into(),
                body: "The fixture raced the seeder; awaits it explicitly now.".into(),
                checks: CheckStatus::Failing,
                mergeable: Mergeable::Conflicting,
            },
        ];
        // "Everyone" mode must be visibly different in the simulator: a PR by
        // someone who is *not* a collaborator, i.e. exactly the fork
        // contributor the pinned-author path can never reach.
        if scope == ReviewScope::Everyone {
            prs.push(PrSummary {
                number: 103,
                title: "Typo in the onboarding guide".into(),
                author: "outside-contributor".into(),
                head_ref: "docs/typo".into(),
                base_ref: "main".into(),
                url: "https://github.com/example/repo/pull/103".into(),
                body: "Drive-by fix from a fork — the author isn't a repo collaborator.".into(),
                checks: CheckStatus::Passing,
                mergeable: Mergeable::Clean,
            });
        }
        Ok(prs)
    }

    async fn submit_review(
        &self,
        _repo: &Path,
        _pr_number: u64,
        _event: ReviewEvent,
        _body: &str,
        _comments: &[DraftComment],
    ) -> Result<()> {
        Ok(())
    }

    /// A same-repo, pushable head, so "publish & fix" runs end to end in the sim.
    async fn pr_push_target(&self, _repo: &Path, pr_number: u64) -> Result<Option<PrPushTarget>> {
        Ok(Some(PrPushTarget {
            head_ref: format!("sim/pr-{pr_number}"),
            cross_repo: false,
            head_repo: String::new(),
            maintainer_can_modify: true,
        }))
    }

    async fn list_reviewers(&self, _repo: &Path) -> Result<Vec<String>> {
        Ok(vec!["octocat".into(), "hubot".into(), "monalisa".into()])
    }

    async fn list_pr_authors(&self, _repo: &Path) -> Result<Vec<String>> {
        // `outside-contributor` and `drive-by` are deliberately absent from
        // `list_reviewers` — they're what the picker gains over collaborators.
        Ok(vec![
            "octocat".into(),
            "outside-contributor".into(),
            "drive-by".into(),
        ])
    }

    async fn list_submitted_reviews(
        &self,
        _repo: &Path,
        _pr_number: u64,
    ) -> Result<Vec<ReviewSummary>> {
        Ok(vec![
            ReviewSummary::new("octocat", "CHANGES_REQUESTED"),
            // A body-only review — the bot-report shape: no inline comments,
            // the whole report in the summary text.
            ReviewSummary {
                author: "gemini-code-assist".into(),
                state: "COMMENTED".into(),
                body: "## Review summary\n\nThe change looks reasonable overall. \
                       One concern: the retry loop has no backoff, which could \
                       hammer the endpoint under sustained failure."
                    .into(),
                submitted_at: "2026-01-01T00:00:00Z".into(),
            },
        ])
    }

    async fn reply_to_comment(
        &self,
        _repo: &Path,
        _pr_number: u64,
        _comment_id: u64,
        _body: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn mark_ready(&self, _repo: &Path, _pr_number: u64) -> Result<()> {
        Ok(())
    }

    async fn merge(&self, _repo: &Path, _pr_number: u64) -> Result<()> {
        Ok(())
    }

    async fn is_merged(&self, _repo: &Path, _pr_number: u64) -> Result<bool> {
        Ok(true)
    }

    async fn merge_status(&self, _repo: &Path, _pr_number: u64) -> Result<Mergeable> {
        Ok(Mergeable::Clean)
    }

    async fn delete_remote_branch(&self, _repo: &Path, _branch: &str) -> Result<()> {
        Ok(())
    }

    async fn resolve_threads(
        &self,
        _repo: &Path,
        _pr_number: u64,
        _comment_ids: &[u64],
    ) -> Result<usize> {
        Ok(0)
    }

    async fn list_threads(&self, _repo: &Path, _pr_number: u64) -> Result<Vec<ReviewThread>> {
        // One unresolved, reviewer-last thread per canned comment, so the sim
        // triage sees exactly the comments `fetch_comments` returns.
        Ok((1..=3)
            .map(|id| ReviewThread {
                id: format!("SIMTHREAD_{id}"),
                resolved: false,
                comment_ids: vec![id],
                last_by_viewer: false,
            })
            .collect())
    }
}
