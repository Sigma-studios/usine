//! Turn a card's committed work into a structured [`DiffData`] via git2.
//!
//! Diffs the fork point (`merge_base(base, branch)`) tree against the branch tip
//! tree — the card's own commits, and nothing from the base branch. This reads
//! only committed objects from the shared ODB, so it works even if the card's
//! worktree was cleaned up. Follows the repo's "git2 for inspection" rule.

use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};
use crate::infra::git::{merge_base, remote_tracking_base};

use super::highlight::Highlighter;
use super::lang::{is_markdown, lang_for_path};
use super::{DiffData, DiffFile, DiffHunk, DiffLine, DiffLineKind, FileStatus, Token};

/// Files larger than this (either side) are shown without syntax highlighting —
/// the diff structure is still produced, but tokens are plain.
const MAX_HIGHLIGHT_BYTES: u64 = 512 * 1024;

/// Markdown files larger than this (new side) get no rendered preview.
const MAX_PREVIEW_BYTES: u64 = 512 * 1024;

/// Compute a card's committed contribution over its fork point. `repo` is the
/// project's main checkout (shares the object DB with the card's worktree);
/// `base` is the project's effective base branch (the caller names it — the
/// project config, not re-detection, is the authority); `branch` is the card's
/// branch. An empty result means nothing is committed ahead of the base branch
/// — the caller maps that to `DiffState::Empty`.
///
/// The base is resolved remote-first (`origin/<base>` when it exists): card
/// branches are cut from the remote-tracking ref, so taking the fork point
/// against a stale local base would swell the diff with commits merged since
/// the last pull.
pub fn compute_card_diff(repo: &Path, base: &str, branch: &str) -> Result<DiffData> {
    let base = remote_tracking_base(repo, base);
    compute_branch_diff(repo, &base, branch)
}

/// Compute `branch`'s contribution over its fork point from an *explicit* base.
///
/// Same walk as [`compute_card_diff`], but the caller names the base: a
/// contributor's PR targets whatever branch it was opened against, and diffing
/// it against the repo's default instead would show every commit the base has
/// that the PR's target doesn't. Both refs resolve through `origin/<name>` when
/// no local branch exists, so a freshly fetched PR works without a checkout.
pub fn compute_branch_diff(repo: &Path, base: &str, branch: &str) -> Result<DiffData> {
    let fork_point = merge_base(repo, base, branch)?;
    diff_range(repo, &fork_point, branch)
}

fn diff_range(repo_path: &Path, base_hex: &str, branch: &str) -> Result<DiffData> {
    let repo = git2::Repository::open(repo_path)?;

    let base_tree = repo.find_commit(git2::Oid::from_str(base_hex)?)?.tree()?;
    // Resolve the branch tip, tolerating a name that only exists as origin/<name>.
    let head_obj = repo
        .revparse_single(branch)
        .or_else(|_| repo.revparse_single(&format!("origin/{branch}")))?;
    let head_tree = head_obj.peel_to_commit()?.tree()?;

    let mut opts = git2::DiffOptions::new();
    opts.context_lines(3).indent_heuristic(true);
    let mut diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut opts))?;

    // Coalesce delete+add pairs into renames/copies, matching CLI output.
    let mut find = git2::DiffFindOptions::new();
    find.renames(true).copies(true);
    diff.find_similar(Some(&mut find))?;

    let hl = Highlighter::global();
    let mut files = Vec::new();

    for (idx, delta) in diff.deltas().enumerate() {
        let status = map_status(delta.status());
        let old_path = delta.old_file().path().map(path_str);
        let new_path = delta.new_file().path().map(path_str);
        let lang = new_path
            .as_deref()
            .or(old_path.as_deref())
            .and_then(|p| lang_for_path(Path::new(p)));

        // The blob the file "is" here: the new side, or the old one for a
        // deletion. Kept even for binary/content-less files — an image or PDF
        // is exactly what the UI's "open in default app" is for.
        let new_id = delta.new_file().id();
        let old_id = delta.old_file().id();
        let blob = [new_id, old_id]
            .into_iter()
            .find(|id| !id.is_zero())
            .map(|id| id.to_string());

        // Detect binary from the blob content up front: for a tree-vs-tree diff
        // git2 doesn't reliably flag the delta, and it still emits a (hunk-less)
        // patch for a binary file, so we can't infer it from `Patch::from_diff`.
        if is_blob_binary(&repo, new_id) || is_blob_binary(&repo, old_id) {
            files.push(meta(
                old_path,
                new_path,
                status,
                true,
                false,
                0,
                0,
                Vec::new(),
                None,
                blob,
            ));
            continue;
        }

        let preview = markdown_preview(&repo, new_path.as_deref(), new_id);

        // A content-less change (mode-only / pure rename) yields no patch.
        let Some(patch) = git2::Patch::from_diff(&diff, idx)? else {
            files.push(meta(
                old_path,
                new_path,
                status,
                false,
                false,
                0,
                0,
                Vec::new(),
                preview,
                blob,
            ));
            continue;
        };

        let (_context, added, removed) = patch.line_stats()?;
        let size = delta.new_file().size().max(delta.old_file().size());
        let highlighted = lang.is_some() && size <= MAX_HIGHLIGHT_BYTES;

        let mut hunks = Vec::with_capacity(patch.num_hunks());
        for h in 0..patch.num_hunks() {
            let (hunk, _line_count) = patch.hunk(h)?;
            let header = String::from_utf8_lossy(hunk.header())
                .trim_end()
                .to_string();

            let n = patch.num_lines_in_hunk(h)?;
            let mut lines = Vec::with_capacity(n);
            for l in 0..n {
                let dl = patch.line_in_hunk(h, l)?;
                let kind = match dl.origin_value() {
                    git2::DiffLineType::Addition => DiffLineKind::Added,
                    git2::DiffLineType::Deletion => DiffLineKind::Removed,
                    git2::DiffLineType::Context => DiffLineKind::Context,
                    // Header / EOFNL / binary marker lines carry no code — skip.
                    _ => continue,
                };
                let content = String::from_utf8_lossy(dl.content());
                let tokens = if highlighted {
                    hl.line_tokens(lang, &content)
                } else {
                    vec![plain_token(&content)]
                };
                lines.push(DiffLine {
                    kind,
                    old_no: dl.old_lineno(),
                    new_no: dl.new_lineno(),
                    tokens,
                });
            }
            hunks.push(DiffHunk {
                header,
                old_start: hunk.old_start(),
                new_start: hunk.new_start(),
                lines,
            });
        }

        files.push(meta(
            old_path,
            new_path,
            status,
            false,
            highlighted,
            added as u32,
            removed as u32,
            hunks,
            preview,
            blob,
        ));
    }

    Ok(DiffData { files })
}

#[allow(clippy::too_many_arguments)]
fn meta(
    old_path: Option<String>,
    new_path: Option<String>,
    status: FileStatus,
    binary: bool,
    highlighted: bool,
    added: u32,
    removed: u32,
    hunks: Vec<DiffHunk>,
    preview: Option<String>,
    blob: Option<String>,
) -> DiffFile {
    DiffFile {
        old_path,
        new_path,
        status,
        binary,
        highlighted,
        added,
        removed,
        hunks,
        preview,
        blob,
    }
}

/// The new side's text for a markdown file within the preview cap. `None` for
/// a non-markdown path, a deletion (zero id), or an oversized blob. Invalid
/// UTF-8 is replaced rather than dropping the preview.
fn markdown_preview(
    repo: &git2::Repository,
    new_path: Option<&str>,
    new_id: git2::Oid,
) -> Option<String> {
    let path = new_path?;
    if new_id.is_zero() || !is_markdown(Path::new(path)) {
        return None;
    }
    // Size from the blob itself: a tree-to-tree delta reports 0 until the
    // patch has loaded it.
    let blob = repo.find_blob(new_id).ok()?;
    if blob.size() as u64 > MAX_PREVIEW_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(blob.content()).into_owned())
}

/// Write blob `oid_hex` of `repo` to a read-only temp file named `file_name`,
/// for opening a diffed file in its default app when no worktree copy exists
/// (a merged card, a deleted file, a PR that was never checked out).
///
/// The file lands at `$TMPDIR/usine-diff/<12 hex of the id>/<file_name>`: the
/// directory is content-addressed, so reopening reuses the copy, and the
/// original name keeps the OS's app choice right. `file_name` must be a bare
/// name. The copy is `0o444` — never executable, never mistaken for the real
/// file to edit.
pub fn snapshot_blob(repo: &Path, oid_hex: &str, file_name: &str) -> Result<PathBuf> {
    if file_name.is_empty()
        || file_name == "."
        || file_name == ".."
        || file_name.contains(['/', '\\'])
    {
        return Err(CoreError::other(format!(
            "not a bare file name: {file_name:?}"
        )));
    }
    let oid = git2::Oid::from_str(oid_hex)?;
    let hex = oid.to_string();
    let dir = std::env::temp_dir().join("usine-diff").join(&hex[..12]);
    let path = dir.join(file_name);
    if path.exists() {
        return Ok(path);
    }
    let repo = git2::Repository::open(repo)?;
    let blob = repo.find_blob(oid)?;
    std::fs::create_dir_all(&dir)?;
    // Written aside and renamed in, so a reused copy is never a half-write.
    let tmp = dir.join(format!(".{file_name}.{}.part", std::process::id()));
    std::fs::write(&tmp, blob.content())?;
    let mut perms = std::fs::metadata(&tmp)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o444);
    }
    #[cfg(not(unix))]
    perms.set_readonly(true);
    std::fs::set_permissions(&tmp, perms)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn plain_token(content: &str) -> Token {
    Token {
        text: content.trim_end_matches(['\n', '\r']).to_string(),
        color: None,
    }
}

/// Whether a blob (by Oid) is binary per git's heuristic (NUL scan). A zero Oid
/// (the missing side of an add/delete) is treated as non-binary.
fn is_blob_binary(repo: &git2::Repository, oid: git2::Oid) -> bool {
    !oid.is_zero() && repo.find_blob(oid).map(|b| b.is_binary()).unwrap_or(false)
}

fn map_status(d: git2::Delta) -> FileStatus {
    match d {
        git2::Delta::Added | git2::Delta::Untracked => FileStatus::Added,
        git2::Delta::Deleted => FileStatus::Deleted,
        git2::Delta::Renamed => FileStatus::Renamed,
        git2::Delta::Copied => FileStatus::Copied,
        _ => FileStatus::Modified,
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
