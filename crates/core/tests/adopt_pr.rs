//! End-to-end guards on PR adoption (`AdoptPr`), against REAL git with a bare
//! `origin` — the card attaches to the PR's own head branch, so only a real
//! repo can prove which branch it ends up on and where that branch points.
//!
//! The invariants under test:
//! - adoption attaches the card to the PR head (no `usine/` cut) and lands it
//!   at the PR-review stage — or the merge gate when nobody is asked to review;
//! - a local head behind the remote is fast-forwarded, one ahead is kept and
//!   pushed (so a Merge can't land the PR without it);
//! - a PR with no pending review request leaves its reviewer unrecorded, so
//!   the project's configured reviewer still applies;
//! - a diverged or checked-out head, a fork, another base and an already-owned
//!   PR are refused before any card or worktree exists;
//! - the listing moves a PR's head out of the branch group and hides PRs a
//!   card already owns;
//! - deleting the adopted card reaps its worktree and local branch but never
//!   the remote PR branch.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardState, DraftComment, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, Forge, Mergeable, OpenPr, PrInfo, PrPushTarget, PrReviewSub, PrSummary,
    Project, ProjectConfig, RealGit, ReviewComment, ReviewEvent, ReviewScope, ReviewSummary,
    ReviewThread, Severity, SimFactory, SimForge, Store,
};

const PR: u64 = 7;
const HEAD: &str = "feat/remote";

/// A quiet forge (no comments, reviews or threads) holding one open PR, #7 on
/// `HEAD` against `dev`. Everything unrelated delegates to [`SimForge`].
#[derive(Clone)]
struct PrForge {
    cross_repo: bool,
    base: String,
    reviewer: Option<String>,
}

impl Default for PrForge {
    fn default() -> Self {
        PrForge {
            cross_repo: false,
            base: "dev".into(),
            reviewer: Some("octocat".into()),
        }
    }
}

#[async_trait]
impl Forge for PrForge {
    async fn list_open_prs(&self, _: &Path) -> usine_core::Result<Vec<OpenPr>> {
        Ok(vec![OpenPr {
            number: PR,
            title: "Remote work".into(),
            author: "me".into(),
            head_ref: HEAD.into(),
            base_ref: self.base.clone(),
            url: "u".into(),
            body: "Opened on another machine.".into(),
            draft: false,
            cross_repo: self.cross_repo,
            mine: true,
        }])
    }
    async fn pr_push_target(&self, _: &Path, _: u64) -> usine_core::Result<Option<PrPushTarget>> {
        Ok(Some(PrPushTarget {
            head_ref: HEAD.into(),
            base_ref: self.base.clone(),
            cross_repo: self.cross_repo,
            head_repo: String::new(),
            maintainer_can_modify: false,
        }))
    }
    async fn pr_by_number(&self, _: &Path, n: u64) -> usine_core::Result<Option<PrInfo>> {
        Ok(Some(PrInfo {
            number: n,
            url: "u".into(),
            title: "Remote work".into(),
            state: "open".into(),
            reviewer: self.reviewer.clone(),
            reviewer_recorded: false,
        }))
    }
    async fn fetch_comments(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewComment>> {
        Ok(vec![])
    }
    async fn list_submitted_reviews(
        &self,
        _: &Path,
        _: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        Ok(vec![])
    }
    async fn list_threads(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewThread>> {
        Ok(vec![])
    }
    async fn create_pr(
        &self,
        r: &Path,
        t: &str,
        b: &str,
        base: &str,
        h: &str,
        rev: Option<&str>,
        d: bool,
    ) -> usine_core::Result<PrInfo> {
        SimForge.create_pr(r, t, b, base, h, rev, d).await
    }
    async fn list_review_prs(
        &self,
        r: &Path,
        s: ReviewScope,
    ) -> usine_core::Result<Vec<PrSummary>> {
        SimForge.list_review_prs(r, s).await
    }
    async fn submit_review(
        &self,
        r: &Path,
        n: u64,
        e: ReviewEvent,
        b: &str,
        c: &[DraftComment],
    ) -> usine_core::Result<()> {
        SimForge.submit_review(r, n, e, b, c).await
    }
    async fn list_reviewers(&self, r: &Path) -> usine_core::Result<Vec<String>> {
        SimForge.list_reviewers(r).await
    }
    async fn reply_to_comment(&self, r: &Path, n: u64, c: u64, b: &str) -> usine_core::Result<()> {
        SimForge.reply_to_comment(r, n, c, b).await
    }
    async fn mark_ready(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.mark_ready(r, n).await
    }
    async fn merge(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.merge(r, n).await
    }
    async fn is_merged(&self, r: &Path, n: u64) -> usine_core::Result<bool> {
        SimForge.is_merged(r, n).await
    }
    async fn merge_status(&self, r: &Path, n: u64) -> usine_core::Result<Mergeable> {
        SimForge.merge_status(r, n).await
    }
    async fn delete_remote_branch(&self, r: &Path, b: &str) -> usine_core::Result<()> {
        SimForge.delete_remote_branch(r, b).await
    }
    async fn resolve_threads(&self, r: &Path, n: u64, c: &[u64]) -> usine_core::Result<usize> {
        SimForge.resolve_threads(r, n, c).await
    }
}

/// Keep adopted cards' worktrees out of the user's real data dir (see
/// `adopt_branch.rs`).
fn isolate_data_dir() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = tempfile::tempdir().expect("create data dir").keep();
        std::env::set_var("USINE_DATA_DIR", dir);
    });
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn git(dir: &Path, args: &[&str]) {
    git_stdout(dir, args);
}

fn sha(dir: &Path, rev: &str) -> String {
    git_stdout(dir, &["rev-parse", rev])
}

fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .expect("run git")
        .success()
}

fn commit(dir: &Path, file: &str, msg: &str) {
    std::fs::write(dir.join(file), msg).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
}

/// A repo on `dev` whose bare `origin` holds `dev` and the PR head `HEAD` (one
/// commit ahead). The local `HEAD` branch is deleted — the PR was opened from
/// another machine — and origin is fetched, so only `origin/HEAD` exists.
fn repo_with_remote_pr(tmp: &Path) -> (PathBuf, PathBuf) {
    let repo = tmp.join("repo");
    let origin = tmp.join("origin.git");
    std::fs::create_dir_all(&repo).unwrap();
    git(tmp, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    commit(&repo, "a.txt", "base");
    git(&repo, &["branch", "-M", "dev"]);
    git(&repo, &["checkout", "-qb", HEAD]);
    commit(&repo, "remote.txt", "feat: work from the laptop");
    git(&repo, &["checkout", "-q", "dev"]);
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "-q", "origin", "dev", HEAD]);
    git(&repo, &["branch", "-D", HEAD]);
    git(&repo, &["fetch", "-q", "origin"]);
    (repo, origin)
}

async fn wait_for<F, T>(rx: &mut UnboundedReceiver<ExecutorEvent>, mut f: F) -> T
where
    F: FnMut(&ExecutorEvent) -> Option<T>,
{
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(15), rx.next())
            .await
            .expect("timed out waiting for an executor event")
            .expect("event stream closed unexpectedly");
        if let Some(v) = f(&evt) {
            return v;
        }
    }
}

fn executor_for(
    repo: &Path,
    forge: PrForge,
) -> (
    Store,
    uuid::Uuid,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
) {
    isolate_data_dir();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", repo.to_path_buf(), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(forge),
        git: Arc::new(RealGit),
    });
    (store, project.id, handle, rx)
}

fn adopt_cmd(project_id: uuid::Uuid) -> ExecutorCommand {
    ExecutorCommand::AdoptPr {
        project_id,
        pr_number: PR,
        title: "Remote work".into(),
        description: "Opened on another machine.".into(),
    }
}

/// Adopt and wait for the success toast, returning the card as persisted
/// once the first review refresh settled (the claim's release marks it).
async fn adopt(
    store: &Store,
    project_id: uuid::Uuid,
    handle: &usine_core::ExecutorHandle,
    rx: &mut UnboundedReceiver<ExecutorEvent>,
) -> Card {
    handle.send(adopt_cmd(project_id));
    let card_id = wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } if message.contains("Adopted PR #7") => Some(e.card_id),
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => panic!("adoption failed: {message}"),
        _ => None,
    })
    .await;
    wait_for(rx, |e| {
        matches!(&e.kind, ExecutorEventKind::CardBusy { busy: false } if e.card_id == card_id)
            .then_some(())
    })
    .await;
    store.get_card(card_id).unwrap()
}

/// Send `AdoptPr` and return the error toast it answers with.
async fn refusal(
    project_id: uuid::Uuid,
    handle: &usine_core::ExecutorHandle,
    rx: &mut UnboundedReceiver<ExecutorEvent>,
) -> String {
    handle.send(adopt_cmd(project_id));
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => Some(message.clone()),
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } => panic!("expected a refusal, got: {message}"),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn a_remote_only_pr_is_adopted_onto_its_own_head() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    let (store, project_id, handle, mut rx) = executor_for(&repo, PrForge::default());

    let card = adopt(&store, project_id, &handle, &mut rx).await;

    assert_eq!(card.state, CardState::PrReview(PrReviewSub::Idle));
    assert_eq!(
        card.branch.as_deref(),
        Some(HEAD),
        "no usine/ branch is cut"
    );
    let pr = card.pr.expect("the card tracks the PR");
    assert_eq!(pr.number, PR);
    assert!(
        pr.reviewer_recorded,
        "the forge's reviewer is authoritative"
    );
    assert_eq!(sha(&repo, HEAD), sha(&repo, &format!("origin/{HEAD}")));
    let wt = card.worktree_path.expect("the card has a worktree");
    assert!(wt.join("remote.txt").exists(), "the worktree holds the PR");
    assert_eq!(git_stdout(&wt, &["branch", "--show-current"]), HEAD);
    let usine = git_stdout(&repo, &["branch", "--list", "usine/*"]);
    assert!(usine.is_empty(), "no usine branch created");
}

#[tokio::test]
async fn a_pr_with_no_reviewer_goes_straight_to_the_merge_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    let forge = PrForge {
        reviewer: None,
        ..PrForge::default()
    };
    let (store, project_id, handle, mut rx) = executor_for(&repo, forge);

    let card = adopt(&store, project_id, &handle, &mut rx).await;
    assert_eq!(card.state, CardState::ReadyToMerge);
    assert!(
        !card.pr.unwrap().reviewer_recorded,
        "no pending request means unknown, not explicitly none"
    );
}

#[tokio::test]
async fn a_pr_with_no_pending_request_waits_on_the_project_reviewer() {
    // GitHub drops a reviewer from `reviewRequests` once they've reviewed, so
    // the project's configured reviewer must still gate the merge.
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    let forge = PrForge {
        reviewer: None,
        ..PrForge::default()
    };
    let (store, project_id, handle, mut rx) = executor_for(&repo, forge);
    let mut project = store.get_project(project_id).unwrap();
    project.config.reviewer = Some("octocat".into());
    store.upsert_project(&project).unwrap();

    let card = adopt(&store, project_id, &handle, &mut rx).await;
    assert_eq!(card.state, CardState::PrReview(PrReviewSub::Idle));
}

#[tokio::test]
async fn a_local_head_behind_the_remote_is_fast_forwarded() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    git(&repo, &["branch", HEAD, "dev"]);
    let (store, project_id, handle, mut rx) = executor_for(&repo, PrForge::default());

    let card = adopt(&store, project_id, &handle, &mut rx).await;
    assert_eq!(card.branch.as_deref(), Some(HEAD));
    assert_eq!(sha(&repo, HEAD), sha(&repo, &format!("origin/{HEAD}")));
}

#[tokio::test]
async fn a_local_head_ahead_of_the_remote_is_kept_and_pushed() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, origin) = repo_with_remote_pr(tmp.path());
    git(
        &repo,
        &["checkout", "-q", "-b", HEAD, &format!("origin/{HEAD}")],
    );
    commit(&repo, "more.txt", "unpushed follow-up");
    git(&repo, &["checkout", "-q", "dev"]);
    let ahead = sha(&repo, HEAD);
    let (store, project_id, handle, mut rx) = executor_for(&repo, PrForge::default());

    let card = adopt(&store, project_id, &handle, &mut rx).await;
    assert_eq!(sha(&repo, HEAD), ahead, "unpushed commits survive");
    assert_eq!(
        git_stdout(&origin, &["rev-parse", HEAD]),
        ahead,
        "unpushed commits are published, so a Merge can't drop them"
    );
    assert!(card.worktree_path.unwrap().join("more.txt").exists());
}

/// Every refusal leaves no card and no worktree behind.
async fn assert_refused(repo: &Path, forge: PrForge, expect: &str) {
    let (store, project_id, handle, mut rx) = executor_for(repo, forge);
    let msg = refusal(project_id, &handle, &mut rx).await;
    assert!(msg.contains(expect), "expected “{expect}”, got: {msg}");
    assert!(store.list_cards().unwrap().is_empty(), "no card created");
}

#[tokio::test]
async fn a_diverged_local_head_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    git(&repo, &["checkout", "-q", "-b", HEAD, "dev"]);
    commit(&repo, "other.txt", "local-only work");
    git(&repo, &["checkout", "-q", "dev"]);
    let before = sha(&repo, HEAD);
    assert_refused(&repo, PrForge::default(), "diverged").await;
    assert_eq!(sha(&repo, HEAD), before, "the local branch is untouched");
}

#[tokio::test]
async fn a_checked_out_head_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            HEAD,
            wt.to_str().unwrap(),
            &format!("origin/{HEAD}"),
        ],
    );
    assert_refused(&repo, PrForge::default(), "is checked out at").await;
}

#[tokio::test]
async fn fork_and_other_base_prs_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    let fork = PrForge {
        cross_repo: true,
        ..PrForge::default()
    };
    assert_refused(&repo, fork, "forks").await;
    let other_base = PrForge {
        base: "release".into(),
        ..PrForge::default()
    };
    assert_refused(&repo, other_base, "targets `release`").await;
    assert!(!local_branch_exists(&repo, HEAD), "no local branch created");
}

#[tokio::test]
async fn an_already_adopted_pr_is_refused_and_unlisted() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _origin) = repo_with_remote_pr(tmp.path());
    git(&repo, &["branch", "other", "dev"]);
    let (store, project_id, handle, mut rx) = executor_for(&repo, PrForge::default());

    // Before: the PR is listed, and its head left the branch group.
    handle.send(ExecutorCommand::ListAdoptSources { project_id });
    let (refs, prs) = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AdoptSources { refs, prs, .. } => Some((refs.clone(), prs.clone())),
        _ => None,
    })
    .await;
    assert_eq!(prs.iter().map(|p| p.number).collect::<Vec<_>>(), [PR]);
    assert!(
        !refs.iter().any(|r| r.ends_with(HEAD)),
        "the PR head is listed once, as the PR: {refs:?}"
    );
    assert!(refs.contains(&"other".to_string()));

    adopt(&store, project_id, &handle, &mut rx).await;

    handle.send(ExecutorCommand::ListAdoptSources { project_id });
    let prs = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AdoptSources { prs, .. } => Some(prs.clone()),
        _ => None,
    })
    .await;
    assert!(prs.is_empty(), "an owned PR is no longer offered");

    let msg = refusal(project_id, &handle, &mut rx).await;
    assert!(msg.contains("already belongs"), "got: {msg}");
    assert_eq!(store.list_cards().unwrap().len(), 1);
}

#[tokio::test]
async fn deleting_an_adopted_pr_card_leaves_the_remote_branch_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, origin) = repo_with_remote_pr(tmp.path());
    let remote_tip = sha(&origin, HEAD);
    let (store, project_id, handle, mut rx) = executor_for(&repo, PrForge::default());
    let card = adopt(&store, project_id, &handle, &mut rx).await;

    handle.send(ExecutorCommand::DeleteCard { card_id: card.id });
    wait_for(&mut rx, |e| {
        matches!(&e.kind, ExecutorEventKind::CardRemoved if e.card_id == card.id).then_some(())
    })
    .await;

    assert!(!card.worktree_path.unwrap().exists());
    assert!(!local_branch_exists(&repo, HEAD));
    assert_eq!(
        sha(&origin, HEAD),
        remote_tip,
        "the PR's branch is not ours to delete"
    );
}
