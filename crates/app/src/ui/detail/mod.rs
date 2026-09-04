//! The right-hand detail panel. `DetailArea` picks the panel for the active
//! board: `CardDetail` for the card board, `ReviewDetail` for the PR-review
//! board. `CardPanel` then dispatches to the per-phase sub-panels, which live in
//! the sibling files of this module.

use dioxus::prelude::*;
use usine_core::{
    Card, CardState, CheckStatus, DesignSub, DiffState, ExecutorCommand, Handoff, PrReviewSub,
    ReviewSub, CONFLICT_INTERVENTION_ID,
};
use uuid::Uuid;

use super::confirm::{request_confirm, ConfirmAction, ConfirmRequest};
use super::diffdialog::{open_diff_dialog, open_diff_dialog_at};
use super::icons::IconDiff;
use crate::state::{AppState, BoardMode};
use crate::ui::widgets::{provider_value, same_path, ArtifactTabs, ArtifactText};
use crate::ui::{Panel, PanelResizer};

mod chat;
mod conclusion;
mod done;
mod edit;
mod fixes;
pub(super) mod plan;
mod pr_create;
mod pr_review;
mod review;
mod transcript;

use chat::AgentChatSection;
use conclusion::ConclusionPanel;
use done::{DonePanel, OutcomeArtifacts};
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
            // From the card, not the state alone: a `ReadyToMerge` card with
            // conflicts, a red build or untriaged comments must not read
            // "ready to merge" beside a button that says otherwise.
            let status = card.status_label();
            let cost = card.cost;
            // Kept in step with the marker by `Card::set_blocked`: a note only
            // exists while the card is blocked.
            let blocked_note = card.blocked_note.clone();
            // Only point at the note when there is one to read.
            let blocked_title = if blocked_note.is_some() {
                "Marked blocked — this card doesn't count toward the attention badge; the message below is the user's own note"
            } else {
                "Marked blocked — this card doesn't count toward the attention badge"
            };
            let state_key = state_discriminant(&card.state);
            // Same gap the board card covers: a lifecycle command is working but
            // hasn't transitioned the card yet, so the panel still shows the
            // buttons that led here (notably "Approve & implement").
            let busy = state.busy.read().contains(&id);
            // The two states that WRAP another one — a question run and a fault
            // — render as a banner ABOVE the panel of the state underneath,
            // instead of replacing it. Asking a question used to take "What was
            // done", the merge gate and the chat log off screen for the duration
            // (and a fault took them for good); now they stay where the user
            // left them, frozen. Frozen is not a taste call: a command sent from
            // `Answering`/`Failed` fails the state guard and toasts an illegal
            // transition, so the underlying actions must be disabled — which is
            // exactly what `.is-busy` already does. The banner's own buttons sit
            // outside `.detail-body`, so they stay live.
            let question = match &card.state {
                CardState::Answering { question, .. } => Some(question.clone()),
                _ => None,
            };
            let fail_msg = match &card.state {
                CardState::Failed { message, .. } => Some(message.clone()),
                _ => None,
            };
            let interrupted = fail_msg
                .as_deref()
                .is_some_and(|m| m.starts_with("Interrupted"));
            let recover_label = if interrupted { "Resume" } else { "Retry" };
            let fail_display = fail_msg.as_ref().map(|m| {
                if interrupted {
                    m.clone()
                } else {
                    format!("Run failed: {m}")
                }
            });
            let body_class = if busy || question.is_some() || fail_display.is_some() {
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
                    PanelResizer { panel: Panel::Detail }
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
                            // Read-only here — the toggle lives in the board's
                            // actions dropdown — but an open card should still
                            // explain why it stopped badging.
                            if card.blocked {
                                span {
                                    class: "badge blocked",
                                    title: "{blocked_title}",
                                    "blocked"
                                }
                            }
                            if !cost.is_zero() {
                                span { class: "badge cost", "{cost}" }
                            }
                        }
                        // The reason left when marking the card blocked, in full
                        // (the board card clamps it).
                        if let Some(note) = blocked_note {
                            div { class: "blocked-note", "{note}" }
                        }
                    }
                    if let Some(q) = question {
                        div { class: "detail-banner",
                            div { class: "row",
                                span { class: "spinner" }
                                span { "The agent is answering: {q}" }
                            }
                            button {
                                class: "btn subtle",
                                onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                                "Cancel"
                            }
                        }
                    }
                    if let Some(msg) = fail_display {
                        div { class: "detail-banner danger",
                            div { "{msg}" }
                            button {
                                class: "btn",
                                onclick: move |_| state.send(ExecutorCommand::Retry { card_id: id }),
                                "{recover_label}"
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

    // The gate sections dispatch on the state seen THROUGH a question or a
    // fault (`effective()`), so neither takes the panel away — the banner in
    // `CardDetail` says what is happening and freezes the body. The live-run
    // bits below (the running-phase Stop, the intervention question) keep
    // reading the raw state: they describe the run that is actually in flight.
    let st = card.state.effective();
    let is_start = matches!(st, CardState::StartingBlock);
    let is_ready = matches!(st, CardState::ReadyToMerge);
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

    rsx! {
        if is_start {
            EditableTask { card: card.clone() }
            ConfigForm { card: card.clone() }
        } else if !card.description.trim().is_empty() {
            // Past the starting block the task is reference material, not
            // something to act on — but it used to open every panel with up to
            // 240px of scrolling prose. Collapsed behind the same muted caret
            // summary the fix picker and the Q&A log already use. (A card with
            // no description gets no section at all: "No description." is a row
            // of panel spent saying nothing.)
            details { class: "section task-collapsed",
                summary {
                    title: "{card.description}",
                    "Task — {chat::summary_line(&card.description)}"
                }
                div { class: "plan-box", "{card.description}" }
            }
        }
        // Not just the starting block: a screenshot is exactly the thing you
        // want to hand a card you are already reviewing, and the chat and
        // revise paths send attachments along. Where the panel renders a chat
        // box, the control lives *in* that box instead (`chat::renders_chat`),
        // next to the change it is evidence for. The terminal states keep the
        // chips but lose the picker — nothing there can read a new attachment,
        // yet what was attached earlier still has to be visible and removable.
        if !chat::renders_chat(&card.state) {
            Attachments {
                card_id: id,
                can_attach: !matches!(st, CardState::Done | CardState::MergedWithoutReview { .. }),
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

        // A conflict-resolution run that stopped rather than guessing. Say what
        // it did get through, and make plain that nothing has been published —
        // the InterventionPanel below carries the question itself. Keyed on the
        // conflict request id, not the state: a fix run's live `AskUserQuestion`
        // (a review-comment or CI fix) parks in the same sub-state, and none of
        // this copy is true of it — that one just gets the panel below.
        if card
            .state
            .intervention()
            .is_some_and(|i| i.request_id == CONFLICT_INTERVENTION_ID)
        {
            div { class: "section",
                h3 { "Conflict resolution needs a decision" }
                div { class: "hint",
                    "Nothing has been published — the PR is exactly as it was."
                }
                if let Some(recap) = recap.clone() {
                    div { class: "hint", "What it got through" }
                    ArtifactText { text: recap }
                }
                button {
                    class: "btn subtle",
                    onclick: move |_| request_confirm(ConfirmRequest {
                        title: "Stop the conflict resolution?".into(),
                        message: "Discard everything the agent resolved and return the card to the PR gate? You can then resolve the conflicts yourself, or ask the agent again.".into(),
                        confirm_label: "Stop".into(),
                        danger: true,
                        action: ConfirmAction::Send(ExecutorCommand::Cancel { card_id: id }),
                    }),
                    title: "Discards everything the agent resolved in the card's worktree and returns the card to the PR gate. Answering instead resumes the resolution where it left off.",
                    "Stop and resolve it myself"
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

        if let CardState::Designing(DesignSub::AwaitingApproval { plan }) = st {
            // Stable anchors for the board buttons that only open the card:
            // they scroll here rather than looking like they did nothing.
            div { class: "section-group", id: "plan-approval",
                PlanApproval { card_id: id, plan: plan.clone() }
            }
        }

        if let CardState::Concluded { conclusion } = st {
            div { class: "section-group", id: "conclusion",
                ConclusionPanel { card_id: id, conclusion: conclusion.clone() }
            }
        }

        if matches!(st, CardState::AwaitingReview(_)) {
            div { class: "section-group", id: "pr-create",
                PrCreateForm { card: card.clone() }
            }
        }

        if matches!(st, CardState::PrReview(_)) {
            PrReviewPanel { card: card.clone() }
        }

        if let CardState::PrReview(PrReviewSub::SelectingFixes { verdicts }) = st {
            div { class: "section-group", id: "fix-selection",
                FixSelection { card_id: id, verdicts: verdicts.clone(), self_review: false }
            }
        }

        if let CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) = st {
            div { class: "section-group", id: "fix-selection",
                FixSelection { card_id: id, verdicts: verdicts.clone(), self_review: true }
            }
        }

        if is_ready && (card.unanswered_count > 0 || pending_bodies > 0) {
            div { class: "section",
                h3 { "New review comments" }
                div { class: "hint",
                    if card.unanswered_count == 0 {
                        // Only review bodies await — a body-only review (e.g. a
                        // bot report) landed after the card reached the gate.
                        "A review's summary text hasn't been read yet."
                    } else if card.unanswered_count == 1 {
                        "1 review comment has no answer yet."
                    } else {
                        {format!("{} review threads have no answer yet.", card.unanswered_count)}
                    }
                }
                div { class: "row",
                    button {
                        class: "btn primary",
                        title: "They arrived after (or survived) the last pass. The agent reads them, triages each one and proposes which to fix — worth doing before merging.",
                        onclick: move |_| state.send(ExecutorCommand::FetchComments { card_id: id }),
                        "Re-read the review"
                    }
                    // A pending review body can also be dismissed by hand —
                    // same affordance as the PR-review panel, so a body landing
                    // at the merge gate doesn't force a full triage run.
                    if pending_bodies > 0 {
                        button {
                            class: "btn",
                            title: "Records the review's summary as handled, locally — nothing is posted. Use it when the body needs no work.",
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
                FixOutcomes { card_id: id }
                if let Some(recap) = recap.clone() {
                    div { class: "hint", "Fixes recap" }
                    ArtifactText {
                        text: recap,
                        on_path: move |path: String| open_diff_dialog_at(id, path),
                    }
                }
                if pr_is_draft {
                    div { class: "hint", "This PR is still a draft — GitHub won't merge it." }
                    button {
                        class: "btn primary",
                        title: "Flips the PR from draft to ready for review on GitHub; the merge button comes back once it is",
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
                        div { class: "hint", "The PR conflicts with the base branch." }
                        div { class: "option-row",
                            button {
                                class: "btn primary",
                                title: "The agent merges the base branch into this one in the card's worktree, resolves the conflicts and pushes. Nothing is published unless it succeeds; GitHub can't merge a conflicting PR either way.",
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
                            div { class: "hint", "The PR's CI checks are failing." }
                            div { class: "option-row",
                                button {
                                    class: "btn primary",
                                    title: "The agent reads the failing run's logs, fixes the cause in the card's worktree and pushes",
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
                            div { class: "hint", "The PR's CI checks are still running." }
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
                on_request: move |fb: String| {
                    state.send(ExecutorCommand::RequestPostPrChange { card_id: id, feedback: fb });
                },
            }
        }

        if let CardState::MergedWithoutReview { merged } = st {
            div { class: "section",
                h3 { if *merged { "Merged without review" } else { "PR closed" } }
                if let Some(p) = card.pr.clone() {
                    PrLink { number: p.number, url: p.url }
                }
                div {
                    class: "hint",
                    title: if *merged {
                        "The work is on the base branch; the worktree was cleaned up and the branch left alone. Mark the card done, or send it back to start, from the card menu."
                    } else {
                        "The branch was left alone in case the work is still wanted. Mark the card done, or send it back to start, from the card menu."
                    },
                    if *merged {
                        "Merged on GitHub before its review finished here."
                    } else {
                        "Closed on GitHub without merging."
                    }
                }
            }
            // Terminal too, and just as bare — show what the run produced.
            OutcomeArtifacts { card_id: id }
        }

        if matches!(st, CardState::Done) {
            DonePanel { card: card.clone() }
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
    // Amber says "waiting on you"; green once the box holds an answer.
    let qcls = if can_send {
        "question answered"
    } else {
        "question"
    };

    rsx! {
        div { class: "section",
            h3 { "Needs your input" }
            div { class: "{qcls}",
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
                            // Paste a screenshot straight into the answer.
                            onpaste: move |_| edit::attach_from_clipboard(state, card_id),
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

/// The implement run's note to whoever reviews it, tabbed by the kind of thing
/// it is saying: the recap, the files it touched, what to exercise by hand, and
/// what it is unsure about. A tab appears only when the agent filled it in, so a
/// terse hand-off is still a single box. Purely informative — to weigh in on an
/// open question, use the Agent Chat.
#[component]
pub(super) fn HandoffPanel(card_id: Uuid, handoff: Handoff) -> Element {
    let sections: Vec<HandoffTab> = HandoffTab::ALL
        .into_iter()
        .filter(|tab| tab.has_content(&handoff))
        .collect();
    // Summary first when there is one — it is the orienting view — otherwise
    // whatever the agent did fill in.
    let mut active = use_signal(|| sections.first().copied().unwrap_or(HandoffTab::Summary));
    let index = sections.iter().position(|t| *t == active()).unwrap_or(0);
    let shown = sections.get(index).copied();

    rsx! {
        div { class: "section",
            h3 { "What was done" }
            if handoff.malformed {
                div { class: "hint",
                    "The run attached a hand-off block that wasn't valid JSON, so it was dropped. \
                     Its notes are in the transcript."
                }
            }
            if sections.len() > 1 {
                ArtifactTabs {
                    labels: sections.iter().map(|t| t.label().to_string()).collect::<Vec<_>>(),
                    active: index,
                    onselect: {
                        let sections = sections.clone();
                        move |i: usize| {
                            if let Some(tab) = sections.get(i) {
                                active.set(*tab);
                            }
                        }
                    },
                }
            }
            match shown {
                Some(HandoffTab::Summary) => rsx! {
                    ArtifactText {
                        text: handoff.summary.clone(),
                        on_path: move |path: String| open_diff_dialog_at(card_id, path),
                    }
                },
                Some(HandoffTab::Changes) => rsx! {
                    HandoffChanges { card_id, changes: handoff.changes.clone() }
                },
                Some(HandoffTab::Tests) => rsx! {
                    ul { class: "handoff-list",
                        for (i, t) in handoff.tests.iter().enumerate() {
                            li { key: "{i}",
                                if t.verified {
                                    span { class: "badge verified", title: "The run exercised this itself", "✓ run" }
                                }
                                span { class: "handoff-scenario", "{t.scenario}" }
                                if !t.expect.is_empty() {
                                    span { class: "handoff-expect", " → {t.expect}" }
                                }
                            }
                        }
                    }
                },
                Some(HandoffTab::Risks) => rsx! {
                    if !handoff.risks.is_empty() {
                        ul { class: "handoff-list",
                            for (i, r) in handoff.risks.iter().enumerate() {
                                li { key: "{i}", "{r}" }
                            }
                        }
                    }
                    if !handoff.questions.is_empty() {
                        div { class: "hint", "Open questions" }
                        ul { class: "handoff-list",
                            for (i, q) in handoff.questions.iter().enumerate() {
                                li { key: "{i}", "{q}" }
                            }
                        }
                    }
                },
                None => rsx! {},
            }
        }
    }
}

/// The hand-off's sections, in reading order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HandoffTab {
    Summary,
    Changes,
    Tests,
    Risks,
}

impl HandoffTab {
    const ALL: [HandoffTab; 4] = [
        HandoffTab::Summary,
        HandoffTab::Changes,
        HandoffTab::Tests,
        HandoffTab::Risks,
    ];

    fn label(self) -> &'static str {
        match self {
            HandoffTab::Summary => "Summary",
            HandoffTab::Changes => "Changes",
            HandoffTab::Tests => "Test it",
            HandoffTab::Risks => "Risks",
        }
    }

    /// Whether this hand-off has anything to put under the tab. Computed from
    /// the hand-off alone, never from the diff — the tab strip must not shift
    /// under the user when the diff finishes computing.
    fn has_content(self, h: &Handoff) -> bool {
        match self {
            HandoffTab::Summary => !h.summary.is_empty(),
            HandoffTab::Changes => !h.changes.is_empty(),
            HandoffTab::Tests => !h.tests.is_empty(),
            HandoffTab::Risks => !h.risks.is_empty() || !h.questions.is_empty(),
        }
    }
}

/// The files the run says it touched, checked against the diff it actually
/// produced: one row per really-changed file with the agent's description
/// joined on, and a marker on anything it claimed that isn't in the diff.
///
/// The diff is requested on mount rather than on tab click, so the tab is
/// instant instead of spinning — one `git diff` per card opened at the review
/// gate, which is the point of being here.
#[component]
fn HandoffChanges(card_id: Uuid, changes: Vec<usine_core::Change>) -> Element {
    let state = use_context::<AppState>();
    let mut requested = use_signal(|| false);
    use_effect(move || {
        if requested() {
            return;
        }
        if !state.diffs.read().contains_key(&card_id) {
            requested.set(true);
            state.send(ExecutorCommand::ComputeDiff { card_id });
        }
    });

    let diff = state.diffs.read().get(&card_id).cloned();
    // Real changed files first, each carrying whatever the agent said about it;
    // then the claims that matched nothing.
    let mut claimed: Vec<Option<usine_core::Change>> = changes.iter().cloned().map(Some).collect();
    let mut rows: Vec<ChangeRow> = Vec::new();
    if let Some(DiffState::Ready(data)) = &diff {
        for file in &data.files {
            let path = file
                .new_path
                .clone()
                .or_else(|| file.old_path.clone())
                .unwrap_or_default();
            let what = claimed
                .iter_mut()
                .find(|c| c.as_ref().is_some_and(|c| same_path(&c.path, &path)))
                .and_then(|slot| slot.take());
            rows.push(ChangeRow {
                path,
                what: what.as_ref().map(|c| c.what.clone()).unwrap_or_default(),
                kind: what.map(|c| c.kind).unwrap_or_default(),
                stat: Some((file.status, file.added, file.removed)),
            });
        }
    }
    let unmatched_are_claims = matches!(diff, Some(DiffState::Ready(_)));
    for change in claimed.into_iter().flatten() {
        rows.push(ChangeRow {
            path: change.path,
            what: change.what,
            kind: change.kind,
            stat: None,
        });
    }

    rsx! {
        if matches!(diff, Some(DiffState::Computing)) || diff.is_none() {
            div { class: "hint", "Checking the diff…" }
        }
        if let Some(DiffState::Failed(why)) = &diff {
            div { class: "hint", "Couldn't read the diff ({why}) — showing the run's own list." }
        }
        div { class: "change-list",
            for (i, row) in rows.iter().enumerate() {
                div {
                    key: "{i}",
                    class: "change-row",
                    onclick: {
                        let path = row.path.clone();
                        move |_| open_diff_dialog_at(card_id, path.clone())
                    },
                    div { class: "change-head",
                        if let Some((status, added, removed)) = row.stat {
                            span { class: "badge status", "{status_label(status)}" }
                            span { class: "diffstat-add", "+{added}" }
                            span { class: "diffstat-del", "−{removed}" }
                        } else if unmatched_are_claims {
                            span { class: "badge warn", title: "The run listed this file but it isn't in the diff", "not in the diff" }
                        }
                        span { class: "change-path", "{row.path}" }
                        if !row.kind.is_empty() {
                            span { class: "badge kind", "{row.kind}" }
                        }
                    }
                    if !row.what.is_empty() {
                        div { class: "change-what", "{row.what}" }
                    }
                }
            }
        }
    }
}

/// One row of the Changes tab: a path, what (if anything) the run said about it,
/// and its real diffstat when the file is actually in the diff.
struct ChangeRow {
    path: String,
    what: String,
    kind: String,
    stat: Option<(usine_core::FileStatus, u32, u32)>,
}

fn status_label(status: usine_core::FileStatus) -> &'static str {
    match status {
        usine_core::FileStatus::Added => "added",
        usine_core::FileStatus::Deleted => "deleted",
        usine_core::FileStatus::Modified => "modified",
        usine_core::FileStatus::Renamed => "renamed",
        usine_core::FileStatus::Copied => "copied",
    }
}

/// What the last fix run was asked to do, and what it says it did — one row per
/// finding the user ticked in the picker, with the run's outcome joined on by
/// id. This is the answer to "I asked for four things; did four happen?", which
/// the prose recap alone never gave at the gate where it matters.
///
/// Nothing here is posted anywhere. In particular a finding reported `skipped`
/// still has its GitHub thread resolved on commit — that follows the
/// checkboxes, not the outcomes; this panel is what makes it visible.
#[component]
pub(super) fn FixOutcomes(card_id: Uuid) -> Element {
    let state = use_context::<AppState>();
    let report = state.fix_reports.read().get(&card_id).cloned();
    let Some(report) = report else {
        return rsx! {};
    };
    let (addressed, asked) = report.tally();
    let rows = report.rows();
    rsx! {
        div { class: "hint",
            if asked > 0 {
                "Fixes asked for — {addressed} of {asked} reported addressed"
            } else {
                "The run reported outcomes for findings that weren't picked"
            }
        }
        if report.malformed {
            div { class: "hint",
                "The run attached an outcomes block that wasn't valid JSON, so it was dropped."
            }
        }
        div { class: "fix-outcomes",
            for (i, row) in rows.iter().enumerate() {
                div {
                    key: "{i}",
                    class: "fix-outcome-row",
                    onclick: {
                        // Only rows that name a real file open the diff — a
                        // review-body finding's path is a label, and scrolling
                        // to it would open a modal that goes nowhere.
                        let path = row
                            .item
                            .as_ref()
                            .and_then(|i| i.diff_path())
                            .map(str::to_string);
                        move |_| {
                            if let Some(path) = path.clone() {
                                open_diff_dialog_at(card_id, path);
                            }
                        }
                    },
                    div { class: "fix-outcome-head",
                        match &row.outcome {
                            Some(o) => rsx! {
                                span { class: "badge outcome-{o.outcome.label()}", "{o.outcome.label()}" }
                            },
                            None => rsx! {
                                span {
                                    class: "badge warn",
                                    title: "You picked this one, but the run never said what it did about it",
                                    "no outcome reported"
                                }
                            },
                        }
                        match &row.item {
                            Some(item) => rsx! {
                                if !item.severity.is_empty() {
                                    span { class: "badge kind", "{item.severity}" }
                                }
                                span { class: "change-path", "{location(item)}" }
                            },
                            None => rsx! {
                                span {
                                    class: "badge warn",
                                    title: "The run reported an id that wasn't in the picker",
                                    "not picked"
                                }
                            },
                        }
                    }
                    if let Some(item) = &row.item {
                        div { class: "fix-outcome-label", "{item.label}" }
                    }
                    if let Some(o) = &row.outcome {
                        if !o.note.is_empty() {
                            div { class: "change-what", "{o.note}" }
                        }
                    }
                }
            }
        }
    }
}

/// Where a picked finding lives, as the picker showed it.
fn location(item: &usine_core::FixItem) -> String {
    match item.line {
        Some(line) => format!("{}:{}", item.path, line),
        None => item.path.clone(),
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

/// The panel body's remount key. Deliberately computed from
/// [`CardState::effective`]: a question (or a fault) must not tear the body
/// down and rebuild it — that resets every form signal and snaps the scroll
/// position to the top, twice (going in, and coming back out).
fn state_discriminant(s: &CardState) -> &'static str {
    match s.effective() {
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
        CardState::PrReview(PrReviewSub::AwaitingAnswer(_)) => "pr-answer",
        CardState::ReadyToMerge => "ready",
        CardState::MergedWithoutReview { merged: true } => "ext-merged",
        CardState::MergedWithoutReview { merged: false } => "ext-closed",
        CardState::Done => "done",
        // `effective()` never returns either of these.
        CardState::Failed { .. } | CardState::Answering { .. } => "wrapped",
    }
}
