//! Forge integration: the code host a project's pull requests live on.
//!
//! [`Forge`] is the port the executor drives for everything past `git push` —
//! opening PRs, reading reviews, comments, CI and mergeability, replying,
//! resolving, merging, reviewing other people's PRs. Implementations:
//! - [`GhForge`] — GitHub, through the `gh` CLI (reusing the user's `gh auth`);
//! - [`AzureForges`] — Azure DevOps Services, over its REST API (a PAT from the
//!   environment, or the `az` CLI's login);
//! - [`SimForge`] — canned data for demo mode and tests.
//!
//! Which one a project uses is its [`ForgeKind`] — detected from `origin`
//! ([`parse_remote`]) or pinned in settings — and [`ForgeRegistry`] resolves it
//! per call, so projects on different hosts coexist on one board.
//!
//! The trait speaks one vocabulary for both hosts. A few conventions carry the
//! weight of that:
//! - A [`ReviewComment::id`] is an opaque handle only the forge that issued it
//!   interprets (Azure packs a thread id and a comment id into it). It must
//!   stay below 2^53: ids round-trip through the triage agent's JSON.
//! - Review verdicts use the GitHub strings (`APPROVED`, `CHANGES_REQUESTED`,
//!   `COMMENTED`); other forges map onto them (see [`ReviewSummary`]).
//! - Handles (reviewers, authors) are whatever identifies a person on the host
//!   — a GitHub login, an Azure DevOps email — compared case-insensitively.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::config::ForgeKind;
use crate::domain::model::{
    CheckStatus, DraftComment, Mergeable, PrInfo, Project, ReviewComment, ReviewEvent,
    ReviewSummary, ReviewThread,
};
use crate::error::{CoreError, Result};

mod azure;
mod github;
mod remote;
mod sim;

pub use azure::{
    normalize_azure_identity, AzureForge, AzureForges, PAT_ENV as AZURE_DEVOPS_PAT_ENV,
};
pub use github::*;
pub use remote::{parse_azure_remote, parse_remote, AzureRepo, RemoteForge};
pub use sim::SimForge;

/// A one-line summary of an open PR discovered by the review poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrSummary {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_ref: String,
    pub base_ref: String,
    pub url: String,
    /// The PR description, as written by the author.
    pub body: String,
    /// Rolled-up CI state.
    pub checks: CheckStatus,
    /// Whether it merges cleanly into its base.
    pub mergeable: Mergeable,
}

/// A PR's live lifecycle state on the forge, as opposed to the snapshot taken
/// at creation. What the reconciliation passes read to notice a PR that was
/// merged or closed on the forge directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LivePrState {
    Open { draft: bool },
    Merged,
    Closed,
}

/// One failing CI check on a PR — enough to name it in a dialog and (via
/// `url`, which identifies the run to the forge that reported it) fetch its
/// failed-step log for the fixing agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedCheck {
    /// The check's name (e.g. `test`), or the status context (e.g. `ci/lint`).
    pub name: String,
    /// The workflow the check belongs to, when reported (`CheckRun`s only).
    pub workflow: String,
    /// The check's details page — an Actions run URL for GitHub Actions
    /// checks, a build results URL for Azure Pipelines.
    pub url: String,
}

/// An open PR on this repo that a card could adopt — the adopt dialog's
/// "Pull requests" group. Carries everything the dialog prefills from, so
/// picking one needs no probe round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub head_ref: String,
    pub base_ref: String,
    pub url: String,
    /// The PR description, as written by the author.
    pub body: String,
    pub draft: bool,
    /// Whether the head lives on a fork (not adoptable: we can't push to it
    /// as the card's own branch).
    pub cross_repo: bool,
    /// Whether the signed-in user authored it. `false` when the login can't be
    /// read — the dialog then merely shows an extra warning.
    pub mine: bool,
}

/// The reviewer login to record on a created PR: trimmed, with the empty/absent
/// case collapsed to `None` (matching the `--reviewer` arg being omitted).
pub fn normalize_reviewer(reviewer: Option<&str>) -> Option<String> {
    reviewer
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
}

/// Where a PR's head branch lives, and whether we may push to it — what
/// "I'll fix this myself" needs to know *before* the promise is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrPushTarget {
    /// The PR's head branch name, as the target repo holds it.
    pub head_ref: String,
    /// The branch the PR targets (empty when the forge didn't say).
    pub base_ref: String,
    /// Whether the head is on a fork rather than this repo.
    pub cross_repo: bool,
    /// `owner/repo` of the head repository (GitHub forks only; empty otherwise).
    pub head_repo: String,
    /// Whether the author ticked "allow edits by maintainers".
    pub maintainer_can_modify: bool,
}

impl PrPushTarget {
    /// Whether a maintainer may push to this head: always for a same-repo
    /// branch, only with the author's consent for a fork.
    pub fn pushable(&self) -> bool {
        !self.cross_repo || self.maintainer_can_modify
    }
}

/// Which open PRs the review board should track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewScope {
    /// Only PRs authored by these forge handles (logins, emails).
    Authors(Vec<String>),
    /// Every open PR except the current user's own.
    Everyone,
}

// --- the port ---------------------------------------------------------------

#[async_trait]
pub trait Forge: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn create_pr(
        &self,
        repo: &Path,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
        reviewer: Option<&str>,
        draft: bool,
    ) -> Result<PrInfo>;

    async fn fetch_comments(&self, repo: &Path, pr_number: u64) -> Result<Vec<ReviewComment>>;

    /// Open PRs in `scope` that the current user hasn't yet reviewed.
    async fn list_review_prs(&self, repo: &Path, scope: ReviewScope) -> Result<Vec<PrSummary>>;

    /// Submit a review (a batch of inline comments + an overall verdict) on a PR.
    async fn submit_review(
        &self,
        repo: &Path,
        pr_number: u64,
        event: ReviewEvent,
        body: &str,
        comments: &[DraftComment],
    ) -> Result<()>;

    /// Forge handles that can be requested as PR reviewers on this repo.
    async fn list_reviewers(&self, repo: &Path) -> Result<Vec<String>>;

    /// Forge handles with an open PR on this repo — the contributor picker's
    /// suggestions, which collaborators alone miss entirely for fork PRs.
    /// Defaulted rather than required: test doubles never call it.
    async fn list_pr_authors(&self, _repo: &Path) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// The latest submitted review per reviewer (who actually reviewed).
    async fn list_submitted_reviews(
        &self,
        repo: &Path,
        pr_number: u64,
    ) -> Result<Vec<ReviewSummary>>;

    /// Post a reply on a specific PR review comment.
    async fn reply_to_comment(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<()>;

    /// Answer a review comment the user chose *not* to fix: post `body` as a
    /// reply and, where the forge models it, close the conversation as
    /// "won't fix". On Azure DevOps that second step matters — a branch
    /// policy can refuse to complete a PR while any thread is still active —
    /// so a declined comment must not stay open there. GitHub has no such
    /// state (a thread we spoke last on already reads as answered), so the
    /// default is the plain reply.
    async fn decline_comment(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<()> {
        self.reply_to_comment(repo, pr_number, comment_id, body)
            .await
    }

    /// Flip a draft PR to ready-for-review.
    async fn mark_ready(&self, repo: &Path, pr_number: u64) -> Result<()>;

    /// Squash-merge the PR. `Ok` means the merge has *landed* — a forge that
    /// completes merges asynchronously (Azure DevOps) waits for it — because
    /// the caller tears the worktree down right after. Branch cleanup is the
    /// caller's job.
    async fn merge(&self, repo: &Path, pr_number: u64) -> Result<()>;

    /// Whether the PR is already merged on the forge. Used to recover a merge
    /// whose local cleanup failed after the merge itself landed.
    async fn is_merged(&self, repo: &Path, pr_number: u64) -> Result<bool>;

    /// Whether the PR still merges cleanly onto its base. Asked after a failed
    /// merge to recognize a conflict, and by the background poll to gate the
    /// merge button. `Unknown` is a real answer, not an error: GitHub recomputes
    /// mergeability asynchronously after every push, and reports `UNKNOWN` until
    /// it lands — a caller that needs certainty must poll.
    async fn merge_status(&self, repo: &Path, pr_number: u64) -> Result<Mergeable>;

    /// Delete the PR's head branch on the remote.
    async fn delete_remote_branch(&self, repo: &Path, branch: &str) -> Result<()>;

    /// Mark the review threads of the given comments *resolved* on the PR. Best
    /// effort: unknown or already-resolved threads are skipped. Returns how many
    /// threads were newly resolved.
    async fn resolve_threads(
        &self,
        repo: &Path,
        pr_number: u64,
        comment_ids: &[u64],
    ) -> Result<usize>;

    /// List the PR's review threads (resolved flag, comment ids, who spoke
    /// last). This is what tells an answered comment from one still awaiting a
    /// reaction — the flat comment list can't.
    async fn list_threads(&self, repo: &Path, pr_number: u64) -> Result<Vec<ReviewThread>>;

    /// The PR's rolled-up CI state plus the failing checks, if any. Defaults to
    /// "no checks" so forges that don't model CI (the sim, test doubles) keep
    /// merging unimpeded.
    async fn pr_checks(
        &self,
        _repo: &Path,
        _pr_number: u64,
    ) -> Result<(CheckStatus, Vec<FailedCheck>)> {
        Ok((CheckStatus::None, Vec::new()))
    }

    /// The failed-step logs behind `failed` checks, as `(display name, raw log)`
    /// pairs — one per distinct CI run, however many of its jobs failed.
    /// Best-effort context for the fixing agent: a check this forge can't read
    /// a log for (a third-party status, a fetch that failed) is just skipped,
    /// and the default has nothing to offer at all.
    async fn failed_check_logs(
        &self,
        _repo: &Path,
        _failed: &[FailedCheck],
    ) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The PR's live lifecycle state on the forge. `Ok(None)` means "can't
    /// tell" and is the default, so forges that don't model it (the sim, test
    /// doubles) leave every card and task untouched. A transport failure is an
    /// `Err`, never `Closed` — a network hiccup must not move or tear down
    /// anything.
    async fn pr_live_state(&self, _repo: &Path, _pr_number: u64) -> Result<Option<LivePrState>> {
        Ok(None)
    }

    /// Where the PR's head branch lives and whether we may push to it.
    /// `Ok(None)` means "can't tell" and is the default, so forges that don't
    /// model it leave the caller to proceed on its own judgement.
    async fn pr_push_target(&self, _repo: &Path, _pr_number: u64) -> Result<Option<PrPushTarget>> {
        Ok(None)
    }

    /// Post a plain comment on the PR's conversation (not a review). Used to
    /// follow up on a published review — "pushed the fix", or "on reflection
    /// I'm leaving these to you". Defaults to a no-op for forges that don't
    /// model it.
    async fn comment_on_pr(&self, _repo: &Path, _pr_number: u64, _body: &str) -> Result<()> {
        Ok(())
    }

    /// The open PR whose head branch is `head`, if one exists. Best-effort
    /// context for the adopt probe's "open PR" warning, and for `create_pr`'s
    /// recovery of a PR gh opened before failing: "no PR" and "can't tell" both
    /// come back `None`, so forges that don't model it — the sim, test doubles —
    /// need no override.
    async fn pr_for_head(&self, _repo: &Path, _head: &str) -> Result<Option<PrInfo>> {
        Ok(None)
    }

    /// Every open PR on the repo — the adopt dialog's "Pull requests" group.
    /// Defaulted to none so test doubles need no override.
    async fn list_open_prs(&self, _repo: &Path) -> Result<Vec<OpenPr>> {
        Ok(Vec::new())
    }

    /// PR `pr_number` as a [`PrInfo`], `None` unless it is open. What PR
    /// adoption records on the card; the default ("can't tell") refuses it.
    async fn pr_by_number(&self, _repo: &Path, _pr_number: u64) -> Result<Option<PrInfo>> {
        Ok(None)
    }
}

// --- per-kind behavior that needs no forge instance --------------------------

impl ForgeKind {
    /// A person's handle as typed by a human, normalized to the form the forge
    /// compares — or `None` when it can't be one (see [`normalize_login`] for
    /// why GitHub's check is strict).
    pub fn normalize_identity(self, input: &str) -> Option<String> {
        match self {
            ForgeKind::GitHub => normalize_login(input),
            ForgeKind::AzureDevOps => normalize_azure_identity(input),
        }
    }

    /// Whether the checkout carries CI configuration that will report checks
    /// on a PR — the offline fallback for
    /// [`crate::ProjectConfig::ci_checks`], which a real observation always
    /// overrides.
    pub fn expects_ci_offline(self, repo: &Path) -> bool {
        match self {
            ForgeKind::GitHub => repo_has_workflows(repo),
            ForgeKind::AzureDevOps => azure::repo_has_pipelines(repo),
        }
    }

    /// The sentence the CI-fix prompt uses to point the agent at the forge's
    /// own tooling for digging past the log tails it was given.
    pub fn ci_hint(self) -> &'static str {
        match self {
            ForgeKind::GitHub => {
                "You can inspect CI yourself with `gh pr checks` and `gh run view <run-id> \
                 --log-failed` if you need more than the logs above."
            }
            ForgeKind::AzureDevOps => {
                "The failing builds' pages are linked above if you need more than the logs \
                 above; `az pipelines runs show --id <build-id>` works too when the Azure \
                 CLI is installed and signed in."
            }
        }
    }

    /// The remote ref a contributor PR's head is fetched from for review, when
    /// the forge doesn't use GitHub's `pull/<n>/head`. `None` means "use
    /// [`crate::GitOps::fetch_pr`]". Azure DevOps publishes no head ref for a
    /// PR (only `refs/pull/<n>/merge`, a merge commit), so the source branch
    /// itself is fetched.
    pub fn pr_fetch_ref(self, head_ref: &str) -> Option<String> {
        match self {
            ForgeKind::GitHub => None,
            ForgeKind::AzureDevOps => {
                let branch = head_ref.strip_prefix("refs/heads/").unwrap_or(head_ref);
                Some(format!("refs/heads/{branch}"))
            }
        }
    }
}

/// The forge `repo`'s `origin` remote points at, or `None` when there is no
/// `origin` or its host isn't one we recognize (see [`parse_remote`]).
pub fn detect_forge(repo: &Path) -> Option<ForgeKind> {
    let url = crate::infra::git::origin_url(repo)?;
    Some(match parse_remote(&url)? {
        RemoteForge::GitHub => ForgeKind::GitHub,
        RemoteForge::AzureDevOps(_) => ForgeKind::AzureDevOps,
    })
}

// --- resolution ---------------------------------------------------------------

/// Builds the forge for one repository of a given kind — for hosts whose client
/// is per-repository (Azure DevOps addresses org/project/repo explicitly, where
/// `gh` infers them from the checkout).
pub trait ForgeFactory: Send + Sync {
    fn for_repo(&self, repo: &Path) -> Result<Arc<dyn Forge>>;
}

/// Resolves the forge each project's PR operations go through. The default
/// forge serves every kind without a registered factory — in production that
/// is GitHub, in demo mode and tests the one simulator serving every project.
#[derive(Clone)]
pub struct ForgeRegistry {
    default: Arc<dyn Forge>,
    factories: HashMap<ForgeKind, Arc<dyn ForgeFactory>>,
}

impl ForgeRegistry {
    /// Every project, whatever its kind, uses `default`.
    pub fn new(default: Arc<dyn Forge>) -> Self {
        ForgeRegistry {
            default,
            factories: HashMap::new(),
        }
    }

    /// The forge serving every kind without a factory of its own.
    pub fn default_forge(&self) -> Arc<dyn Forge> {
        Arc::clone(&self.default)
    }

    /// Route projects of `kind` through `factory` instead of the default.
    pub fn with(mut self, kind: ForgeKind, factory: Arc<dyn ForgeFactory>) -> Self {
        self.factories.insert(kind, factory);
        self
    }

    /// The real hosts: GitHub via `gh`, Azure DevOps over REST.
    pub fn real() -> Self {
        Self::new(Arc::new(GhForge)).with(ForgeKind::AzureDevOps, Arc::new(AzureForges::new()))
    }

    /// The forge for `project`'s current kind. Never fails: a forge that can't
    /// be built (an Azure project whose `origin` isn't an Azure remote)
    /// resolves to one whose every call fails with the reason, so the error
    /// surfaces exactly where a transport failure would — a toast on a user
    /// action, a logged skip in a background poll.
    pub fn for_project(&self, project: &Project) -> Arc<dyn Forge> {
        let kind = project.config.effective_forge();
        match self.factories.get(&kind) {
            None => Arc::clone(&self.default),
            Some(factory) => factory.for_repo(&project.path).unwrap_or_else(|e| {
                Arc::new(UnavailableForge {
                    reason: format!("{}: {e}", kind.display_name()),
                })
            }),
        }
    }
}

/// A forge that could not be built. Every call fails with the reason — including
/// the defaulted reads, whose "can't tell" answers would otherwise be taken for
/// real ones ("no CI", "no push target").
struct UnavailableForge {
    reason: String,
}

impl UnavailableForge {
    fn err<T>(&self) -> Result<T> {
        Err(CoreError::forge(self.reason.clone()))
    }
}

#[async_trait]
impl Forge for UnavailableForge {
    async fn create_pr(
        &self,
        _: &Path,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: bool,
    ) -> Result<PrInfo> {
        self.err()
    }
    async fn fetch_comments(&self, _: &Path, _: u64) -> Result<Vec<ReviewComment>> {
        self.err()
    }
    async fn list_review_prs(&self, _: &Path, _: ReviewScope) -> Result<Vec<PrSummary>> {
        self.err()
    }
    async fn submit_review(
        &self,
        _: &Path,
        _: u64,
        _: ReviewEvent,
        _: &str,
        _: &[DraftComment],
    ) -> Result<()> {
        self.err()
    }
    async fn list_reviewers(&self, _: &Path) -> Result<Vec<String>> {
        self.err()
    }
    async fn list_pr_authors(&self, _: &Path) -> Result<Vec<String>> {
        self.err()
    }
    async fn list_submitted_reviews(&self, _: &Path, _: u64) -> Result<Vec<ReviewSummary>> {
        self.err()
    }
    async fn reply_to_comment(&self, _: &Path, _: u64, _: u64, _: &str) -> Result<()> {
        self.err()
    }
    async fn mark_ready(&self, _: &Path, _: u64) -> Result<()> {
        self.err()
    }
    async fn merge(&self, _: &Path, _: u64) -> Result<()> {
        self.err()
    }
    async fn is_merged(&self, _: &Path, _: u64) -> Result<bool> {
        self.err()
    }
    async fn merge_status(&self, _: &Path, _: u64) -> Result<Mergeable> {
        self.err()
    }
    async fn delete_remote_branch(&self, _: &Path, _: &str) -> Result<()> {
        self.err()
    }
    async fn resolve_threads(&self, _: &Path, _: u64, _: &[u64]) -> Result<usize> {
        self.err()
    }
    async fn list_threads(&self, _: &Path, _: u64) -> Result<Vec<ReviewThread>> {
        self.err()
    }
    async fn pr_checks(&self, _: &Path, _: u64) -> Result<(CheckStatus, Vec<FailedCheck>)> {
        self.err()
    }
    async fn pr_live_state(&self, _: &Path, _: u64) -> Result<Option<LivePrState>> {
        self.err()
    }
    async fn pr_push_target(&self, _: &Path, _: u64) -> Result<Option<PrPushTarget>> {
        self.err()
    }
    async fn comment_on_pr(&self, _: &Path, _: u64, _: &str) -> Result<()> {
        self.err()
    }
    async fn list_open_prs(&self, _: &Path) -> Result<Vec<OpenPr>> {
        self.err()
    }
    async fn pr_by_number(&self, _: &Path, _: u64) -> Result<Option<PrInfo>> {
        self.err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::ProjectConfig;
    use std::path::PathBuf;

    fn project(kind: Option<ForgeKind>) -> Project {
        Project::new(
            "p",
            PathBuf::from("/nonexistent/usine-forge-registry"),
            ProjectConfig {
                pinned_forge: kind,
                ..ProjectConfig::default()
            },
        )
    }

    struct Failing;
    impl ForgeFactory for Failing {
        fn for_repo(&self, _: &Path) -> Result<Arc<dyn Forge>> {
            Err(CoreError::forge("origin is not an Azure DevOps remote"))
        }
    }

    #[tokio::test]
    async fn a_single_forge_serves_every_kind() {
        let reg = ForgeRegistry::new(Arc::new(SimForge));
        for kind in [None, Some(ForgeKind::GitHub), Some(ForgeKind::AzureDevOps)] {
            let forge = reg.for_project(&project(kind));
            assert!(forge.merge(Path::new("/"), 1).await.is_ok());
        }
    }

    #[tokio::test]
    async fn an_unbuildable_forge_fails_every_call_with_the_reason() {
        let reg =
            ForgeRegistry::new(Arc::new(SimForge)).with(ForgeKind::AzureDevOps, Arc::new(Failing));
        // GitHub projects still get the default.
        assert!(reg
            .for_project(&project(None))
            .merge(Path::new("/"), 1)
            .await
            .is_ok());
        let forge = reg.for_project(&project(Some(ForgeKind::AzureDevOps)));
        let err = forge
            .merge(Path::new("/"), 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Azure DevOps") && err.contains("not an Azure DevOps remote"),
            "{err}"
        );
        // Defaulted reads fail too, instead of answering "no CI".
        assert!(forge.pr_checks(Path::new("/"), 1).await.is_err());
    }

    #[test]
    fn azure_fetches_the_source_branch_github_the_pull_ref() {
        assert_eq!(ForgeKind::GitHub.pr_fetch_ref("feat/x"), None);
        assert_eq!(
            ForgeKind::AzureDevOps
                .pr_fetch_ref("refs/heads/feat/x")
                .as_deref(),
            Some("refs/heads/feat/x")
        );
        assert_eq!(
            ForgeKind::AzureDevOps.pr_fetch_ref("feat/x").as_deref(),
            Some("refs/heads/feat/x")
        );
    }
}
