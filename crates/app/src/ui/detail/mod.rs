//! The right-hand detail panel. `DetailArea` picks the panel for the active
//! board: `CardDetail` for the card board, `ReviewDetail` for the PR-review
//! board. `CardPanel` then dispatches to the per-phase sub-panels, which live in
//! the sibling files of this module.

use dioxus::prelude::*;
use usine_core::{
    Card, CardState, CheckStatus, DesignSub, ExecutorCommand, PrReviewSub, ReviewSub,
};
use uuid::Uuid;

use super::confirm::{request_confirm, ConfirmAction, ConfirmRequest};
use super::diffdialog::open_diff_dialog;
use super::icons::IconDiff;
use crate::state::{AppState, BoardMode};
use crate::ui::widgets::provider_value;

mod chat;
mod conclusion;
mod edit;
mod fixes;
mod plan;
mod pr_create;
mod pr_review;
mod review;
mod transcript;

use chat::AgentChatSection;
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
            // Committed work to diff: the same window the card menu's entry
            // uses — through a fault or an in-flight question, since the
            // worktree is still there.
            let can_diff = matches!(
                card.state.effective(),
                CardState::AwaitingReview(_) | CardState::PrReview(_) | CardState::ReadyToMerge
            );

            rsx! {
                div { class: "detail",
                    div { class: "detail-header",
                        div { class: "detail-title-row",
                            // Keyed per card (single-item `for`, same trick as
                            // CardPanel below) so selecting another card remounts
                            // the input and resets its buffer signal. Keyed by id
                            // only — a state transition or poll echo must not wipe
                            // an in-progress rename.
                            for t in [card.title.clone()] {
                                EditableTitle { key: "{id}", card_id: id, title: t }
                            }
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
                    // Remount the panel whenever the selected card or its state
                    // changes, so every per-card/per-state form signal (the
                    // description mirror, skip-plan, answers, …) resets instead of
                    // leaking the previously-viewed card's input.
                    //
                    // Dioxus only honors `key` for siblings in a *list*: a lone
                    // child's key is ignored and its scope — with all its
                    // `use_signal` state — is reused across renders. Wrapping in a
                    // single-item `for` puts the subtree in a keyed-list context, so
                    // a changed key genuinely tears it down and rebuilds it.
                    //
                    // The key sits on the `.detail-body` scroll container itself:
                    // the WebView keeps `scrollTop` on a reused DOM node, so keying
                    // only the child would carry card A's scroll offset into card
                    // B's panel (opening it "scrolled to the bottom"). Recreating
                    // the container resets scroll to the top. Busy flips only swap
                    // `body_class` in place and keep the scroll position.
                    for c in [card.clone()] {
                        div { key: "{id}:{state_key}", class: "{body_class}",
                            CardPanel { card: c }
                        }
                    }
                }
            }
        }
    }
}

/// The panel-header title, editable in every card state. Follows
/// `EditableTask`'s commit pattern: a local mirror saved on blur/Enter
/// (`onchange`), plus an unmount commit so deselecting mid-edit — which tears
/// the panel down before blur can fire — doesn't drop the typed text.
/// Comparing against the last value this input committed (seeded with the
/// mount-time title) keeps an untouched input from overwriting an edit that
/// arrived from elsewhere, while still saving a rename back to a previously
/// committed value; a blur-then-unmount double save is idempotent.
#[component]
fn EditableTitle(card_id: Uuid, title: String) -> Element {
    let state = use_context::<AppState>();
    let mut buf = use_signal(|| title.clone());
    let mut committed = use_signal(|| title.clone());
    // Uncontrolled field (see `chat.rs`): only a remount can push a value back
    // into it, so Escape's revert bumps this generation and the input is keyed
    // with it. The remount costs focus, which `onmounted` puts back.
    let mut generation = use_signal(|| 0u32);
    use_drop(move || {
        let t = buf.peek().clone();
        if t != *committed.peek() {
            state.update_card(card_id, |c| c.title = t);
        }
    });
    rsx! {
        for g in [generation()] {
            input {
                key: "{g}",
                class: "detail-title-input",
                initial_value: "{buf.peek()}",
                placeholder: "Untitled card",
                "aria-label": "Card title",
                oninput: move |e| buf.set(e.value()),
                onchange: move |e| {
                    let t = e.value();
                    committed.set(t.clone());
                    state.update_card(card_id, |c| c.title = t);
                },
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        buf.set(committed.peek().clone());
                        generation += 1;
                    }
                },
                onmounted: move |e: MountedEvent| {
                    // Only after a revert — the first mount must not steal focus.
                    if g > 0 {
                        spawn(async move {
                            let _ = e.data().set_focus(true).await;
                        });
                    }
                },
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
    // The PR's CI state as of the last poll/refresh; the executor re-reads it
    // authoritatively when a merge is actually requested.
    let checks = card.checks;
    // Who approved the PR (same poll keeps it fresh) — restated at the merge
    // gate so the approval that made the card mergeable stays visible.
    let approved_by = card.approved_by().join(", ");
    // How many review bodies (a body-only review's summary text) still await
    // reading — they gate the merge-gate "reevaluate" offer alongside the
    // unanswered threads, so feedback landing at `ReadyToMerge` gets triaged
    // before merging.
    let pending_bodies = card.pending_review_bodies().len();
    let recap = state.review_recaps.read().get(&id).cloned();
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
            Attachments { card_id: id }
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

        // The main running phases used to render nothing actionable here; give
        // them a status line and a way out. Cancel drops the run's progress and
        // returns the card to the starting block, hence the confirm.
        if let Some(phase) = match &card.state {
            CardState::Designing(DesignSub::Running) => Some("designing"),
            CardState::Investigating(usine_core::RunSub::Running) => Some("investigating"),
            CardState::Implementing(usine_core::RunSub::Running) => Some("implementing"),
            _ => None,
        } {
            div { class: "section",
                div { class: "hint", "The agent is {phase}…" }
                button {
                    class: "btn subtle",
                    onclick: move |_| request_confirm(ConfirmRequest {
                        title: "Stop the run?".into(),
                        message: "Stop the agent's current run? Its progress is discarded and the card returns to the starting block.".into(),
                        confirm_label: "Stop".into(),
                        danger: true,
                        action: ConfirmAction::Send(ExecutorCommand::Cancel { card_id: id }),
                    }),
                    "Stop"
                }
            }
        }

        if let CardState::Answering { question, .. } = &card.state {
            div { class: "section",
                h3 { "Answering" }
                div { class: "hint", "The agent is answering: {question}" }
                button {
                    class: "btn subtle",
                    onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                    "Cancel"
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

        if is_ready && (card.unanswered_count > 0 || pending_bodies > 0) {
            div { class: "section",
                h3 { "New review comments" }
                div { class: "hint",
                    if card.unanswered_count == 0 {
                        // Only review bodies await — a body-only review (e.g. a
                        // bot report) landed after the card reached the gate.
                        "A review's summary text on the PR hasn't been read yet. Have the agent read and triage it before merging — or mark it read if it needs nothing."
                    } else if card.unanswered_count == 1 {
                        "A review comment on the PR has no answer yet — it arrived after (or survived) the last pass. Have the agent read and triage it before merging."
                    } else {
                        {format!("{} review threads on the PR have no answer yet — they arrived after (or survived) the last pass. Have the agent read and triage them before merging.", card.unanswered_count)}
                    }
                }
                div { class: "row",
                    button {
                        class: "btn primary",
                        onclick: move |_| state.send(ExecutorCommand::FetchComments { card_id: id }),
                        "Reevaluate comments"
                    }
                    // A pending review body can also be dismissed by hand —
                    // same affordance as the PR-review panel, so a body landing
                    // at the merge gate doesn't force a full triage run.
                    if pending_bodies > 0 {
                        button {
                            class: "btn",
                            onclick: move |_| state.send(ExecutorCommand::MarkReviewBodiesRead { card_id: id }),
                            "Mark as read"
                        }
                    }
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
                    if !approved_by.is_empty() || checks.is_reportable() || pending_bodies > 0 {
                        div { class: "card-meta",
                            if !approved_by.is_empty() {
                                span {
                                    class: "badge approved",
                                    title: "An approving review has been submitted on the PR",
                                    "✓ Approved by {approved_by}"
                                }
                            }
                            if pending_bodies > 0 {
                                span {
                                    class: "badge commented",
                                    title: "A review's summary text hasn't been read yet — see \"New review comments\" above",
                                    "💬 commented"
                                }
                            }
                            if checks.is_reportable() {
                                span {
                                    class: "badge {checks.css_class()}",
                                    title: "{checks.label()}",
                                    "{checks.glyph()} CI"
                                }
                            }
                        }
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
                    // A conflicting PR replaces the merge button with its only way
                    // out: resolve with an agent. Deliberately no "Merge anyway"
                    // here — GitHub cannot merge a conflicting PR server-side, so
                    // the override would be a lie.
                    if card.mergeable.is_conflicting() {
                        div { class: "hint",
                            "The PR conflicts with the base branch — GitHub can't merge it until the conflicts are resolved. Have the agent resolve them, then merge again."
                        }
                        div { class: "option-row",
                            button {
                                class: "btn primary",
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
                    // A red or still-running build replaces the merge button with
                    // its way out: fix with an agent (red only), wait/refresh, or
                    // the explicit "Merge anyway" override. The executor re-checks
                    // before merging anyway, so this gate is a convenience — the
                    // real one can't be dodged by a stale panel.
                    match checks {
                        CheckStatus::Failing => rsx! {
                            div { class: "hint",
                                "The PR's CI checks are failing — GitHub's checks must pass before this merges cleanly. Have the agent fix them, or merge anyway if the failure is noise."
                            }
                            div { class: "option-row",
                                button {
                                    class: "btn primary",
                                    onclick: move |_| state.send(ExecutorCommand::FixChecks { card_id: id }),
                                    "Fix checks with AI"
                                }
                                button {
                                    class: "btn",
                                    onclick: move |_| state.fetch_reviews(id),
                                    "Refresh checks"
                                }
                                button {
                                    class: "btn subtle",
                                    onclick: move |_| request_confirm(ConfirmRequest {
                                        title: "Merge with failing checks?".into(),
                                        message: "This PR's CI checks are failing. Merge it into the base branch anyway? This can't be undone.".into(),
                                        confirm_label: "Merge anyway".into(),
                                        danger: true,
                                        action: ConfirmAction::Send(ExecutorCommand::Merge {
                                            card_id: id,
                                            delete_branch: delete_branch(),
                                            force: true,
                                        }),
                                    }),
                                    "Merge anyway"
                                }
                            }
                        },
                        CheckStatus::Pending => rsx! {
                            div { class: "hint",
                                "The PR's CI checks are still running. Merge once they're green — or merge anyway without waiting for them."
                            }
                            div { class: "option-row",
                                button {
                                    class: "btn",
                                    onclick: move |_| state.fetch_reviews(id),
                                    "Refresh checks"
                                }
                                button {
                                    class: "btn subtle",
                                    onclick: move |_| request_confirm(ConfirmRequest {
                                        title: "Merge before checks finish?".into(),
                                        message: "This PR's CI checks are still running. Merge it into the base branch without waiting for them? This can't be undone.".into(),
                                        confirm_label: "Merge anyway".into(),
                                        danger: false,
                                        action: ConfirmAction::Send(ExecutorCommand::Merge {
                                            card_id: id,
                                            delete_branch: delete_branch(),
                                            force: true,
                                        }),
                                    }),
                                    "Merge anyway"
                                }
                            }
                        },
                        CheckStatus::Passing | CheckStatus::None => rsx! {
                            button {
                                class: "btn success",
                                onclick: move |_| super::confirm_then_send(
                                    state,
                                    "Merge pull request",
                                    "Merge this pull request into the base branch on GitHub? This can't be undone.".to_string(),
                                    "Merge",
                                    ExecutorCommand::Merge { card_id: id, delete_branch: delete_branch(), force: false },
                                ),
                                "Merge PR"
                            }
                        },
                    }
                    }
                }
            }
            AgentChatSection {
                card_id: id,
                hint: "Not happy with a fix, have a reviewer follow-up, or a question about the \
                       work? Request a change, or ask without sending it back.",
                on_request: move |fb: String| {
                    state.send(ExecutorCommand::RequestPostPrChange { card_id: id, feedback: fb });
                },
            }
        }

        if let CardState::MergedWithoutReview { merged } = &card.state {
            div { class: "section",
                h3 { if *merged { "Merged without review" } else { "PR closed" } }
                if let Some(p) = card.pr.clone() {
                    PrLink { number: p.number, url: p.url }
                }
                div { class: "hint",
                    if *merged {
                        "This PR was merged on GitHub before its review finished here. The work is on the base branch; the worktree was cleaned up, the branch was left alone. Use the card menu to mark it done or send it back to start."
                    } else {
                        "This PR was closed on GitHub without merging. The branch was left alone in case the work is still wanted. Use the card menu to mark the card done or send it back to start."
                    }
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
    // Both fields are drafts keyed by the question they answer: the half-typed
    // answer survives deselects, but a *different* intervention arriving later
    // reseeds instead of restoring an answer meant for the previous question.
    let mut answer =
        crate::ui::drafts::use_draft_of(card_id, "intervention.answer", &question, String::new);
    // Draft-then-submit, like the plan questions: clicking an option only
    // selects it (click again to unselect); one "Send answer" button submits
    // the selection and/or the typed text.
    let mut selected = crate::ui::drafts::use_draft_of(
        card_id,
        "intervention.option",
        &question,
        || None::<String>,
    );
    // Uncontrolled field (see `chat.rs`); the send clears it via a remount.
    let mut generation = use_signal(|| 0u32);
    let can_send = selected.read().is_some() || !answer.read().trim().is_empty();

    rsx! {
        div { class: "section",
            h3 { "Needs your input" }
            div { class: "question",
                div { "{question}" }
                div { class: "option-row",
                    for opt in options.iter() {
                        {
                            let opt = opt.clone();
                            let is_sel = selected.read().as_deref() == Some(opt.as_str());
                            let cls = if is_sel { "btn primary" } else { "btn" };
                            rsx! {
                                button {
                                    key: "{opt}",
                                    class: "{cls}",
                                    onclick: move |_| {
                                        let cur = selected.read().clone();
                                        selected.set(if cur.as_deref() == Some(opt.as_str()) {
                                            None
                                        } else {
                                            Some(opt.clone())
                                        });
                                    },
                                    "{opt}"
                                }
                            }
                        }
                    }
                }
                div { class: "row",
                    for g in [generation()] {
                        input {
                            key: "{g}",
                            // Stable hook for the debug regression checks.
                            id: "intervention-answer",
                            placeholder: if options.is_empty() { "Type an answer…" } else { "Or type an answer…" },
                            initial_value: "{answer.peek()}",
                            oninput: move |e| answer.set(e.value()),
                        }
                    }
                    button {
                        class: "btn primary",
                        disabled: !can_send,
                        onclick: move |_| {
                            let mut parts: Vec<String> = Vec::new();
                            if let Some(opt) = selected.read().clone() {
                                parts.push(opt);
                            }
                            let typed = answer.read().trim().to_string();
                            if !typed.is_empty() {
                                parts.push(typed);
                            }
                            if !parts.is_empty() {
                                state.send(ExecutorCommand::Answer { card_id, text: parts.join("\n\n") });
                                selected.set(None);
                                answer.set(String::new());
                                generation += 1;
                            }
                        },
                        "Send answer"
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
        CardState::MergedWithoutReview { merged: true } => "ext-merged",
        CardState::MergedWithoutReview { merged: false } => "ext-closed",
        CardState::Done => "done",
        CardState::Failed { .. } => "failed",
        CardState::Answering { .. } => "answering",
    }
}
