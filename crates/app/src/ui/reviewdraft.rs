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
//! It's persisted alongside the draft store (`drafts::snapshot`/`load`): every
//! change bumps the shared revision, and a restored buffer is kept only while
//! its review exists and its `origin` still matches the drafted comments.
//!
//! This is the same origin-keyed pattern as `drafts::use_draft_of`, kept
//! separate because two surfaces edit it through module-level setters rather
//! than one component's signal; folding it onto the draft store is possible
//! but low value.

use dioxus::prelude::*;
use usine_core::{DraftComment, ReviewEvent, StoredReviewEdits};
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
        super::drafts::bump();
    }
}

/// The current buffer, if it belongs to `review_id`.
pub(crate) fn edits_for(review_id: Uuid) -> Option<DraftEdits> {
    DRAFTS.read().clone().filter(|d| d.review_id == review_id)
}

/// Drop the buffer (after publishing or dismissing).
pub(crate) fn clear() {
    if DRAFTS.peek().is_some() {
        *DRAFTS.write() = None;
        super::drafts::bump();
    }
}

/// The buffer as the record the store keeps.
pub(crate) fn snapshot() -> Option<StoredReviewEdits> {
    DRAFTS.peek().clone().map(|d| StoredReviewEdits {
        review_id: d.review_id,
        origin: d.origin,
        comments: d.comments,
        summary: d.summary,
        verdict: d.verdict,
    })
}

/// Restore the saved buffer at startup. `ensure_seeded` still reseeds it if
/// the review's drafted comments have changed since.
pub(crate) fn load(saved: Option<StoredReviewEdits>) {
    *DRAFTS.write() = saved.map(|e| DraftEdits {
        review_id: e.review_id,
        origin: e.origin,
        comments: e.comments,
        summary: e.summary,
        verdict: e.verdict,
    });
}

/// Apply one edit to the buffer and mark the drafts changed.
fn edit(f: impl FnOnce(&mut DraftEdits)) {
    if let Some(d) = DRAFTS.write().as_mut() {
        f(d);
        super::drafts::bump();
    }
}

/// Flip a comment's include/skip checkbox.
pub(crate) fn toggle(index: usize) {
    edit(|d| {
        if let Some(c) = d.comments.get_mut(index) {
            c.selected = !c.selected;
        }
    });
}

/// Replace a comment's body with the user's edit.
pub(crate) fn set_body(index: usize, body: String) {
    edit(|d| {
        if let Some(c) = d.comments.get_mut(index) {
            c.body = body;
        }
    });
}

/// Replace a comment's criticality with the maintainer's judgement. The empty
/// string clears it — that comment then publishes untagged, verbatim.
pub(crate) fn set_severity(index: usize, severity: String) {
    edit(|d| {
        if let Some(c) = d.comments.get_mut(index) {
            c.severity = severity;
        }
    });
}

pub(crate) fn set_summary(summary: String) {
    edit(|d| d.summary = summary);
}

pub(crate) fn set_verdict(verdict: ReviewEvent) {
    edit(|d| d.verdict = verdict);
}

/// The DOM id of a comment's inline thread in the diff viewer, so "jump to" can
/// scroll to it from the panel.
pub(crate) fn anchor_id(index: usize) -> String {
    format!("draftcomment-{index}")
}

/// True when publishing would post only the verdict: Approve, no checked
/// comment, and an empty summary (whitespace counts as empty).
pub(crate) fn is_bare_approval(
    comments: &[DraftComment],
    summary: &str,
    verdict: ReviewEvent,
) -> bool {
    verdict == ReviewEvent::Approve
        && summary.trim().is_empty()
        && !comments.iter().any(|c| c.selected)
}

/// The publish button's label: what will actually be posted. Shared by the
/// detail panel and the diff viewer's bar so the two can't disagree.
pub(crate) fn publish_label(
    comments: &[DraftComment],
    summary: &str,
    verdict: ReviewEvent,
) -> String {
    if is_bare_approval(comments, summary, verdict) {
        return "Approve the PR".into();
    }
    match comments.iter().filter(|c| c.selected).count() {
        0 => "Publish summary only".into(),
        1 => "Publish 1 comment".into(),
        n => format!("Publish {n} comments"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(selected: bool) -> DraftComment {
        DraftComment {
            path: "src/lib.rs".into(),
            line: Some(1),
            body: "nit".into(),
            severity: String::new(),
            selected,
        }
    }

    #[test]
    fn bare_approval_says_approve() {
        assert_eq!(
            publish_label(&[], "", ReviewEvent::Approve),
            "Approve the PR"
        );
        assert_eq!(
            publish_label(&[comment(false)], "  \n ", ReviewEvent::Approve),
            "Approve the PR"
        );
    }

    #[test]
    fn approval_with_summary_is_summary_only() {
        assert_eq!(
            publish_label(&[comment(false)], "LGTM", ReviewEvent::Approve),
            "Publish summary only"
        );
    }

    #[test]
    fn other_verdicts_without_summary_stay_summary_only() {
        assert_eq!(
            publish_label(&[], "", ReviewEvent::Comment),
            "Publish summary only"
        );
        assert_eq!(
            publish_label(&[], "", ReviewEvent::RequestChanges),
            "Publish summary only"
        );
    }

    #[test]
    fn checked_comments_are_counted() {
        assert_eq!(
            publish_label(&[comment(true), comment(false)], "", ReviewEvent::Approve),
            "Publish 1 comment"
        );
        assert_eq!(
            publish_label(&[comment(true), comment(true)], "", ReviewEvent::Approve),
            "Publish 2 comments"
        );
    }
}
