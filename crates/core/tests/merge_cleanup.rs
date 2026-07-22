//! Guards the post-merge cleanup order.
//!
//! Merging used to run `gh pr merge --squash --delete-branch`, which makes `gh`
//! delete the *local* branch — something git refuses while the card's worktree
//! has it checked out. `gh`'s non-zero exit then aborted both the `Done`
//! transition and the worktree removal, so every merge left the card stranded
//! mid-board with an orphan worktree and branch, on both sides.
//!
//! Two properties keep that from coming back: the branch is only deleted once
//! its worktree is gone (proven against real git, since SimGit is a no-op), and
//! a merged PR always reaches `Done` even when every cleanup step fails.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, CoreError, DraftComment, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Forge, GitOps, MergeOutcome, MergeStatus,
    PrInfo, PrSummary, Project, ProjectConfig, RealGit, ReviewComment, ReviewEvent, ReviewSummary,
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

/// A repo on `dev` with a `feature` branch checked out in its own worktree —
/// exactly the state a card is in when its PR is merged.
fn repo_with_worktree_on_feature(tmp: &Path) -> (PathBuf, PathBuf) {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["branch", "-M", "dev"]);

    let wt = tmp.join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
            "dev",
        ],
    );
    std::fs::write(wt.join("feat.txt"), "f").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "feat"]);
    (repo, wt)
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

/// The fact the old code got backwards: a branch held by a worktree cannot be
/// deleted, and can be as soon as the worktree is removed.
#[tokio::test]
async fn a_branch_is_only_deletable_once_its_worktree_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, wt) = repo_with_worktree_on_feature(tmp.path());
    let gitops = RealGit;

    // Deleting first — what `gh pr merge --delete-branch` did — fails.
    let err = gitops
        .delete_branch(&repo, "feature")
        .await
        .expect_err("git must refuse to delete a branch checked out in a worktree");
    // Git's wording for this refusal varies by version: >= 2.36 says
    // "used by worktree at", older releases (Apple Git included) say
    // "checked out at". Both are the same worktree conflict.
    let msg = err.to_string();
    assert!(
        msg.contains("worktree") || msg.contains("checked out at"),
        "expected a worktree conflict, got: {err}"
    );
    assert!(local_branch_exists(&repo, "feature"));

    // Freeing the branch first — what the executor now does — succeeds.
    gitops.remove_worktree(&repo, &wt).await.unwrap();
    gitops.delete_branch(&repo, "feature").await.unwrap();
    assert!(!local_branch_exists(&repo, "feature"));
    assert!(!wt.exists());
}

/// A forge that merges fine (so the PR really is merged) but fails every branch
/// deletion, standing in for any cleanup that can go wrong after the merge.
struct CleanupFailsForge;

#[async_trait]
impl Forge for CleanupFailsForge {
    async fn merge(&self, _: &Path, _: u64) -> usine_core::Result<()> {
        Ok(())
    }
    async fn delete_remote_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Err(CoreError::forge("remote branch delete failed"))
    }
    async fn is_merged(&self, _: &Path, _: u64) -> usine_core::Result<bool> {
        Ok(true)
    }
    async fn merge_status(&self, _: &Path, _: u64) -> usine_core::Result<MergeStatus> {
        Ok(MergeStatus::Mergeable)
    }
    // The rest is never reached by a merge; defer to the simulator.
    async fn create_pr(
        &self,
        repo: &Path,
        title: &str,
        body: &str,
        base: &str,
        head: &str,
        reviewer: Option<&str>,
        draft: bool,
    ) -> usine_core::Result<PrInfo> {
        SimForge
            .create_pr(repo, title, body, base, head, reviewer, draft)
            .await
    }
    async fn fetch_comments(&self, r: &Path, n: u64) -> usine_core::Result<Vec<ReviewComment>> {
        SimForge.fetch_comments(r, n).await
    }
    async fn list_review_prs(&self, r: &Path, a: &[String]) -> usine_core::Result<Vec<PrSummary>> {
        SimForge.list_review_prs(r, a).await
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
    async fn list_submitted_reviews(
        &self,
        r: &Path,
        n: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        SimForge.list_submitted_reviews(r, n).await
    }
    async fn reply_to_comment(&self, r: &Path, n: u64, c: u64, b: &str) -> usine_core::Result<()> {
        SimForge.reply_to_comment(r, n, c, b).await
    }
    async fn mark_ready(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.mark_ready(r, n).await
    }
    async fn resolve_threads(&self, r: &Path, n: u64, c: &[u64]) -> usine_core::Result<usize> {
        SimForge.resolve_threads(r, n, c).await
    }
    async fn list_threads(
        &self,
        r: &Path,
        n: u64,
    ) -> usine_core::Result<Vec<usine_core::ReviewThread>> {
        SimForge.list_threads(r, n).await
    }
}

/// Git ops whose worktree removal and branch deletion always fail.
struct CleanupFailsGit;

#[async_trait]
impl GitOps for CleanupFailsGit {
    async fn remove_worktree(&self, _: &Path, _: &Path) -> usine_core::Result<()> {
        Err(CoreError::other("worktree is locked"))
    }
    async fn delete_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Err(CoreError::other("branch delete failed"))
    }
    async fn fetch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn merge_ref(&self, _: &Path, _: &str) -> usine_core::Result<MergeOutcome> {
        Ok(MergeOutcome::Clean)
    }
    async fn create_worktree(
        &self,
        _: &Path,
        _: &str,
        _: &Path,
        _: &str,
    ) -> usine_core::Result<()> {
        Ok(())
    }
    async fn worktree_add_existing(&self, _: &Path, _: &str, _: &Path) -> usine_core::Result<()> {
        Ok(())
    }
    async fn worktree_add_detached(&self, _: &Path, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn fetch_pr(&self, _: &Path, _: u64, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn reset_mixed(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn rename_branch(&self, _: &Path, _: &str, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn commit_all(&self, _: &Path, _: &str) -> usine_core::Result<bool> {
        Ok(true)
    }
    async fn push(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
}

/// Await events until `f` returns `Some`, failing the test on timeout.
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

/// Seed a card sitting in `ReadyToMerge` with an open PR, branch and worktree.
fn ready_to_merge_card(store: &Store, project_id: uuid::Uuid) -> Card {
    let mut card = Card::new(project_id, "t", "d", CardConfig::default());
    card.state = CardState::ReadyToMerge;
    card.branch = Some("feat/thing".into());
    card.worktree_path = Some(PathBuf::from("/tmp/usine-merge-cleanup/wt"));
    card.pr = Some(PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: None,
        reviewer_recorded: false,
    });
    store.upsert_card(&card).unwrap();
    card
}

/// Once GitHub has merged the PR, no local cleanup failure may hold the card
/// back — the work is on the base branch whether or not the branch got deleted.
#[tokio::test]
async fn a_merged_card_reaches_done_even_when_every_cleanup_step_fails() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-cleanup"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = ready_to_merge_card(&store, project.id);

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(CleanupFailsForge),
        git: Arc::new(CleanupFailsGit),
    });
    handle.send(ExecutorCommand::Merge {
        card_id: card.id,
        delete_branch: true,
        force: false,
    });

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            matches!(c.state, CardState::Done).then_some(())
        }
        _ => None,
    })
    .await;
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::Done
    ));
}

/// Cleanup that failed is reported, not swallowed — an orphan worktree the user
/// never hears about is how you end up with six of them.
#[tokio::test]
async fn failed_cleanup_warns_the_user() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-cleanup"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = ready_to_merge_card(&store, project.id);

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(CleanupFailsForge),
        git: Arc::new(CleanupFailsGit),
    });
    handle.send(ExecutorCommand::Merge {
        card_id: card.id,
        delete_branch: true,
        force: false,
    });

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: usine_core::Severity::Warning,
            message,
        } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(
        msg.contains("worktree"),
        "worktree failure must surface: {msg}"
    );
    assert!(
        msg.contains("remote branch"),
        "remote failure must surface: {msg}"
    );
}

/// A card whose PR merged on a previous attempt (the merge landed, cleanup blew
/// up) must still be able to finish: retrying Merge sees `is_merged` and
/// completes the transition instead of erroring forever.
#[tokio::test]
async fn an_already_merged_pr_still_completes_the_card() {
    struct AlreadyMergedForge;
    #[async_trait]
    impl Forge for AlreadyMergedForge {
        async fn merge(&self, _: &Path, _: u64) -> usine_core::Result<()> {
            Err(CoreError::forge("Pull request #7 is already merged"))
        }
        async fn is_merged(&self, _: &Path, _: u64) -> usine_core::Result<bool> {
            Ok(true)
        }
        async fn merge_status(&self, _: &Path, _: u64) -> usine_core::Result<MergeStatus> {
            Ok(MergeStatus::Mergeable)
        }
        async fn delete_remote_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
            Ok(())
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
        async fn fetch_comments(&self, r: &Path, n: u64) -> usine_core::Result<Vec<ReviewComment>> {
            SimForge.fetch_comments(r, n).await
        }
        async fn list_review_prs(
            &self,
            r: &Path,
            a: &[String],
        ) -> usine_core::Result<Vec<PrSummary>> {
            SimForge.list_review_prs(r, a).await
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
        async fn list_submitted_reviews(
            &self,
            r: &Path,
            n: u64,
        ) -> usine_core::Result<Vec<ReviewSummary>> {
            SimForge.list_submitted_reviews(r, n).await
        }
        async fn reply_to_comment(
            &self,
            r: &Path,
            n: u64,
            c: u64,
            b: &str,
        ) -> usine_core::Result<()> {
            SimForge.reply_to_comment(r, n, c, b).await
        }
        async fn mark_ready(&self, r: &Path, n: u64) -> usine_core::Result<()> {
            SimForge.mark_ready(r, n).await
        }
        async fn resolve_threads(&self, r: &Path, n: u64, c: &[u64]) -> usine_core::Result<usize> {
            SimForge.resolve_threads(r, n, c).await
        }
        async fn list_threads(
            &self,
            r: &Path,
            n: u64,
        ) -> usine_core::Result<Vec<usine_core::ReviewThread>> {
            SimForge.list_threads(r, n).await
        }
    }

    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-cleanup"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = ready_to_merge_card(&store, project.id);

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(AlreadyMergedForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::Merge {
        card_id: card.id,
        delete_branch: true,
        force: false,
    });

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            matches!(c.state, CardState::Done).then_some(())
        }
        _ => None,
    })
    .await;
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::Done
    ));
}
