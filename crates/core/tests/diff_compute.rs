//! Proves `compute_card_diff` against REAL git: the structure, statuses, line
//! kinds/numbers, syntax highlighting, and the binary/empty guards. SimGit is a
//! no-op, so only a real repo exercises the git2 tree-vs-tree walk.

use std::path::Path;
use std::process::Command;

use usine_core::{compute_card_diff, snapshot_blob, DiffLineKind, FileStatus};

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

/// A repo whose `docs` branch adds `README.md` + `src/a.rs` and deletes
/// `OLD.md` over `dev`. Returns `OLD.md`'s blob id on `dev`.
fn setup_markdown(repo: &Path) -> String {
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.dev"]);
    git(repo, &["config", "user.name", "t"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("OLD.md"), "# Old\n\ngone soon\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "base"]);
    git(repo, &["branch", "-M", "dev"]);
    let out = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "dev:OLD.md"])
        .output()
        .unwrap();
    let old_blob = String::from_utf8(out.stdout).unwrap().trim().to_string();

    git(repo, &["checkout", "-qb", "docs"]);
    std::fs::remove_file(repo.join("OLD.md")).unwrap();
    std::fs::write(repo.join("README.md"), "# Title\n\n- one\n- two\n").unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "docs"]);
    old_blob
}

#[test]
fn markdown_files_carry_a_preview_and_every_file_a_blob() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let old_blob = setup_markdown(&repo);

    let diff = compute_card_diff(&repo, "dev", "docs").expect("diff computes");
    let find = |name: &str| {
        diff.files
            .iter()
            .find(|f| f.new_path.as_deref() == Some(name) || f.old_path.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name} in diff"))
    };

    let readme = find("README.md");
    assert_eq!(readme.preview.as_deref(), Some("# Title\n\n- one\n- two\n"));
    assert!(readme.blob.is_some());

    let code = find("src/a.rs");
    assert!(code.preview.is_none(), "non-markdown has no preview");
    assert!(code.blob.is_some());

    let gone = find("OLD.md");
    assert_eq!(gone.status, FileStatus::Deleted);
    assert!(gone.preview.is_none(), "a deletion has no preview");
    assert_eq!(
        gone.blob.as_deref(),
        Some(old_blob.as_str()),
        "deletion → old blob"
    );
}

#[test]
fn oversized_markdown_gets_no_preview() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.as_path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t.dev"]);
    git(repo, &["config", "user.name", "t"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("CHANGELOG.md"), "# Changes\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "base"]);
    git(repo, &["branch", "-M", "dev"]);
    git(repo, &["checkout", "-qb", "big"]);
    // Grown past the 512 KiB cap. A modified file is no rename candidate, so
    // its tree-to-tree delta reports size 0: the cap must read the blob.
    std::fs::write(repo.join("CHANGELOG.md"), "- item\n".repeat(100_000)).unwrap();
    git(repo, &["commit", "-qam", "grow"]);

    let diff = compute_card_diff(repo, "dev", "big").expect("diff computes");
    assert_eq!(diff.files.len(), 1);
    assert!(diff.files[0].preview.is_none(), "over the cap → no preview");
}

#[test]
fn snapshot_blob_writes_a_read_only_named_copy() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let old_blob = setup_markdown(&repo);

    let path = snapshot_blob(&repo, &old_blob, "OLD.md").expect("snapshot");
    assert_eq!(path.file_name().unwrap(), "OLD.md");
    assert_eq!(std::fs::read(&path).unwrap(), b"# Old\n\ngone soon\n");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o444, "read-only, never executable");
    }

    // Content-addressed: a second call reuses the same copy.
    let again = snapshot_blob(&repo, &old_blob, "OLD.md").expect("snapshot again");
    assert_eq!(again, path);

    for bad in ["", ".", "..", "a/b.md", "../x"] {
        assert!(
            snapshot_blob(&repo, &old_blob, bad).is_err(),
            "rejects {bad:?}"
        );
    }
}
