//! UI components. Each reads `AppState` from context and renders signals; every
//! action (agent runs and direct CRUD alike) goes to the executor via the
//! `AppState` command helpers — the UI never touches the store itself.

mod adoptdialog;
mod board;
mod card;
mod cardmenu;
mod confirm;
mod detail;
mod diffdialog;
pub(crate) mod drafts;
mod icons;
mod reviewboard;
mod reviewdraft;
mod search;
mod settings;
mod sidebar;
mod usagebar;
mod widgets;

pub use adoptdialog::AdoptDialogHost;
pub use board::BoardArea;
pub use cardmenu::CardMenuHost;
pub use confirm::ConfirmHost;
pub use detail::DetailArea;
pub use diffdialog::DiffDialogHost;
pub use reviewboard::ReviewBoard;
pub use search::SearchHost;
pub use settings::{ProjectSettingsModal, SettingsModal};
pub use sidebar::Sidebar;
pub use usagebar::UsageBar;

use usine_core::{DraftComment, ExecutorCommand, ReviewEvent, SEVERITY_LEVELS};
use uuid::Uuid;

use crate::state::AppState;
// Re-exported so the event reducer in `state` can raise a dialog of its own (a
// merge conflict is discovered by the executor, not by a click).
pub(crate) use confirm::{request_confirm, ConfirmAction, ConfirmRequest};

/// Run an irreversible, outward-facing command (Create PR / Merge) behind a
/// themed confirm dialog. In demo mode nothing real happens, so it fires
/// immediately without a prompt.
fn confirm_then_send(
    state: AppState,
    title: &str,
    message: String,
    confirm_label: &str,
    cmd: ExecutorCommand,
) {
    if crate::state::demo_mode() {
        state.send(cmd);
        return;
    }
    request_confirm(ConfirmRequest {
        title: title.to_string(),
        message,
        confirm_label: confirm_label.to_string(),
        danger: false,
        action: ConfirmAction::Send(cmd),
    });
}

/// The criticality of what is about to be posted, as " (1 critical, 2 medium)",
/// most severe first with unrated comments last.
///
/// Publishing is irreversible and the level rides out with each comment, so the
/// dialog names it — including how many comments carry no judgement at all.
/// Empty when nothing is selected.
fn severity_breakdown(drafts: &[DraftComment]) -> String {
    // One pass over the drafts, normalising each severity once: counts land in
    // `SEVERITY_LEVELS` order, with anything unrated in the trailing slot.
    let mut counts = [0usize; SEVERITY_LEVELS.len() + 1];
    for d in drafts.iter().filter(|d| d.selected) {
        counts[usine_core::severity_rank(&d.severity).unwrap_or(SEVERITY_LEVELS.len())] += 1;
    }
    let mut parts: Vec<String> = SEVERITY_LEVELS
        .iter()
        .zip(counts)
        .filter(|&(_, n)| n > 0)
        .map(|(level, n)| format!("{n} {level}"))
        .collect();
    if let n @ 1.. = counts[SEVERITY_LEVELS.len()] {
        parts.push(format!("{n} untagged"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(" ({})", parts.join(", "))
}

/// Publish a drafted review to GitHub behind a confirm dialog, from either
/// validation surface (the detail panel's list or the diff viewer).
///
/// Posting a review is outward-facing and effectively irreversible — the comments
/// land on someone else's PR under the user's name — so it gets the same gate as
/// Create PR and Merge. The dialog states exactly what will be sent, because the
/// verdict (approve vs. request changes) matters as much as the comment count.
///
/// On confirm the shared draft buffer is cleared and the diff viewer closed, so
/// neither surface is left editing a review that has already been submitted.
fn confirm_publish_review(
    state: AppState,
    review_id: Uuid,
    drafts: Vec<DraftComment>,
    event: ReviewEvent,
    body: String,
) {
    let selected = drafts.iter().filter(|d| d.selected).count();
    let what = match selected {
        0 => "a summary-only review".to_string(),
        1 => format!("1 inline comment{}", severity_breakdown(&drafts)),
        n => format!("{n} inline comments{}", severity_breakdown(&drafts)),
    };
    let cmd = ExecutorCommand::PublishReview {
        review_id,
        drafts,
        event,
        body,
    };
    if crate::state::demo_mode() {
        state.send(cmd);
        finish_review_validation();
        return;
    }
    request_confirm(ConfirmRequest {
        title: "Publish review".into(),
        message: format!(
            "Submit {what} to GitHub as “{}”? This posts on the contributor's pull \
             request under your account and can't be undone from here.",
            event.label()
        ),
        confirm_label: "Publish".into(),
        danger: false,
        action: ConfirmAction::PublishReview(cmd),
    });
}

/// Publish the drafted review *and* have an agent fix its comments, behind the
/// same confirm as a plain publish — it posts the same review, and then some.
///
/// The dialog is explicit about both halves: the comments go out carrying a
/// promise the user is making on their own behalf, and the fix that follows
/// touches someone else's branch. It also states the part that makes this safe
/// to accept — nothing is pushed until the diff has been approved.
fn confirm_publish_and_fix_review(
    state: AppState,
    review_id: Uuid,
    drafts: Vec<DraftComment>,
    event: ReviewEvent,
    body: String,
) {
    let n = drafts.iter().filter(|d| d.selected).count();
    let breakdown = severity_breakdown(&drafts);
    let cmd = ExecutorCommand::PublishReviewAndFix {
        review_id,
        drafts,
        event,
        body,
    };
    if crate::state::demo_mode() {
        state.send(cmd);
        finish_review_validation();
        return;
    }
    request_confirm(ConfirmRequest {
        title: "Publish & fix".into(),
        message: format!(
            "Submit {n} inline comment(s){breakdown} as “{}”, each saying you'll fix it yourself, then have              an agent fix all {n} in the PR's checkout? Nothing is pushed until you approve the              diff.",
            event.label()
        ),
        confirm_label: "Publish & fix".into(),
        // Same buffer teardown as a plain publish: the review is posted either way.
        action: ConfirmAction::PublishReview(cmd),
        danger: false,
    });
}

/// Push a committed fix onto the contributor's own PR branch. Outward-facing and
/// not undoable from here — it rewrites what their branch points at — so it
/// names the branch it will write to.
pub(crate) fn confirm_push_review_fix(state: AppState, review_id: Uuid, head_ref: String) {
    confirm_then_send(
        state,
        "Push the fix",
        format!(
            "Push the committed fix to `{head_ref}` on the contributor's pull request? It lands              under your account, and a comment on the PR will say so."
        ),
        "Push",
        ExecutorCommand::PushReviewFix { review_id },
    );
}

/// Abandon a committed fix. Destructive (the commits go with the checkout) *and*
/// outward-facing: a retraction is posted so the author isn't left waiting.
pub(crate) fn confirm_discard_review_fix(review_id: Uuid) {
    request_confirm(ConfirmRequest {
        title: "Discard the fix".into(),
        message: "Throw away the committed fix and its checkout? A comment is posted on the PR                   telling the author the fix isn't coming, so the comments become theirs to                   address."
            .into(),
        confirm_label: "Discard".into(),
        danger: true,
        action: ConfirmAction::Send(ExecutorCommand::DiscardReviewFix { review_id }),
    });
}

/// Approve a PR outright, skipping the review agent — for the PRs the user has
/// already read and judged fine on their own. Same outward-facing gate as
/// publishing a drafted review: it's the same POST, just with no comments and
/// no summary.
pub(crate) fn confirm_approve_review(state: AppState, review_id: Uuid, pr_number: u64) {
    confirm_then_send(
        state,
        "Approve pull request",
        format!(
            "Approve PR #{pr_number} without running a review agent? The approval posts \
             on the contributor's pull request under your account and can't be undone \
             from here."
        ),
        "Approve",
        ExecutorCommand::PublishReview {
            review_id,
            drafts: Vec::new(),
            event: ReviewEvent::Approve,
            body: String::new(),
        },
    );
}

/// Tear down both validation surfaces once a review has been submitted: drop the
/// edit buffer and close the diff viewer if it's the one that was open on it.
pub(crate) fn finish_review_validation() {
    reviewdraft::clear();
    diffdialog::dismiss_dialog();
}

/// Dismiss a PR from the review board, behind a confirm.
///
/// This is permanent in a way the label doesn't suggest: the PR number is
/// recorded as dismissed forever, so no later scan will ever surface it again.
/// There is no undo, which is exactly why it asks first.
pub(crate) fn confirm_dismiss_review(review_id: Uuid, pr_number: u64) {
    request_confirm(ConfirmRequest {
        title: "Dismiss pull request".into(),
        message: format!(
            "Remove PR #{pr_number} from the review board? It won't come back on a \
             later scan, and any checkout made for it is torn down."
        ),
        confirm_label: "Dismiss".into(),
        danger: true,
        action: ConfirmAction::Send(ExecutorCommand::DismissReview { review_id }),
    });
}

/// Discard a drafted review that's awaiting validation. Same command as dismiss —
/// and the same permanence — but phrased for the case where the agent's work is
/// what's being thrown away.
pub(crate) fn confirm_discard_review(review_id: Uuid) {
    request_confirm(ConfirmRequest {
        title: "Discard review".into(),
        message: "Throw away these drafted comments and drop the PR from the board? \
                  Nothing is posted to GitHub, and the PR won't reappear on a later scan."
            .into(),
        confirm_label: "Discard".into(),
        danger: true,
        action: ConfirmAction::Send(ExecutorCommand::DismissReview { review_id }),
    });
}
