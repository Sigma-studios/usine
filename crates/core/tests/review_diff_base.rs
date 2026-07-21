//! Proves a contributor's PR is diffed against the base the *forge* uses, over
//! REAL git: an `origin` that has moved on, a clone whose `dev` is behind it.
//!
//! The bug this pins down: `git fetch origin +pull/<n>/head:<branch>` refreshes
//! only the PR head, and ref resolution preferred a local `dev` — so the merge
//! base landed at the stale local tip and every commit merged into `dev` since
//! the last pull was attributed to the contributor. A one-line PR read as
//! thousands of changed lines.

use std::path::Path;
use std::process::Command;

use usine_core::{compute_branch_diff, remote_tracking_base};

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

/// The PR's local branch, as `ReviewTask::local_branch` names it.
const PR_BRANCH: &str = "usine-review/402";

#[test]
fn pr_diff_ignores_commits_merged_into_the_base_since_the_last_pull() {
    let tmp = tempfile::tempdir().unwrap();

    // The upstream repo, at the point the user last cloned it.
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q"]);
    identify(&upstream);
    std::fs::write(upstream.join("labels.ts"), "export const a = 1;\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-qm", "base"]);
    git(&upstream, &["branch", "-M", "dev"]);

    // The user's checkout: local `dev` and `origin/dev` both sit here, and
    // nothing below ever pulls, exactly like a machine that reviews PRs but
    // hasn't touched `dev` in days.
    let repo = tmp.path().join("repo");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            upstream.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    identify(&repo);

    // Upstream `dev` moves on: someone else's feature lands.
    std::fs::write(upstream.join("merged.ts"), "export const big = 2;\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-qm", "someone else's merged PR"]);

    // The PR under review forks from the *new* dev and touches one file.
    git(&upstream, &["checkout", "-qb", "fix/labels"]);
    std::fs::write(upstream.join("labels.ts"), "export const a = 2;\n").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-qm", "fix: update labels"]);
    git(&upstream, &["checkout", "-q", "dev"]);

    // What the executor does: fetch the PR head into its stable local branch...
    git(
        &repo,
        &[
            "fetch",
            "origin",
            &format!("+refs/heads/fix/labels:{PR_BRANCH}"),
        ],
    );
    // ...then refresh the remote-tracking refs, so `origin/dev` is current.
    git(&repo, &["fetch", "origin"]);

    let files = |base: &str| {
        let mut names: Vec<String> = compute_branch_diff(&repo, base, PR_BRANCH)
            .expect("diff computes")
            .files
            .iter()
            .filter_map(|f| f.new_path.clone())
            .collect();
        names.sort();
        names
    };

    // The base as the forge sees it: only the PR's own file.
    let base = remote_tracking_base(&repo, "dev");
    assert_eq!(base, "origin/dev", "a fetched origin/dev must win");
    assert_eq!(files(&base), vec!["labels.ts".to_string()]);

    // The regression, stated as the property it violates: resolving through the
    // stale local `dev` drags the already-merged commit into the PR's diff.
    assert!(
        files("dev").contains(&"merged.ts".to_string()),
        "local dev is behind, so it must be the ref we do NOT diff against"
    );
}

#[test]
fn base_falls_back_to_the_bare_name_without_a_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("solo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    identify(&repo);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["branch", "-M", "dev"]);

    // No `origin/dev` to prefer — the local branch is all there is, and naming a
    // nonexistent `origin/dev` would fail the diff outright.
    assert_eq!(remote_tracking_base(&repo, "dev"), "dev");
    assert_eq!(
        remote_tracking_base(Path::new("/nonexistent"), "dev"),
        "dev"
    );
}
