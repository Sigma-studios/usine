//! The right-hand detail panel for a PR under review — the review-board
//! counterpart of [`super::CardDetail`]. Same shell (header, badges, body,
//! transcript), and the drafted comments use the same checkbox-list markup as
//! the card board's fix selection.

use dioxus::prelude::*;
use usine_core::{CheckStatus, DraftComment, ReviewEvent, ReviewStatus, ReviewTask};
use uuid::Uuid;

use super::transcript::TranscriptView;
use crate::state::AppState;
use crate::ui::diffdialog::{event_value, open_review_diff, open_review_diff_at, parse_event};
use crate::ui::icons::{IconDiff, IconExternal};
use crate::ui::reviewdraft;

/// The detail panel for the selected review task.
#[component]
pub fn ReviewDetail() -> Element {
    let state = use_context::<AppState>();
    let sel = *state.selected_review.read();
    let task = sel.and_then(|id| state.review_task(id));

    match task {
        // Collapsed: no panel at all when nothing is selected, so the board gets
        // the full width.
        None => rsx! {},
        Some(task) => {
            let id = task.id;
            let project = state.project_name(task.project_id);
            let status = task.status.status_label();
            let cost = task.cost;
            let status_key = status_discriminant(&task.status);
            let url = task.url.clone();

            rsx! {
                div { class: "detail",
                    div { class: "detail-header",
                        div { class: "detail-title-row",
                            h2 { "{task.pr_title}" }
                            button {
                                class: "card-icon-btn",
                                title: "Show the diff",
                                "aria-label": "Show the diff",
                                onclick: move |_| open_review_diff(id),
                                IconDiff {}
                            }
                            a {
                                class: "card-icon-btn",
                                href: "{url}",
                                target: "_blank",
                                rel: "noreferrer",
                                title: "Open on GitHub",
                                "aria-label": "Open on GitHub",
                                IconExternal {}
                            }
                            button {
                                class: "detail-close",
                                "aria-label": "Close panel",
                                onclick: move |_| state.select_review(None),
                                "×"
                            }
                        }
                        div { class: "card-meta",
                            span { class: "badge", "{project}" }
                            span { class: "badge", "#{task.pr_number}" }
                            span { class: "badge", "@{task.author}" }
                            span { class: "badge status", "{status}" }
                            if task.checks.is_reportable() {
                                span { class: "badge {task.checks.css_class()}",
                                    "{task.checks.glyph()} {task.checks.label()}"
                                }
                            }
                            if task.mergeable.is_conflicting() {
                                span { class: "badge conflict", "conflicts" }
                            }
                            if !cost.is_zero() {
                                span { class: "badge cost", "{cost}" }
                            }
                        }
                    }
                    div { class: "detail-body",
                        // Remount on every status change so the draft editor's signals
                        // (checkboxes, edited bodies, verdict) reset instead of leaking
                        // the previous review's input. Dioxus only honors `key` for
                        // siblings in a *list*, hence the single-item `for` — see the
                        // same trick in `super::CardDetail`.
                        for t in [task.clone()] {
                            ReviewPanel { key: "{id}:{status_key}", task: t }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ReviewPanel(task: ReviewTask) -> Element {
    let state = use_context::<AppState>();
    let id = task.id;
    // Steering for the run this panel can start, seeded from what the last one
    // used so a retry starts from the same instruction instead of a blank box.
    // A draft, so a half-typed instruction survives deselects and the panel's
    // remount-on-status-change; starting the run forgets it explicitly.
    let mut guidance =
        crate::ui::drafts::use_draft(id, "review.guidance", || task.guidance.clone());
    // Once the run is under way the box is gone, but the instruction still
    // explains why the drafts read the way they do — so it's shown back.
    let steering = task.guidance.trim().to_string();
    let show_steering = !steering.is_empty()
        && matches!(
            task.status,
            ReviewStatus::Reviewing | ReviewStatus::AwaitingValidation { .. }
        );

    rsx! {
        div { class: "section",
            // The diff is reachable from the panel header's icon (always) and,
            // where it's the obvious next step, from a labelled button in the
            // state's own section — no third entry point here.
            h3 { "Pull request" }
            div { class: "hint", "{task.head_ref} → {task.base_ref}" }
            if task.checks == CheckStatus::Failing {
                div { class: "hint error", "CI is failing on this PR." }
            }
            if task.mergeable.is_conflicting() {
                div { class: "hint error", "This PR conflicts with {task.base_ref}." }
            }
            a { class: "wt-path", href: "{task.url}", target: "_blank", rel: "noreferrer", "Open on GitHub ↗" }
        }

        // The author's own account of the change. Reviewing a diff without the
        // stated intent means guessing at what it was *meant* to do, so this sits
        // above the review controls rather than behind a trip to GitHub.
        if !task.body.trim().is_empty() {
            div { class: "section",
                h3 { "Description" }
                div { class: "pr-body", "{task.body}" }
            }
        }

        if show_steering {
            div { class: "section",
                h3 { "Steering" }
                div { class: "pr-body", "{steering}" }
            }
        }

        match &task.status {
            ReviewStatus::ToReview => rsx! {
                div { class: "section",
                    h3 { "Review" }
                    div { class: "hint",
                        "Check out the PR in a worktree and have an agent draft review comments — you approve each one before anything is posted. Already read the PR yourself? Approve it directly, no agent pass needed."
                    }
                    div { class: "field",
                        label { "Steer this review (optional)" }
                        textarea {
                            placeholder: "What should this pass focus on? e.g. \"the migration and its rollback path\"",
                            value: "{guidance}",
                            oninput: move |e| guidance.set(e.value()),
                        }
                    }
                    div { class: "row",
                        button {
                            class: "btn primary",
                            onclick: move |_| {
                                state.start_review(id, guidance.read().clone());
                                // Consumed by the run (it becomes `task.guidance`,
                                // the next mount's seed). The seed rule can't
                                // clear this one: its seed is nonempty.
                                crate::ui::drafts::forget(id, "review.guidance");
                            },
                            "Review this PR"
                        }
                        button {
                            class: "btn",
                            title: "Approve on GitHub without running the review agent",
                            onclick: move |_| crate::ui::confirm_approve_review(state, id, task.pr_number),
                            "Approve"
                        }
                        button {
                            class: "btn",
                            title: "Read the diff without running an agent",
                            onclick: move |_| open_review_diff(id),
                            "Show diff"
                        }
                        button {
                            class: "btn subtle",
                            title: "Dismiss this PR from the review board",
                            onclick: move |_| crate::ui::confirm_dismiss_review(id, task.pr_number),
                            "Dismiss"
                        }
                    }
                }
            },
            ReviewStatus::Reviewing => rsx! {
                div { class: "section", div { class: "hint", "Reading the diff and drafting comments…" } }
            },
            ReviewStatus::AwaitingValidation { drafts, summary, event } => rsx! {
                DraftSelection {
                    review_id: id,
                    drafts: drafts.clone(),
                    summary: summary.clone(),
                    event: *event,
                }
            },
            ReviewStatus::Reviewed => rsx! {
                div { class: "section",
                    h3 { "Review" }
                    div { class: "reviewed-tag", "✓ published" }
                    div { class: "hint", "This review was submitted to GitHub." }
                }
            },
            ReviewStatus::MergedWithoutReview { merged } => rsx! {
                div { class: "section",
                    h3 { "Review" }
                    div { class: "hint",
                        if *merged {
                            "This PR was merged on GitHub before the review finished — nothing left to review. Dismiss it once acknowledged; if the PR is reopened, it returns to the queue on the next refresh."
                        } else {
                            "This PR was closed on GitHub without merging — nothing left to review. Dismiss it once acknowledged; if the PR is reopened, it returns to the queue on the next refresh."
                        }
                    }
                    button {
                        class: "btn subtle",
                        title: "Dismiss this PR from the review board",
                        onclick: move |_| crate::ui::confirm_dismiss_review(id, task.pr_number),
                        "Dismiss"
                    }
                }
            },
            ReviewStatus::Failed { message, .. } => rsx! {
                div { class: "section",
                    div { class: "question", "Review failed: {message}" }
                    div { class: "field",
                        label { "Steer this review (optional)" }
                        textarea {
                            placeholder: "What should this pass focus on? e.g. \"the migration and its rollback path\"",
                            value: "{guidance}",
                            oninput: move |e| guidance.set(e.value()),
                        }
                    }
                    div { class: "row",
                        button {
                            class: "btn",
                            onclick: move |_| {
                                state.start_review(id, guidance.read().clone());
                                crate::ui::drafts::forget(id, "review.guidance");
                            },
                            "Retry"
                        }
                        button {
                            class: "btn subtle",
                            onclick: move |_| crate::ui::confirm_dismiss_review(id, task.pr_number),
                            "Dismiss"
                        }
                    }
                }
            },
        }

        TranscriptView { id }
    }
}

/// The awaiting-validation index: every drafted comment, with its severity and
/// location, as a scannable list.
///
/// The *validation* proper happens in the diff viewer, where each comment sits
/// against the code it's about — so this list leads with the button that opens it
/// and each row can jump straight to its own comment there. The controls here are
/// the same shared buffer, though, so a review can still be finished entirely
/// from the panel when the diff isn't wanted.
#[component]
fn DraftSelection(
    review_id: Uuid,
    drafts: Vec<DraftComment>,
    summary: String,
    event: ReviewEvent,
) -> Element {
    let state = use_context::<AppState>();

    // Both surfaces edit one buffer, so unchecking a comment in the diff shows up
    // here and vice versa. Seeding is idempotent and keyed by review id.
    reviewdraft::ensure_seeded(review_id, &drafts, &summary, event);
    let Some(edits) = reviewdraft::edits_for(review_id) else {
        return rsx! {};
    };

    // A summary-only review is a legitimate outcome, so the button stays live
    // with nothing checked — it just says what it will actually post.
    let checked_count = edits.comments.iter().filter(|c| c.selected).count();
    let publish_label = match checked_count {
        0 => "Publish summary only".to_string(),
        1 => "Publish 1 comment".to_string(),
        n => format!("Publish {n} comments"),
    };
    let verdict = edits.verdict;
    let comments_for_publish = edits.comments.clone();
    let summary_for_publish = edits.summary.clone();

    // Grow each comment box to fit its text: a review comment is meant to be read
    // in full, not scrolled inside a two-row window. Runs once after mount (the
    // panel remounts whenever the status changes) and keeps sizing on every
    // keystroke. Without JS the boxes fall back to their `rows` estimate.
    use_effect(move || {
        dioxus::document::eval(
            "(function(){document.querySelectorAll('textarea.autogrow').forEach(function(el){\
             var fit=function(){el.style.height='auto';el.style.height=el.scrollHeight+'px';};\
             if(!el.dataset.growInit){el.dataset.growInit='1';el.addEventListener('input',fit);}\
             fit();});})();",
        );
    });

    rsx! {
        div { class: "section",
            div { class: "row",
                h3 { "Drafted comments" }
                span { class: "badge", "{checked_count}/{edits.comments.len()}" }
            }
            div { class: "hint",
                "Review these against the code — each one opens at the line it's about."
            }
            button {
                class: "btn primary",
                onclick: move |_| open_review_diff(review_id),
                "Review in diff"
            }
            if edits.comments.is_empty() {
                div { class: "hint", "No inline comments — publish a summary-only review below." }
            }
            for (i, d) in edits.comments.iter().enumerate() {
                {
                    let checked = d.selected;
                    let loc = match d.line {
                        Some(l) => format!("{}:{}", d.path, l),
                        None => d.path.clone(),
                    };
                    // Severity badge (falls back to a neutral dash when unrated).
                    let sev = d.severity.clone();
                    let sev_label = if sev.is_empty() { "—".to_string() } else { sev.clone() };
                    let sev_class = if sev.is_empty() {
                        "sev".to_string()
                    } else {
                        format!("sev sev-{sev}")
                    };
                    let body_text = d.body.clone();
                    let rows = fallback_rows(&body_text);
                    rsx! {
                        div { key: "{i}", class: "comment",
                            input {
                                r#type: "checkbox",
                                checked,
                                onchange: move |_| reviewdraft::toggle(i),
                            }
                            div { class: "comment-main",
                                // Open by default — the comment is the thing being
                                // reviewed. Collapsing leaves its severity and location
                                // visible so a long list stays scannable.
                                details { class: "draft", open: true,
                                    summary {
                                        span { class: "{sev_class}", "{sev_label}" }
                                        span { class: "path", "{loc}" }
                                        button {
                                            class: "draft-jump",
                                            title: "Show this comment in the diff",
                                            "aria-label": "Show this comment in the diff",
                                            // `summary` toggles the `details` on click;
                                            // jumping shouldn't also collapse the row.
                                            onclick: move |e| {
                                                e.stop_propagation();
                                                e.prevent_default();
                                                open_review_diff_at(review_id, i);
                                            },
                                            "jump ↗"
                                        }
                                    }
                                    textarea {
                                        class: "review-comment-edit autogrow",
                                        rows: "{rows}",
                                        value: "{body_text}",
                                        oninput: move |e| reviewdraft::set_body(i, e.value()),
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "field",
                label { "Overall summary" }
                textarea {
                    class: "review-summary-edit autogrow",
                    rows: "{fallback_rows(&edits.summary)}",
                    value: "{edits.summary}",
                    oninput: move |e| reviewdraft::set_summary(e.value()),
                }
            }
            div { class: "field",
                label { "Verdict" }
                select {
                    value: "{event_value(verdict)}",
                    onchange: move |e| reviewdraft::set_verdict(parse_event(&e.value())),
                    for ev in ReviewEvent::all() {
                        option {
                            value: event_value(ev),
                            selected: event_value(verdict) == event_value(ev),
                            "{ev.label()}"
                        }
                    }
                }
            }
            div { class: "row",
                button {
                    class: "btn primary",
                    onclick: move |_| {
                        crate::ui::confirm_publish_review(
                            state,
                            review_id,
                            comments_for_publish.clone(),
                            verdict,
                            summary_for_publish.clone(),
                        );
                    },
                    "{publish_label}"
                }
                button {
                    class: "btn subtle",
                    title: "Discard this review",
                    onclick: move |_| crate::ui::confirm_discard_review(review_id),
                    "Discard"
                }
            }
        }
    }
}

/// A `rows` estimate used only until the autogrow script measures the box for
/// real — and as the final height if scripting is unavailable. Counts hard line
/// breaks and allows one wrapped line per ~72 characters.
pub(super) fn fallback_rows(body: &str) -> usize {
    let rows: usize = body.lines().map(|l| 1 + l.len() / 72).sum();
    rows.clamp(2, 24)
}

/// Keys the panel remount (see `ReviewDetail`).
fn status_discriminant(s: &ReviewStatus) -> &'static str {
    match s {
        ReviewStatus::ToReview => "to-review",
        ReviewStatus::Reviewing => "reviewing",
        ReviewStatus::AwaitingValidation { .. } => "validate",
        ReviewStatus::Reviewed => "reviewed",
        ReviewStatus::MergedWithoutReview { merged: true } => "ext-merged",
        ReviewStatus::MergedWithoutReview { merged: false } => "ext-closed",
        ReviewStatus::Failed { .. } => "failed",
    }
}
