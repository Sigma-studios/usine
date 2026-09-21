//! Proves, against REAL git, that a fork PR reached only through its merge ref
//! (Azure DevOps' `refs/pull/<n>/merge`) is checked out at the fork's own head.
//!
//! The merge commit carries the target branch's changes too, so where both
//! sides touched a file its line numbers drift from the source's — and review
//! comments are anchored on the source's lines. SimGit is a no-op, so only a
//! real repo can check the second-parent resolution.

use std::path::Path;
use std::process::Command;

use usine_core::{GitOps, RealGit};

/// Run `git <args>` in `dir`, asserting success; stdout, trimmed.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn a_fork_merge_ref_resolves_to_the_source_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let remote = root.join("origin.git");
    let work = root.join("work");
    git(root, &["init", "-q", "-b", "main", work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@example.com"]);
    git(&work, &["config", "user.name", "t"]);
    std::fs::write(work.join("f.txt"), "a\nb\n").unwrap();
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "init"]);

    // The fork's change, then the target moving on under it.
    git(&work, &["checkout", "-q", "-b", "fork"]);
    std::fs::write(work.join("f.txt"), "a\nb\nfork\n").unwrap();
    git(&work, &["commit", "-qam", "fork"]);
    let fork_head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("f.txt"), "main\na\nb\n").unwrap();
    git(&work, &["commit", "-qam", "main moved"]);
    git(&work, &["merge", "-q", "--no-edit", "fork"]);
    let merge = git(&work, &["rev-parse", "HEAD"]);

    // What the host publishes: the merge ref, but not the fork's branch.
    git(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git(&remote, &["update-ref", "refs/pull/7/merge", &merge]);
    git(&remote, &["update-ref", "-d", "refs/heads/fork"]);
    git(
        &remote,
        &["update-ref", "refs/heads/main", &format!("{merge}~1")],
    );
    let clone = root.join("clone");
    git(
        root,
        &[
            "clone",
            "-q",
            remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );

    RealGit
        .fetch_merge_source(&clone, "refs/pull/7/merge", "review/pr-7")
        .await
        .expect("fetch the merge ref");
    assert_eq!(git(&clone, &["rev-parse", "review/pr-7"]), fork_head);
    assert_eq!(
        git(&clone, &["show", "review/pr-7:f.txt"]),
        "a\nb\nfork",
        "the source's lines, not the merged result's"
    );
}
