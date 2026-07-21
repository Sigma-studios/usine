//! Proves `compute_card_diff` against REAL git: the structure, statuses, line
//! kinds/numbers, syntax highlighting, and the binary/empty guards. SimGit is a
//! no-op, so only a real repo exercises the git2 tree-vs-tree walk.

use std::path::Path;
use std::process::Command;

use usine_core::{compute_card_diff, DiffLineKind, FileStatus};

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

/// A repo with a `dev` base commit and a `feat/x` branch that modifies a Rust
/// file, adds a TypeScript file, and adds a binary blob.
fn setup(repo: &Path) {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.dev"]);
    git(repo, &["config", "user.name", "t"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("foo.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
    std::fs::write(repo.join("keep.ts"), "export const a = 1;\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "base"]);
    git(repo, &["branch", "-M", "dev"]);

    git(repo, &["checkout", "-qb", "feat/x"]);
    // Modify the Rust file (a removed + added line pair over context).
    std::fs::write(repo.join("foo.rs"), "fn main() {\n    let y = 2;\n}\n").unwrap();
    // Add a new TypeScript file.
    std::fs::write(repo.join("bar.ts"), "const greeting: string = \"hi\";\n").unwrap();
    // Add a binary blob (embedded NUL forces git's binary detection).
    std::fs::write(repo.join("logo.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "feat"]);
}

#[test]
fn computes_structured_highlighted_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    setup(&repo);

    let diff = compute_card_diff(&repo, "dev", "feat/x").expect("diff computes");

    // All three changed files are present; keep.ts (untouched) is not.
    let by_new = |name: &str| {
        diff.files
            .iter()
            .find(|f| f.new_path.as_deref() == Some(name))
    };
    assert_eq!(diff.files.len(), 3, "foo.rs + bar.ts + logo.bin");
    assert!(by_new("keep.ts").is_none(), "unchanged file excluded");

    // The Rust file: modified, highlighted, one added + one removed line.
    let foo = by_new("foo.rs").expect("foo.rs in diff");
    assert_eq!(foo.status, FileStatus::Modified);
    assert!(foo.highlighted, "a small .rs file is highlighted");
    assert!(!foo.binary);
    assert_eq!((foo.added, foo.removed), (1, 1));

    let lines: Vec<_> = foo.hunks.iter().flat_map(|h| &h.lines).collect();
    let added = lines
        .iter()
        .find(|l| l.kind == DiffLineKind::Added)
        .expect("an added line");
    // Added line: only the new-side number is set.
    assert!(added.new_no.is_some() && added.old_no.is_none());
    // Highlighting produced at least one colored run.
    assert!(
        added.tokens.iter().any(|t| t.color.is_some()),
        "added Rust line is syntax-highlighted: {:?}",
        added.tokens
    );
    // Token text carries no trailing newline.
    assert!(!added.tokens.iter().any(|t| t.text.contains('\n')));
    // A context line carries both old and new line numbers.
    let ctx = lines
        .iter()
        .find(|l| l.kind == DiffLineKind::Context)
        .expect("a context line");
    assert!(ctx.old_no.is_some() && ctx.new_no.is_some());

    // The TypeScript file: added and highlighted (proves the two-face TS syntax).
    let bar = by_new("bar.ts").expect("bar.ts in diff");
    assert_eq!(bar.status, FileStatus::Added);
    assert!(bar.highlighted);
    let bar_tokens: Vec<_> = bar
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .flat_map(|l| &l.tokens)
        .collect();
    assert!(
        bar_tokens.iter().any(|t| t.color.is_some()),
        "TypeScript is highlighted"
    );

    // The binary file: flagged, no hunks.
    let bin = by_new("logo.bin").expect("logo.bin in diff");
    assert!(bin.binary, "NUL-containing blob is binary");
    assert!(bin.hunks.is_empty());
}

#[test]
fn branch_with_no_commits_ahead_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    setup(&repo);

    // A branch forked from dev with no commits of its own → nothing ahead.
    git(&repo, &["checkout", "-q", "dev"]);
    git(&repo, &["checkout", "-qb", "feat/empty"]);
    let diff = compute_card_diff(&repo, "dev", "feat/empty").expect("diff computes");
    assert!(diff.files.is_empty(), "no committed work over base");
}
