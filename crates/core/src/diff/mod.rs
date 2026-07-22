//! Structured, syntax-highlighted git diff of a card's committed contribution.
//!
//! Computes `merge_base(base, branch)..branch` — the card's own commits over the
//! point they forked from the base branch — as a per-file / per-hunk / per-line
//! structure, tagging each line's runs with theme foreground colors so a Dioxus
//! panel can render it without re-highlighting on every frame.
//!
//! It mirrors the preview feature's shape: an executor handler ([`crate::agent`]'s
//! `compute_diff`) does the work off the async runtime and emits the result as an
//! [`crate::ExecutorEvent`]. The types here derive only `Debug, Clone, PartialEq,
//! Eq` — matching `PreviewStatus`, they flow over the in-process event channel and
//! are never serialized.

mod anchor;
mod compute;
mod highlight;
mod lang;

pub use anchor::{anchor_drafts, fold_unanchorable, DraftAnchors};
pub use compute::{compute_branch_diff, compute_card_diff};

/// A file's change kind (git2's `Delta`, collapsed to what the UI shows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

/// Which side of the diff a physical line belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

/// One highlighted run within a line. `color` is `#rrggbb` from the theme's
/// foreground, or `None` for unhighlighted text (unsupported/unknown language,
/// a binary file, or a file over the highlight size cap).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub color: Option<String>,
}

/// One physical line of a hunk. For a context line both `old_no` and `new_no`
/// are set; an added line has only `new_no`, a removed line only `old_no`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub tokens: Vec<Token>,
}

/// A contiguous change region. `header` is the raw `@@ -a,b +c,d @@ …` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

/// A single changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// Path on the base side (`None` for a pure addition).
    pub old_path: Option<String>,
    /// Path on the head side (`None` for a deletion).
    pub new_path: Option<String>,
    pub status: FileStatus,
    /// A binary blob: no hunks are emitted.
    pub binary: bool,
    /// `false` when the language is unsupported or the file exceeded the size
    /// cap: lines are still present but their tokens are plain (`color: None`).
    pub highlighted: bool,
    pub added: u32,
    pub removed: u32,
    pub hunks: Vec<DiffHunk>,
}

/// The whole diff: the card's committed contribution over its fork point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffData {
    pub files: Vec<DiffFile>,
}

/// UI-facing lifecycle of a card's diff — mirrors `PreviewStatus`. Held in a UI
/// signal, replaced wholesale on each `DiffUpdated` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffState {
    /// The diff is being computed off-thread.
    Computing,
    /// Ready to render.
    Ready(DiffData),
    /// No committed work over the base branch (or the card has no branch yet).
    Empty,
    /// Computation failed; carries the reason for the panel.
    Failed(String),
}
