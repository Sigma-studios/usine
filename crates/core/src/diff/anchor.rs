//! Placing a review's drafted comments onto the lines of a diff.
//!
//! Validating a PR review happens inside the diff viewer, where each drafted
//! comment is threaded under the line it's about. That requires resolving a
//! comment's `(path, line)` — which the agent wrote, and which GitHub will later
//! interpret — against the structure the diff walk produced.
//!
//! The interesting case is the one that *doesn't* resolve. GitHub rejects an
//! inline review comment whose line isn't part of the diff, so a comment the
//! agent anchored to a line outside every hunk (or to a file the PR doesn't
//! touch) would fail at publish time. Those are reported as unplaced rather than
//! dropped, so the UI can surface them instead of silently losing them.

use std::collections::HashMap;

use crate::domain::model::DraftComment;

use super::DiffData;

/// Where each drafted comment lands in a rendered diff. Every index refers to a
/// position in the `drafts` slice passed to [`anchor_drafts`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DraftAnchors {
    /// `(file index, hunk index, line index)` → comment indices on that line.
    pub by_line: HashMap<(usize, usize, usize), Vec<usize>>,
    /// File index → comment indices that name the file but no line.
    pub file_level: HashMap<usize, Vec<usize>>,
    /// File index → how many comments landed anywhere in that file.
    pub per_file: HashMap<usize, usize>,
    /// Comments whose path/line isn't in this diff at all. GitHub would refuse
    /// to post these inline.
    pub unplaced: Vec<usize>,
}

/// Map each drafted comment onto the diff line it annotates.
///
/// A comment's `line` is a line number in the *new* file (what GitHub's review
/// API expects), so it matches against `new_no`. A comment with no line is
/// file-level. Anything that doesn't resolve — a path not in the diff, a line
/// outside every hunk, or a line that only exists on the old side — lands in
/// [`DraftAnchors::unplaced`].
pub fn anchor_drafts(data: &DiffData, drafts: &[DraftComment]) -> DraftAnchors {
    let mut anchors = DraftAnchors::default();
    if drafts.is_empty() {
        return anchors;
    }

    // path → file index, for both sides so a comment on a renamed file's old
    // path still finds its way home. The new path wins on a collision.
    let mut by_path: HashMap<&str, usize> = HashMap::new();
    for (fi, f) in data.files.iter().enumerate() {
        if let Some(p) = f.new_path.as_deref() {
            by_path.insert(p, fi);
        }
    }
    for (fi, f) in data.files.iter().enumerate() {
        if let Some(p) = f.old_path.as_deref() {
            by_path.entry(p).or_insert(fi);
        }
    }

    // (file index, new-side line number) → where that line renders. Built once
    // over the whole diff rather than rescanned per comment: the viewer calls
    // this on every render, and a big diff with a long review would otherwise be
    // files × lines × comments of work per frame.
    let mut positions: HashMap<(usize, u32), (usize, usize)> = HashMap::new();
    for (fi, f) in data.files.iter().enumerate() {
        for (hi, hunk) in f.hunks.iter().enumerate() {
            for (li, line) in hunk.lines.iter().enumerate() {
                if let Some(n) = line.new_no {
                    positions.entry((fi, n)).or_insert((hi, li));
                }
            }
        }
    }

    for (ci, c) in drafts.iter().enumerate() {
        let Some(&fi) = by_path.get(c.path.as_str()) else {
            anchors.unplaced.push(ci);
            continue;
        };
        let Some(want) = c.line else {
            anchors.file_level.entry(fi).or_default().push(ci);
            *anchors.per_file.entry(fi).or_default() += 1;
            continue;
        };
        match positions.get(&(fi, want as u32)) {
            Some(&(hi, li)) => {
                anchors.by_line.entry((fi, hi, li)).or_default().push(ci);
                *anchors.per_file.entry(fi).or_default() += 1;
            }
            None => anchors.unplaced.push(ci),
        }
    }
    anchors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffFile, DiffHunk, DiffLine, DiffLineKind, FileStatus};

    /// A one-hunk file whose new-side lines are exactly `new_nos`.
    fn file(new_path: &str, new_nos: &[Option<u32>]) -> DiffFile {
        DiffFile {
            old_path: Some(new_path.to_string()),
            new_path: Some(new_path.to_string()),
            status: FileStatus::Modified,
            binary: false,
            highlighted: true,
            added: 0,
            removed: 0,
            hunks: vec![DiffHunk {
                header: "@@".into(),
                old_start: 1,
                new_start: 1,
                lines: new_nos
                    .iter()
                    .map(|n| DiffLine {
                        kind: if n.is_some() {
                            DiffLineKind::Added
                        } else {
                            DiffLineKind::Removed
                        },
                        old_no: None,
                        new_no: *n,
                        tokens: Vec::new(),
                    })
                    .collect(),
            }],
        }
    }

    fn draft(path: &str, line: Option<u64>) -> DraftComment {
        DraftComment {
            path: path.into(),
            line,
            body: "b".into(),
            severity: String::new(),
            selected: true,
        }
    }

    #[test]
    fn comment_lands_on_its_line() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(10), Some(11), Some(12)])],
        };
        let a = anchor_drafts(&data, &[draft("src/a.rs", Some(11))]);
        // File 0, hunk 0, the second line.
        assert_eq!(a.by_line.get(&(0, 0, 1)), Some(&vec![0]));
        assert_eq!(a.per_file.get(&0), Some(&1));
        assert!(a.unplaced.is_empty());
    }

    #[test]
    fn line_outside_the_diff_is_unplaced_not_dropped() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(10), Some(11)])],
        };
        // Line 400 is in the file but not in any hunk — GitHub would reject it.
        let a = anchor_drafts(&data, &[draft("src/a.rs", Some(400))]);
        assert_eq!(a.unplaced, vec![0]);
        assert!(a.by_line.is_empty());
        assert_eq!(a.per_file.get(&0), None);
    }

    #[test]
    fn unknown_path_is_unplaced() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(1)])],
        };
        let a = anchor_drafts(&data, &[draft("src/nope.rs", Some(1))]);
        assert_eq!(a.unplaced, vec![0]);
    }

    /// A removed line has no new-side number, so a comment can't anchor to it.
    #[test]
    fn removed_only_line_cannot_anchor() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[None, Some(5)])],
        };
        let a = anchor_drafts(&data, &[draft("src/a.rs", Some(5))]);
        assert_eq!(a.by_line.get(&(0, 0, 1)), Some(&vec![0]));
        assert!(a.unplaced.is_empty());
    }

    #[test]
    fn line_less_comment_is_file_level() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(1)])],
        };
        let a = anchor_drafts(&data, &[draft("src/a.rs", None)]);
        assert_eq!(a.file_level.get(&0), Some(&vec![0]));
        assert_eq!(a.per_file.get(&0), Some(&1));
        assert!(a.unplaced.is_empty());
    }

    #[test]
    fn several_comments_can_share_one_line() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(7)])],
        };
        let a = anchor_drafts(
            &data,
            &[draft("src/a.rs", Some(7)), draft("src/a.rs", Some(7))],
        );
        assert_eq!(a.by_line.get(&(0, 0, 0)), Some(&vec![0, 1]));
        assert_eq!(a.per_file.get(&0), Some(&2));
    }

    /// A comment written against a renamed file's old path still finds it.
    #[test]
    fn old_path_resolves_for_a_rename() {
        let mut f = file("src/new.rs", &[Some(3)]);
        f.old_path = Some("src/old.rs".into());
        f.status = FileStatus::Renamed;
        let data = DiffData { files: vec![f] };
        let a = anchor_drafts(&data, &[draft("src/old.rs", Some(3))]);
        assert_eq!(a.by_line.get(&(0, 0, 0)), Some(&vec![0]));
    }

    #[test]
    fn no_drafts_means_no_work() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(1)])],
        };
        assert_eq!(anchor_drafts(&data, &[]), DraftAnchors::default());
    }
}
