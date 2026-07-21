//! Proves, against REAL git, the core of the startup worktree reconciliation
//! (`reconcile_worktrees`): attaching a worktree to an existing branch
//! (`worktree_add_existing`) and writing/committing in it never touches the
//! user's main working tree. SimGit is a no-op, so only a real git repo can
//! catch a leak here.

use std::path::Path;
use std::process::Command;

use usine_core::infra::git::{current_branch, is_dirty};
use usine_core::{GitOps, RealGit};

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

#[tokio::test]
async fn write_fix_runs_off_the_main_working_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    // A repo with a `dev` base checked out and a `feature` branch carrying a
    // card's work.
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["branch", "-M", "dev"]);
    git(&repo, &["checkout", "-qb", "feature"]);
    std::fs::write(repo.join("feat.txt"), "f").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "feat"]);
    git(&repo, &["checkout", "-q", "dev"]);
    assert_eq!(current_branch(&repo).unwrap(), "dev");

    // The re-attach `reconcile_worktrees` performs for a card whose worktree
    // dir is gone: attach a fresh worktree on the existing branch.
    let gitops = RealGit;
    let wt = tmp.path().join("wt");
    gitops
        .worktree_add_existing(&repo, "feature", &wt)
        .await
        .unwrap();

    // Main repo stays on base and clean; the branch's file isn't in it.
    assert_eq!(current_branch(&repo).unwrap(), "dev");
    assert!(!is_dirty(&repo).await.unwrap(), "main tree must be clean");
    assert!(
        !repo.join("feat.txt").exists(),
        "feature file not in main tree"
    );

    // The worktree holds the feature branch and its file.
    assert_eq!(current_branch(&wt).unwrap(), "feature");
    assert!(wt.join("feat.txt").exists());

    // A "fix" that edits + commits in the worktree must NOT touch the main tree.
    std::fs::write(wt.join("fix.txt"), "fixed").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "fix"]);
    assert!(wt.join("fix.txt").exists());
    assert!(
        !repo.join("fix.txt").exists(),
        "the fix must never appear in the user's main working tree"
    );
    assert!(
        !is_dirty(&repo).await.unwrap(),
        "the fix must not dirty the main tree"
    );
    // The commit landed on `feature` (visible from the main repo's branch history),
    // so it reaches the PR, while the working tree stays put on `dev`.
    let log = Command::new("git")
        .current_dir(&repo)
        .args(["log", "--oneline", "feature"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&log.stdout).contains("fix"));
    assert_eq!(current_branch(&repo).unwrap(), "dev");
}
