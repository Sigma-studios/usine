//! The right-hand detail panel. `DetailArea` picks the panel for the active
//! board: `CardDetail` for the card board, `ReviewDetail` for the PR-review
//! board. `CardPanel` then dispatches to the per-phase sub-panels, which live in
//! the sibling files of this module.

use dioxus::prelude::*;
use usine_core::{Card, CardState, DesignSub, ExecutorCommand, PrReviewSub, ReviewSub};
use uuid::Uuid;

use super::diffdialog::open_diff_dialog;
use super::icons::IconDiff;
use crate::state::{AppState, BoardMode};
use crate::ui::widgets::provider_value;

mod conclusion;
mod edit;
mod fixes;
mod plan;
mod pr_create;
mod pr_review;
mod review;
mod transcript;

use conclusion::ConclusionPanel;
use edit::{Attachments, ConfigForm, EditableTask};
use fixes::FixSelection;
use plan::PlanApproval;
use pr_create::PrCreateForm;
use pr_review::PrReviewPanel;
use review::ReviewDetail;
use transcript::TranscriptView;

/// The detail panel for whichever board is up. Mirrors `BoardArea`: the two
/// boards keep independent selections, so switching between them doesn't leak
/// one's open panel into the other.
#[component]
pub fn DetailArea() -> Element {
    let state = use_context::<AppState>();
    match state.board_mode() {
        BoardMode::Review => rsx! { ReviewDetail {} },
        BoardMode::Normal => rsx! { CardDetail {} },
    }
}

/// The right-hand detail panel for the selected card.
#[component]
fn CardDetail() -> Element {
    let state = use_context::<AppState>();
    let sel = *state.selected_card.read();
    let card = sel.and_then(|id| state.cards.read().iter().find(|c| c.id == id).cloned());

    match card {
        // Collapsed: no panel at all when nothing is selected, so the board gets
        // the full width.
        None => rsx! {},
        Some(card) => {
            let id = card.id;
            let project = state.project_name(card.project_id);
            let provider = card.config.provider.label();
            let provider_class = provider_value(card.config.provider);
            let status = card.state.status_label();
            let cost = card.cost;
            let state_key = state_discriminant(&card.state);
            // Same gap the board card covers: a lifecycle command is working but
            // hasn't transitioned the card yet, so the panel still shows the
            // buttons that led here (notably "Approve & implement").
            let busy = state.busy.read().contains(&id);
            let body_class = if busy {
                "detail-body is-busy"
            } else {
                "detail-body"
            };
            let title = if card.title.trim().is_empty() {
                "Untitled card".to_string()
            } else {
                card.title.clone()
            };
            // Committed work to diff: the same window the card menu's entry uses.
            let can_diff = matches!(
                card.state,
                CardState::AwaitingReview(_) | CardState::PrReview(_) | CardState::ReadyToMerge
            );

            rsx! {
                div { class: "detail",
                    div { class: "detail-header",
                        div { class: "detail-title-row",
                            h2 { "{title}" }
                            if can_diff {
                                button {
                                    class: "card-icon-btn",
                                    title: "Show diff",
                                    "aria-label": "Show diff",
                                    onclick: move |_| open_diff_dialog(id),
                                    IconDiff {}
                                }
                            }
                            button {
                                class: "detail-close",
                                "aria-label": "Close panel",
                                onclick: move |_| state.select_card(None),
                                "×"
                            }
                        }
                        div { class: "card-meta",
                            if busy {
                                span { class: "spinner" }
                            }
                            span { class: "badge", "{project}" }
                            span { class: "badge provider {provider_class}", "{provider}" }
                            span { class: "badge status", "{status}" }
                            if !cost.is_zero() {
                                span { class: "badge cost", "{cost}" }
                            }
                        }
                    }
                    div { class: "{body_class}",
                        // Remount the panel whenever the selected card or its state
                        // changes, so every per-card/per-state form signal (the
                        // description mirror, skip-plan, answers, …) resets instead of
                        // leaking the previously-viewed card's input.
                        //
                        // Dioxus only honors `key` for siblings in a *list*: a lone
                        // child's key is ignored and its scope — with all its
                        // `use_signal` state — is reused across renders. Wrapping in a
                        // single-item `for` puts CardPanel in a keyed-list context, so a
                        // changed key genuinely tears it down and rebuilds it.
                        for c in [card.clone()] {
                            CardPanel { key: "{id}:{state_key}", card: c }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn CardPanel(card: Card) -> Element {
    let state = use_context::<AppState>();
    let id = card.id;

    let is_start = matches!(card.state, CardState::StartingBlock);
    let is_ready = matches!(card.state, CardState::ReadyToMerge);
    // A card can reach `ReadyToMerge` while its PR is still a draft (a draft PR
    // whose reviewer comments were triaged). GitHub won't merge a draft, so gate
    // the merge behind flipping it ready for review first.
    let pr_is_draft = card
        .pr
        .as_ref()
        .map(|p| p.state == "draft")
        .unwrap_or(false);
    // Delete the head branch when merging — on by default.
    let mut delete_branch = use_signal(|| true);
    let recap = state.review_recaps.read().get(&id).cloned();
    let mut post_pr_feedback = use_signal(String::new);
    let fail_msg = if let CardState::Failed { message, .. } = &card.state {
        Some(message.clone())
    } else {
        None
    };
    let interrupted = fail_msg
        .as_deref()
        .map(|m| m.starts_with("Interrupted"))
        .unwrap_or(false);
    let recover_label = if interrupted { "Resume" } else { "Retry" };
    let fail_display = fail_msg.as_ref().map(|m| {
        if interrupted {
            m.clone()
        } else {
            format!("Run failed: {m}")
        }
    });

    rsx! {
        if is_start {
            EditableTask { card: card.clone() }
            Attachments { card_id: id, provider: card.config.provider }
            ConfigForm { card: card.clone() }
        } else {
            div { class: "section",
                h3 { "Task" }
                if card.description.trim().is_empty() {
                    div { class: "hint", "No description." }
                } else {
                    div { class: "plan-box", "{card.description}" }
                }
            }
        }

        if let Some(iv) = card.state.intervention() {
            InterventionPanel {
                card_id: id,
                question: iv.question.clone(),
                options: iv.options.clone(),
            }
        }

        if let CardState::Designing(DesignSub::AwaitingApproval { plan }) = &card.state {
            PlanApproval { card_id: id, plan: plan.clone() }
        }

        if let CardState::Concluded { conclusion } = &card.state {
            ConclusionPanel { card_id: id, conclusion: conclusion.clone() }
        }

        if matches!(card.state, CardState::AwaitingReview(_)) {
            PrCreateForm { card: card.clone() }
        }

        if matches!(card.state, CardState::PrReview(_)) {
            PrReviewPanel { card: card.clone() }
        }

        if let CardState::PrReview(PrReviewSub::SelectingFixes { verdicts }) = &card.state {
            FixSelection { card_id: id, verdicts: verdicts.clone(), self_review: false }
        }

        if let CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) = &card.state {
            FixSelection { card_id: id, verdicts: verdicts.clone(), self_review: true }
        }

        if is_ready && card.unanswered_count > 0 {
            div { class: "section",
                h3 { "New review comments" }
                div { class: "hint",
                    if card.unanswered_count == 1 {
                        "A review comment on the PR has no answer yet — it arrived after (or survived) the last pass. Have the agent read and triage it before merging."
                    } else {
                        {format!("{} review threads on the PR have no answer yet — they arrived after (or survived) the last pass. Have the agent read and triage them before merging.", card.unanswered_count)}
                    }
                }
                button {
                    class: "btn primary",
                    onclick: move |_| state.send(ExecutorCommand::FetchComments { card_id: id }),
                    "Reevaluate comments"
                }
            }
        }

        if is_ready {
            div { class: "section",
                h3 { "Merge" }
                if let Some(p) = card.pr.clone() {
                    PrLink { number: p.number, url: p.url }
                }
                if let Some(recap) = recap.clone() {
                    div { class: "hint", "Fixes recap" }
                    div { class: "plan-box", "{recap}" }
                }
                if pr_is_draft {
                    div { class: "hint",
                        "This PR is still a draft — GitHub won't merge it. Mark it ready for review first, then merge."
                    }
                    button {
                        class: "btn primary",
                        onclick: move |_| state.send(ExecutorCommand::MarkPrReady { card_id: id }),
                        "Mark ready for review"
                    }
                } else {
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
                        class: "btn success",
                        onclick: move |_| super::confirm_then_send(
                            state,
                            "Merge pull request",
                            "Merge this pull request into the base branch on GitHub? This can't be undone.".to_string(),
                            "Merge",
                            ExecutorCommand::Merge { card_id: id, delete_branch: delete_branch() },
                        ),
                        "Merge PR"
                    }
                }
            }
            div { class: "section",
                h3 { "Request another change" }
                div { class: "hint",
                    "Not happy with a fix, or have a reviewer follow-up? Send it back to the agent."
                }
                div { class: "field",
                    textarea {
                        placeholder: "What should the agent change?",
                        value: "{post_pr_feedback}",
                        oninput: move |e| post_pr_feedback.set(e.value()),
                    }
                }
                button {
                    class: "btn",
                    onclick: move |_| {
                        let fb = post_pr_feedback.read().trim().to_string();
                        if !fb.is_empty() {
                            state.send(ExecutorCommand::RequestPostPrChange { card_id: id, feedback: fb });
                            post_pr_feedback.set(String::new());
                        }
                    },
                    "Send change"
                }
            }
        }

        if let Some(msg) = fail_display.clone() {
            div { class: "section",
                div { class: "question", "{msg}" }
                button {
                    class: "btn",
                    onclick: move |_| state.send(ExecutorCommand::Retry { card_id: id }),
                    "{recover_label}"
                }
            }
        }

        TranscriptView { id }
    }
}

// ---------------------------------------------------------------------------
// Intervention (design / implement questions)
// ---------------------------------------------------------------------------

#[component]
fn InterventionPanel(card_id: Uuid, question: String, options: Vec<String>) -> Element {
    let state = use_context::<AppState>();
    let mut answer = use_signal(String::new);

    rsx! {
        div { class: "section",
            h3 { "Needs your input" }
            div { class: "question",
                div { "{question}" }
                div { class: "option-row",
                    for opt in options.iter() {
                        button {
                            key: "{opt}",
                            class: "btn",
                            onclick: {
                                let opt = opt.clone();
                                move |_| state.send(ExecutorCommand::Answer { card_id, text: opt.clone() })
                            },
                            "{opt}"
                        }
                    }
                }
                div { class: "row",
                    input {
                        placeholder: "Or type an answer…",
                        value: "{answer}",
                        oninput: move |e| answer.set(e.value()),
                    }
                    button {
                        class: "btn primary",
                        onclick: move |_| {
                            let text = answer.read().trim().to_string();
                            if !text.is_empty() {
                                state.send(ExecutorCommand::Answer { card_id, text });
                                answer.set(String::new());
                            }
                        },
                        "Send"
                    }
                }
            }
        }
    }
}

/// A card's pull request, rendered identically wherever it shows up (the merge
/// gate and the PR-review phase): the number on its own line and a link that
/// opens the PR on GitHub.
#[component]
pub(super) fn PrLink(number: u64, url: String) -> Element {
    rsx! {
        div { "PR #{number}" }
        a { class: "wt-path", href: "{url}", target: "_blank", rel: "noreferrer", "Open on GitHub ↗" }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn state_discriminant(s: &CardState) -> &'static str {
    match s {
        CardState::StartingBlock => "start",
        CardState::Designing(DesignSub::Running) => "design-run",
        CardState::Designing(DesignSub::Intervention(_)) => "design-iv",
        CardState::Designing(DesignSub::AwaitingApproval { .. }) => "design-approve",
        CardState::Investigating(usine_core::RunSub::Running) => "invest-run",
        CardState::Investigating(usine_core::RunSub::Intervention(_)) => "invest-iv",
        CardState::Concluded { .. } => "concluded",
        CardState::Implementing(usine_core::RunSub::Running) => "impl-run",
        CardState::Implementing(usine_core::RunSub::Intervention(_)) => "impl-iv",
        CardState::AwaitingReview(_) => "await-review",
        CardState::PrReview(PrReviewSub::Idle) => "pr-idle",
        CardState::PrReview(PrReviewSub::FetchingComments) => "pr-fetch",
        CardState::PrReview(PrReviewSub::SelectingFixes { .. }) => "pr-select",
        CardState::PrReview(PrReviewSub::ApplyingFixes) => "pr-apply",
        CardState::PrReview(PrReviewSub::ApplyingChange) => "pr-change",
        CardState::ReadyToMerge => "ready",
        CardState::Done => "done",
        CardState::Failed { .. } => "failed",
    }
}
