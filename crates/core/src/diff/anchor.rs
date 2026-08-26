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

/// Split a review's drafts into the comments GitHub will accept inline and a
/// review body carrying the rest.
///
/// GitHub validates a review atomically: one comment anchored outside the PR's
/// diff and the whole POST is refused (HTTP 422), taking every other comment
/// down with it. File-level comments can't go inline at all — the reviews
/// endpoint has no `subject_type`; that belongs to the standalone comment API.
/// So everything unpostable is folded into the review body instead, each with
/// its `path:line`, which is exactly what the diff viewer's unplaced-comments
/// banner promises.
///
/// `diff` is best-effort: `None` (it couldn't be computed) folds only the
/// drafts that are unpostable regardless of the diff — the line-less ones —
/// and leaves the line-anchored rest for GitHub to judge.
pub fn fold_unanchorable(
    diff: Option<&DiffData>,
    drafts: Vec<DraftComment>,
    body: &str,
) -> (Vec<DraftComment>, String) {
    let postable: Vec<bool> = match diff {
        Some(data) => {
            // Inline-postable is exactly "landed on a diff line" — file-level
            // and unplaced drafts both fold.
            let anchors = anchor_drafts(data, &drafts);
            let mut ok = vec![false; drafts.len()];
            for indices in anchors.by_line.values() {
                for &ci in indices {
                    ok[ci] = true;
                }
            }
            ok
        }
        None => drafts.iter().map(|d| d.line.is_some()).collect(),
    };
    let (inline, folded): (Vec<_>, Vec<_>) = {
        let mut ok = postable.into_iter();
        drafts.into_iter().partition(|_| ok.next().unwrap_or(false))
    };
    if folded.is_empty() {
        return (inline, body.to_string());
    }
    let mut out = body.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n---\n");
    }
    out.push_str("Comments that couldn't be attached to a diff line:\n");
    for d in &folded {
        out.push('\n');
        match d.line {
            Some(l) => out.push_str(&format!("**`{}:{l}`** — {}\n", d.path, d.body)),
            None => out.push_str(&format!("**`{}`** — {}\n", d.path, d.body)),
        }
    }
    (inline, out)
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

    // --- fold_unanchorable --------------------------------------------------

    #[test]
    fn fold_keeps_anchorable_comments_inline_and_body_untouched() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(10), Some(11)])],
        };
        let (inline, body) =
            fold_unanchorable(Some(&data), vec![draft("src/a.rs", Some(11))], "LGTM");
        assert_eq!(inline.len(), 1);
        assert_eq!(body, "LGTM");
    }

    #[test]
    fn fold_moves_out_of_diff_line_into_body() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(10), Some(11)])],
        };
        let drafts = vec![draft("src/a.rs", Some(11)), draft("src/a.rs", Some(400))];
        let (inline, body) = fold_unanchorable(Some(&data), drafts, "Summary.");
        // The in-diff comment stays inline; the out-of-diff one rides the body
        // with its path:line so nothing is lost.
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].line, Some(11));
        assert!(body.starts_with("Summary.\n\n---\n"));
        assert!(body.contains("**`src/a.rs:400`**"));
    }

    /// Publishing tags a rated comment *before* folding, so a comment that ends
    /// up in the review body carries its criticality there too.
    #[test]
    fn fold_keeps_a_tagged_body_intact_in_the_review_body() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(10)])],
        };
        let mut d = draft("src/a.rs", Some(400));
        d.body = "**\u{1F534} Critical:** Guard this unwrap.".into();
        let (inline, body) = fold_unanchorable(Some(&data), vec![d], "");
        assert!(inline.is_empty());
        assert!(
            body.contains("**`src/a.rs:400`** \u{2014} **\u{1F534} Critical:** Guard this unwrap."),
            "{body}"
        );
    }

    #[test]
    fn fold_moves_unknown_path_into_body() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(1)])],
        };
        let (inline, body) =
            fold_unanchorable(Some(&data), vec![draft("src/nope.rs", Some(1))], "");
        assert!(inline.is_empty());
        assert!(body.contains("**`src/nope.rs:1`**"));
        // An empty summary doesn't grow a dangling separator.
        assert!(!body.starts_with("\n"));
        assert!(!body.contains("---"));
    }

    /// The reviews endpoint has no file-level comments, so a line-less draft
    /// folds even though its path is in the diff.
    #[test]
    fn fold_moves_file_level_comment_into_body() {
        let data = DiffData {
            files: vec![file("src/a.rs", &[Some(1)])],
        };
        let (inline, body) = fold_unanchorable(Some(&data), vec![draft("src/a.rs", None)], "S");
        assert!(inline.is_empty());
        assert!(body.contains("**`src/a.rs`**"));
    }

    /// Without a diff to judge against, only the certainly-unpostable (line-less)
    /// drafts fold; line-anchored ones are left for GitHub to validate.
    #[test]
    fn fold_without_diff_only_folds_line_less_drafts() {
        let drafts = vec![draft("src/a.rs", Some(400)), draft("src/b.rs", None)];
        let (inline, body) = fold_unanchorable(None, drafts, "S");
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].line, Some(400));
        assert!(body.contains("**`src/b.rs`**"));
    }
}
