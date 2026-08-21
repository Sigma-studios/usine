//! The review diff must survive a refused PR fetch, over REAL git.
//!
//! `compute_review_diff` refetches the PR head into its stable local branch on
//! every open — but from review start until publish/dismiss that branch is
//! checked out in the review worktree, and git refuses to fetch into a
//! checked-out branch (`fatal: refusing to fetch into branch …`). The bug this
//! pins down: the fetch failure was treated as fatal, so the diff could never
//! be computed fresh during exactly the window when the review is validated.
//! The fix diffs the local branch as-is when the fetch fails; only when the
//! branch never made it locally is the fetch error surfaced as the root cause.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, CoreError, DiffState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, GitOps, MergeOutcome, Project, ProjectConfig, Result, ReviewTask,
    SimFactory, SimForge, SimGit, Store,
};

/// Run `git <args>` in `dir`, asserting success.
fn git(dir: &Path, args: &[&str]) {
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
}

fn identify(repo: &Path) {
    git(repo, &["config", "user.email", "t@t.dev"]);
    git(repo, &["config", "user.name", "t"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
}

/// Git that behaves like the simulator, except `fetch_pr` is refused — the
/// error real git raises while the PR branch is checked out in the review
/// worktree. Everything else defers to the simulator's always-succeeds
/// behavior; the diff walk itself reads the real repo via git2, not this trait.
struct RefusingFetchGit;

#[async_trait]
impl GitOps for RefusingFetchGit {
    async fn fetch_pr(&self, _: &Path, n: u64, b: &str) -> Result<()> {
        Err(CoreError::other(format!(
            "git fetch origin +pull/{n}/head:{b} failed: fatal: refusing to fetch into branch \
             'refs/heads/{b}' checked out at '/somewhere'"
        )))
    }
    async fn create_worktree(&self, r: &Path, b: &str, p: &Path, base: &str) -> Result<()> {
        SimGit.create_worktree(r, b, p, base).await
    }
    async fn remove_worktree(&self, r: &Path, p: &Path) -> Result<()> {
        SimGit.remove_worktree(r, p).await
    }
    async fn worktree_add_existing(&self, r: &Path, b: &str, p: &Path) -> Result<()> {
        SimGit.worktree_add_existing(r, b, p).await
    }
    async fn worktree_add_detached(&self, r: &Path, p: &Path, c: &str) -> Result<()> {
        SimGit.worktree_add_detached(r, p, c).await
    }
    async fn reset_mixed(&self, d: &Path, g: &str) -> Result<()> {
        SimGit.reset_mixed(d, g).await
    }
    async fn rename_branch(&self, d: &Path, o: &str, n: &str) -> Result<()> {
        SimGit.rename_branch(d, o, n).await
    }
    async fn delete_branch(&self, r: &Path, b: &str) -> Result<()> {
        SimGit.delete_branch(r, b).await
    }
    async fn fetch(&self, d: &Path, remote: &str) -> Result<()> {
        SimGit.fetch(d, remote).await
    }
    async fn merge_ref(&self, d: &Path, g: &str) -> Result<MergeOutcome> {
        SimGit.merge_ref(d, g).await
    }
    async fn commit_all(&self, d: &Path, m: &str) -> Result<bool> {
        SimGit.commit_all(d, m).await
    }
    async fn push(&self, d: &Path, b: &str) -> Result<()> {
        SimGit.push(d, b).await
    }
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

/// The user's clone, with the PR head already fetched into `usine-review/552`
/// — the state a repo is in once a review has started.
fn repo_with_fetched_pr(tmp: &Path) -> PathBuf {
    let upstream = tmp.join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q"]);
    identify(&upstream);
    std::fs::write(upstream.join("labels.ts"), "export const a = 1;\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-qm", "base"]);
    git(&upstream, &["branch", "-M", "main"]);

    // The PR under review: forks from main, touches one file.
    git(&upstream, &["checkout", "-qb", "feat/seasons"]);
    std::fs::write(upstream.join("labels.ts"), "export const a = 2;\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-qm", "feat: export all seasons"]);
    git(&upstream, &["checkout", "-q", "main"]);

    let repo = tmp.join("repo");
    git(
        tmp,
        &[
            "clone",
            "-q",
            upstream.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    identify(&repo);
    git(
        &repo,
        &[
            "fetch",
            "-q",
            "origin",
            "+refs/heads/feat/seasons:usine-review/552",
        ],
    );
    repo
}

fn seed(store: &Store, repo: PathBuf, pr: u64) -> uuid::Uuid {
    let project = Project::new("p", repo, ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let task = ReviewTask::new(project.id, pr, "t", "octocat", "u", "feat/seasons", "main");
    store.upsert_review_task(&task).unwrap();
    task.id
}

#[tokio::test]
async fn a_refused_fetch_falls_back_to_the_local_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_fetched_pr(tmp.path());

    let store = Store::open_in_memory().unwrap();
    let review_id = seed(&store, repo, 552);

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store,
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RefusingFetchGit),
    });

    handle.send(ExecutorCommand::ComputeReviewDiff { review_id });

    // The fetch is refused, but `usine-review/552` is sitting right there: the
    // diff must come back Ready with the PR's file, not Failed.
    let data = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::DiffUpdated { state } if e.card_id == review_id => match state {
            DiffState::Ready(data) => Some(data.clone()),
            DiffState::Failed(msg) => panic!("diff failed instead of falling back: {msg}"),
            _ => None,
        },
        _ => None,
    })
    .await;
    let names: Vec<_> = data
        .files
        .iter()
        .filter_map(|f| f.new_path.clone())
        .collect();
    assert_eq!(names, vec!["labels.ts".to_string()]);
}

#[tokio::test]
async fn a_refused_fetch_with_no_local_branch_surfaces_the_fetch_error() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_fetched_pr(tmp.path());

    let store = Store::open_in_memory().unwrap();
    // PR #553 was never fetched: no `usine-review/553` branch exists, so the
    // failed walk must be reported as the fetch failure it really is.
    let review_id = seed(&store, repo, 553);

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store,
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RefusingFetchGit),
    });

    handle.send(ExecutorCommand::ComputeReviewDiff { review_id });

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::DiffUpdated { state } if e.card_id == review_id => match state {
            DiffState::Failed(msg) => Some(msg.clone()),
            DiffState::Ready(_) | DiffState::Empty => {
                panic!("a diff with no local branch cannot succeed")
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(
        msg.contains("couldn't fetch PR #553"),
        "the fetch error is the root cause worth showing: {msg}"
    );
}
