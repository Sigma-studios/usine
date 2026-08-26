//! The in-progress edit buffer for one PR review's drafted comments.
//!
//! Validation happens in two places — the detail panel's list and, GitHub-style,
//! inline in the diff viewer — and both must edit the *same* review. So the
//! buffer lives here as a global signal rather than in either component's local
//! state: unchecking a comment in the diff has to show up in the panel, and a
//! body edited in the panel has to be what the diff shows.
//!
//! It's seeded from the task's `AwaitingValidation` payload the first time that
//! review is rendered and dropped when the review changes or is published, so
//! one review's edits can never leak into the next.
//!
//! This is the same origin-keyed pattern as `drafts::use_draft_of`, kept
//! separate because two surfaces edit it through module-level setters rather
//! than one component's signal; folding it onto the draft store is possible
//! but low value.

use dioxus::prelude::*;
use usine_core::{DraftComment, ReviewEvent};
use uuid::Uuid;

/// The user's working copy of a drafted review: which comments to send, their
/// (possibly edited) bodies, the overall summary, and the verdict.
#[derive(Clone, PartialEq)]
pub(crate) struct DraftEdits {
    /// The review this buffer belongs to. A different id means a different
    /// review, and the buffer is reseeded rather than reused.
    pub review_id: Uuid,
    /// The comments exactly as the agent drafted them, kept untouched alongside
    /// the edits. This is what makes reseeding decidable: the *edited* comments
    /// legitimately diverge from the drafted ones, so they can't tell a stale
    /// buffer from a well-used one — but a fresh agent run produces a different
    /// origin, and that's the signal to start over.
    origin: Vec<DraftComment>,
    pub comments: Vec<DraftComment>,
    pub summary: String,
    pub verdict: ReviewEvent,
}

static DRAFTS: GlobalSignal<Option<DraftEdits>> = Signal::global(|| None);

/// Seed the buffer for `review_id` unless it already holds edits for this exact
/// drafted review. Idempotent, so it's safe to call from every render of either
/// surface.
///
/// Re-reviewing the same PR (retry after a failure, or a second pass over new
/// commits) reuses the review id but produces new comments, so keying on the id
/// alone would show the previous run's drafts — hence the origin comparison.
pub(crate) fn ensure_seeded(
    review_id: Uuid,
    comments: &[DraftComment],
    summary: &str,
    verdict: ReviewEvent,
) {
    let needs_seed = DRAFTS
        .read()
        .as_ref()
        .map(|d| d.review_id != review_id || d.origin != comments)
        .unwrap_or(true);
    if needs_seed {
        *DRAFTS.write() = Some(DraftEdits {
            review_id,
            origin: comments.to_vec(),
            comments: comments.to_vec(),
            summary: summary.to_string(),
            verdict,
        });
    }
}

/// The current buffer, if it belongs to `review_id`.
pub(crate) fn edits_for(review_id: Uuid) -> Option<DraftEdits> {
    DRAFTS.read().clone().filter(|d| d.review_id == review_id)
}

/// Drop the buffer (after publishing or dismissing).
pub(crate) fn clear() {
    *DRAFTS.write() = None;
}

/// Flip a comment's include/skip checkbox.
pub(crate) fn toggle(index: usize) {
    if let Some(d) = DRAFTS.write().as_mut() {
        if let Some(c) = d.comments.get_mut(index) {
            c.selected = !c.selected;
        }
    }
}

/// Replace a comment's body with the user's edit.
pub(crate) fn set_body(index: usize, body: String) {
    if let Some(d) = DRAFTS.write().as_mut() {
        if let Some(c) = d.comments.get_mut(index) {
            c.body = body;
        }
    }
}

/// Replace a comment's criticality with the maintainer's judgement. The empty
/// string clears it — that comment then publishes untagged, verbatim.
pub(crate) fn set_severity(index: usize, severity: String) {
    if let Some(d) = DRAFTS.write().as_mut() {
        if let Some(c) = d.comments.get_mut(index) {
            c.severity = severity;
        }
    }
}

pub(crate) fn set_summary(summary: String) {
    if let Some(d) = DRAFTS.write().as_mut() {
        d.summary = summary;
    }
}

pub(crate) fn set_verdict(verdict: ReviewEvent) {
    if let Some(d) = DRAFTS.write().as_mut() {
        d.verdict = verdict;
    }
}

/// The DOM id of a comment's inline thread in the diff viewer, so "jump to" can
/// scroll to it from the panel.
pub(crate) fn anchor_id(index: usize) -> String {
    format!("draftcomment-{index}")
}
