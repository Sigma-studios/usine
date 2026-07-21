//! Proves, against REAL git, that opening a PR from a branch whose name differs
//! only in capitalisation from an existing ref *directory* still works.
//!
//! Git stores a loose ref as a file, so on a case-insensitive filesystem (APFS,
//! HFS+, NTFS) asking for `Fix/thing` when a `fix/` directory exists writes the
//! ref into `fix/` — `git branch -m` exits 0, but the requested name resolves to
//! nothing, HEAD is left dangling, `git push` fails with "cannot be resolved to
//! branch", and renaming to the lowercase name fails with "already exists".
//! SimGit is a no-op, so only a real repo can catch this.

use std::path::Path;
use std::process::Command;

use usine_core::infra::git::canonicalize_branch_case;
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
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Whether `dir` lives on a filesystem that ignores capitalisation — the
/// condition that makes the folding happen at all.
fn case_insensitive(dir: &Path) -> bool {
    std::fs::create_dir(dir.join("CaseProbe")).expect("create probe dir");
    let folded = dir.join("caseprobe").is_dir();
    std::fs::remove_dir(dir.join("CaseProbe")).expect("remove probe dir");
    folded
}

/// A repo with a `fix/` ref directory already in it, on branch `usine/card-x`,
/// wired to a bare remote so the push in the original bug can be exercised.
fn repo_with_a_fix_directory(root: &Path) -> std::path::PathBuf {
    let remote = root.join("origin.git");
    let work = root.join("work");
    git(root, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    git(root, &["init", "-q", work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@example.com"]);
    git(&work, &["config", "user.name", "t"]);
    git(
        &work,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    std::fs::write(work.join("f.txt"), "hi").expect("write file");
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-qm", "init"]);
    // The ref *directory* that captures a differently-cased sibling.
    git(&work, &["branch", "fix/some-other-branch"]);
    git(&work, &["checkout", "-q", "-b", "usine/card-x"]);
    work
}

/// The end-to-end fix: canonicalising against the real branch list produces a
/// name that renames cleanly, leaves HEAD resolvable, and pushes.
#[tokio::test]
async fn canonicalised_branch_renames_and_pushes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let work = repo_with_a_fix_directory(tmp.path());

    let existing = RealGit
        .local_branches(&work)
        .await
        .expect("list local branches");
    assert!(existing.contains(&"fix/some-other-branch".to_string()));

    let target = canonicalize_branch_case("Fix/displayed-last-name-ordering", &existing);
    if case_insensitive(tmp.path()) {
        assert_eq!(target, "fix/displayed-last-name-ordering");
    }

    RealGit
        .rename_branch(&work, "usine/card-x", &target)
        .await
        .expect("rename branch");

    // The branch exists under exactly the name we asked for...
    let after = RealGit
        .local_branches(&work)
        .await
        .expect("list local branches");
    assert!(
        after.contains(&target),
        "branch missing after rename; git has {after:?}"
    );

    // ...HEAD still resolves (the dangling-HEAD half of the bug)...
    let out = Command::new("git")
        .current_dir(&work)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "HEAD does not resolve: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // ...and the push that originally failed now succeeds.
    RealGit.push(&work, &target).await.expect("push branch");
}

/// The bug itself, pinned: renaming to the raw capitalisation silently stores
/// the ref under a *different* name, which is why the executor verifies the
/// branch exists as named before recording it on the card.
#[tokio::test]
async fn raw_capitalisation_is_captured_by_the_existing_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    if !case_insensitive(tmp.path()) {
        return; // Nothing to fold on a case-sensitive filesystem.
    }
    let work = repo_with_a_fix_directory(tmp.path());

    // git reports success...
    RealGit
        .rename_branch(&work, "usine/card-x", "Fix/displayed-last-name-ordering")
        .await
        .expect("rename branch reports success");

    // ...but no branch by that name exists, which is what the executor's
    // post-rename check catches before it can be pushed or persisted.
    let after = RealGit
        .local_branches(&work)
        .await
        .expect("list local branches");
    assert!(
        !after.contains(&"Fix/displayed-last-name-ordering".to_string()),
        "expected the capitalised name to have been folded away; git has {after:?}"
    );
    assert!(after.contains(&"fix/displayed-last-name-ordering".to_string()));

    // And the original user-visible symptom, for the record.
    let out = Command::new("git")
        .current_dir(&work)
        .args(["push", "-u", "origin", "Fix/displayed-last-name-ordering"])
        .output()
        .expect("run git");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be resolved to branch"));
}
