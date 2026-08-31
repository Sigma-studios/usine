//! The PR-review panel: draft→ready, submitted reviews, and comment triage
//! for the card's own pull request.

use dioxus::prelude::*;
use usine_core::{Card, CardState, ExecutorCommand, PrReviewSub};

use crate::state::AppState;
use crate::ui::{request_confirm, ConfirmAction, ConfirmRequest};

#[component]
pub(super) fn PrReviewPanel(card: Card) -> Element {
    let state = use_context::<AppState>();
    let id = card.id;
    let is_draft = card
        .pr
        .as_ref()
        .map(|p| p.state == "draft")
        .unwrap_or(false);
    // Triage needs something to triage: any review comment, from the assigned
    // reviewer or another one (the dock badge stays the assigned reviewer's job,
    // but every comment is worth reading) — or an unread review *body*, the
    // summary text a body-only review carries all its feedback in. The poll —
    // and the ↻ button — keep both fresh while the card sits in `Idle`.
    let can_triage = matches!(card.state, CardState::PrReview(PrReviewSub::Idle))
        && card.has_triageable_feedback();
    // Bodies not yet handled (triaged or marked read) — what the "Mark as
    // read" button below clears.
    let has_pending_bodies = !card.pending_review_bodies().is_empty();
    let is_fetching = matches!(
        card.state,
        CardState::PrReview(PrReviewSub::FetchingComments)
    );
    let is_applying = matches!(card.state, CardState::PrReview(PrReviewSub::ApplyingFixes));
    let is_applying_change = matches!(card.state, CardState::PrReview(PrReviewSub::ApplyingChange));
    // A card sitting at the PR gate can be reprompted with a free-form change,
    // just like a Ready-for-PR card — the agent updates the branch and pushes,
    // updating the open PR in place.
    let is_idle = matches!(card.state, CardState::PrReview(PrReviewSub::Idle));
    // The last-resort merge below deletes the head branch by default, like the
    // merge gate's own checkbox.
    let mut delete_branch = use_signal(|| true);
    // Offer the last resort only where it can actually work: a draft PR is
    // unmergeable on GitHub (the panel already offers "Mark ready for review"),
    // and a conflicting one the executor would refuse — the merge gate keeps
    // the same promise of never offering a merge that can't happen.
    let can_merge_without_review = is_idle && !is_draft;
    let is_conflicting = card.mergeable.is_conflicting();

    // The background poll keeps `card.reviews` fresh alongside the comment count,
    // so the panel just renders what the card knows — no fetch on open.
    let reviews = card.reviews.clone();

    rsx! {
        div { class: "section",
            h3 { "Pull request" }
            if let Some(p) = card.pr.clone() {
                super::PrLink { number: p.number, url: p.url }
            }
            if is_draft {
                div { class: "hint",
                    "This PR is a draft — add any screenshots and finish its description on GitHub, then mark it ready for review."
                }
                button {
                    class: "btn primary",
                    onclick: move |_| state.send(ExecutorCommand::MarkPrReady { card_id: id }),
                    "Mark ready for review"
                }
            }
        }

        div { class: "section",
            div { class: "row",
                h3 { "Reviews" }
                button {
                    class: "btn icon",
                    title: "Refresh review status",
                    "aria-label": "Refresh review status",
                    onclick: move |_| state.fetch_reviews(id),
                    "↻"
                }
            }
            if reviews.is_empty() {
                div { class: "hint", "No submitted reviews yet." }
            } else {
                for r in reviews.iter() {
                    {
                        let author = r.author.clone();
                        let rstate = r.state.clone();
                        // The review's summary text — for a body-only review
                        // (a bot report, or a human Comment review with no
                        // inline comments) this is the entire feedback, so
                        // show it in full. Bot bodies can be long: the
                        // plan-box style pre-wraps and scrolls.
                        let body = r.body.trim().to_string();
                        rsx! {
                            div { key: "{author}", class: "comment",
                                div { class: "comment-main",
                                    div { class: "comment-head",
                                        div { class: "path", "{author}" }
                                        div { "{rstate}" }
                                    }
                                    if !body.is_empty() {
                                        div { class: "plan-box", "{body}" }
                                    }
                                }
                            }
                        }
                    }
                }
                if has_pending_bodies {
                    div { class: "hint",
                        "A review's summary text awaits you — triage it below, or mark it read if it needs nothing."
                    }
                    button {
                        class: "btn",
                        onclick: move |_| state.send(ExecutorCommand::MarkReviewBodiesRead { card_id: id }),
                        "Mark as read"
                    }
                }
            }
        }

        if can_triage {
            div { class: "section",
                h3 { "Triage" }
                div { class: "hint",
                    "Have an agent read the review comments and recommend which are worth fixing."
                }
                button {
                    class: "btn primary",
                    onclick: move |_| state.send(ExecutorCommand::FetchComments { card_id: id }),
                    "Evaluate the review"
                }
            }
        }
        if is_idle {
            super::AgentChatSection {
                card_id: id,
                hint: "Want to tweak the branch without waiting on a review, or ask about the \
                       work? A change updates this PR in place; a question leaves it untouched.",
                on_request: move |fb: String| {
                    state.send(ExecutorCommand::RequestPostPrChange { card_id: id, feedback: fb });
                },
            }
        }
        // The bottom-of-the-list escape hatch: the reviewer never came, or left
        // a comment nobody wants triaged. Skips the *review* only — the
        // executor still re-reads CI and refuses a red or pending build, which
        // is why there is no "Merge anyway" twin here; skipping both gates in
        // one click stays at the merge gate, after an approval.
        if can_merge_without_review {
            div { class: "section",
                h3 { "Merge without review" }
                // A conflicting PR replaces the merge with its way out, exactly
                // like the merge gate: the agent merges the base in and resolves
                // them. `ResolveConflicts` is legal from `Idle` (it runs as a
                // post-PR change and lands the card back here), so this is the
                // same offer the executor's own conflict dialog makes.
                if is_conflicting {
                    div { class: "hint",
                        "The PR conflicts with the base branch — GitHub can't merge it as it stands. Have the agent resolve them, then merge again."
                    }
                    div { class: "option-row",
                        button {
                            class: "btn",
                            onclick: move |_| state.send(ExecutorCommand::ResolveConflicts { card_id: id }),
                            "Resolve conflicts with AI"
                        }
                        button {
                            class: "btn",
                            onclick: move |_| state.fetch_reviews(id),
                            "Refresh checks"
                        }
                    }
                } else {
                    div { class: "hint",
                        "Nobody is coming to review this? Merge the PR as it stands. The review comments stay unread and unanswered — last resort."
                    }
                    label { class: "checkbox-row",
                        input {
                            r#type: "checkbox",
                            checked: delete_branch(),
                            onchange: move |_| {
                                let v = !delete_branch();
                                delete_branch.set(v);
                            },
                        }
                        "Delete the branch after merging"
                    }
                    button {
                        class: "btn subtle",
                        onclick: move |_| request_confirm(ConfirmRequest {
                            title: "Merge without review?".into(),
                            message: "No review has cleared this PR. Merge it into the base branch anyway? \
                                      Any review comments stay unread and unanswered. This can't be undone.".into(),
                            confirm_label: "Merge without review".into(),
                            danger: true,
                            action: ConfirmAction::Send(ExecutorCommand::Merge {
                                card_id: id,
                                delete_branch: delete_branch(),
                                force: false,
                            }),
                        }),
                        "Merge without review"
                    }
                }
            }
        }
        // Cancelling any of the three running PR-review phases returns the
        // card to the idle PR gate; a cancelled write run's half-applied edits
        // are discarded by the executor so they can't ride the next fix commit.
        if is_fetching {
            div { class: "section",
                div { class: "hint", "Triaging review comments…" }
                button {
                    class: "btn",
                    onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                    "Cancel"
                }
            }
        }
        if is_applying {
            div { class: "section",
                div { class: "hint", "Applying fixes & replying to comments…" }
                button {
                    class: "btn",
                    onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                    "Cancel"
                }
            }
        }
        if is_applying_change {
            div { class: "section",
                div { class: "hint", "Applying your requested change…" }
                button {
                    class: "btn",
                    onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                    "Cancel"
                }
            }
        }
    }
}
