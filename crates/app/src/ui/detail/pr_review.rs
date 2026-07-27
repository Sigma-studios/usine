//! The PR-review panel: draft→ready, submitted reviews, and comment triage
//! for the card's own pull request.

use dioxus::prelude::*;
use usine_core::{Card, CardState, ExecutorCommand, PrReviewSub};

use crate::state::AppState;

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
    // but every comment is worth reading). The poll — and the ↻ button — keep this
    // total fresh while the card sits in `Idle`.
    let can_triage =
        matches!(card.state, CardState::PrReview(PrReviewSub::Idle)) && card.comment_count > 0;
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
                        rsx! {
                            div { key: "{author}", class: "comment",
                                div {
                                    div { class: "path", "{author}" }
                                    div { "{rstate}" }
                                }
                            }
                        }
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
