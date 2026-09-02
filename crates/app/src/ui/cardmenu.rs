//! The per-entity actions dropdown (opened from the "⌄" chevron on a board card).
//!
//! Serves both boards: a card's lifecycle actions, and a PR-under-review's. The
//! ones that mean the same thing on both — show the diff, open in terminal, open
//! in editor — are shared verbatim, which is the point: "open this in my editor"
//! shouldn't be a card-only privilege just because review mode was built later.
//! "Bump to front of queue" goes one step further: nothing about it differs per
//! kind (the queue is global and keyed by entity id), so the host renders it
//! itself, above whichever per-kind list follows.
//!
//! Like the confirm and toast hosts, it renders once at the app root from a
//! global signal: an entity's overflow-clipping scroll column would otherwise cut
//! the dropdown off, so the menu floats above the board at the click point.

use dioxus::prelude::*;
use usine_core::{ExecutorCommand, OpenTarget};
use uuid::Uuid;

use super::confirm::{request_confirm, request_confirm_with_note, ConfirmAction, ConfirmRequest};
use super::diffdialog::{open_diff_dialog, open_review_diff};
use crate::state::AppState;

/// Which board's entity the menu was opened on, plus the per-kind state that
/// decides which entries apply.
#[derive(Clone, PartialEq)]
pub(crate) enum MenuKind {
    Card {
        /// "Back to starting block" only applies once a card has left the start.
        can_reset: bool,
        /// "Mark as done" is hidden when the card is already done.
        can_done: bool,
        /// "Show diff" shows once the card has committed work to diff.
        can_diff: bool,
        /// "Open in terminal/editor" only when there's somewhere to open: any
        /// non-done card (the open falls back to the project), or a done card
        /// whose worktree still exists on disk.
        can_open: bool,
        /// The card's cosmetic "blocked" marker — decides which way the
        /// mark/unmark entry reads. Offered in every state.
        blocked: bool,
        /// The message currently stored on a blocked card, used to prefill the
        /// edit dialog and to label it. Separate from `blocked`: a card can be
        /// blocked with nothing written down.
        blocked_note: Option<String>,
    },
    Review {
        pr_number: u64,
        /// Whether a checkout exists (or can be made) to open or run. False once
        /// the review is published and its worktree has been torn down.
        has_checkout: bool,
        /// Whether the published review pledged a fix we're still on the hook
        /// for — a running fix run, or one waiting at the push gate. Dismissing
        /// such a PR would drop the promise silently *and* blacklist it, so the
        /// menu offers the fix's own discard instead.
        has_fix: bool,
    },
}

/// A request to open the actions menu, anchored at viewport (`x`, `y`).
///
/// The preview controls used to live here; they're now icon buttons on the card
/// itself, where their live status (idle / starting / running) is visible without
/// opening anything.
#[derive(Clone)]
pub(crate) struct CardMenuRequest {
    pub kind: MenuKind,
    /// The card id or review-task id, depending on `kind`.
    pub target_id: Uuid,
    pub title: String,
    pub x: f64,
    pub y: f64,
}

static CARD_MENU: GlobalSignal<Option<CardMenuRequest>> = Signal::global(|| None);

pub(crate) fn open_card_menu(req: CardMenuRequest) {
    *CARD_MENU.write() = Some(req);
}

fn dismiss() {
    *CARD_MENU.write() = None;
}

/// Close the menu from outside — the app-level Escape handler, which has to
/// close the topmost thing rather than the panel behind it.
pub(crate) fn close_card_menu() {
    dismiss();
}

/// Whether a menu is open, so Escape can tell "close the menu" from "close the
/// panel" without reaching into the DOM.
pub(crate) fn card_menu_open() -> bool {
    CARD_MENU.read().is_some()
}

#[component]
pub fn CardMenuHost() -> Element {
    // Before the early return: the host re-renders as the menu opens and closes,
    // and a hook that only runs on one of those paths is a hook-order break.
    let state = use_context::<AppState>();
    let req = CARD_MENU.read().clone();
    let Some(req) = req else {
        return rsx! {};
    };
    let pos = format!("left: {}px; top: {}px;", req.x, req.y);
    let id = req.target_id;

    rsx! {
        // Full-viewport catcher: a click anywhere outside the menu closes it.
        div { class: "menu-backdrop", onclick: move |_| dismiss(),
            div {
                class: "card-menu",
                style: "{pos}",
                onclick: move |e| e.stop_propagation(),
                // Read live rather than via `MenuKind` (which is snapshotted at
                // click time): queue position moves under an open menu, and
                // reading `run_queue` here subscribes the host, so the entry
                // disappears the moment the run launches. Hidden at position 1 —
                // bumping the head is a no-op.
                if state.queue_position(id).is_some_and(|n| n > 1) {
                    button {
                        class: "menu-item",
                        onclick: move |_| {
                            dismiss();
                            state.send(ExecutorCommand::BumpQueued { id });
                        },
                        "Bump to front of queue"
                    }
                }
                match req.kind.clone() {
                    MenuKind::Card { can_reset, can_done, can_diff, can_open, blocked, blocked_note } => rsx! {
                        CardMenuItems {
                            card_id: id,
                            title: req.title.clone(),
                            can_reset,
                            can_done,
                            can_diff,
                            can_open,
                            blocked,
                            blocked_note,
                        }
                    },
                    MenuKind::Review { pr_number, has_checkout, has_fix } => rsx! {
                        ReviewMenuItems { review_id: id, pr_number, has_checkout, has_fix }
                    },
                }
            }
        }
    }
}

#[component]
fn CardMenuItems(
    card_id: Uuid,
    title: String,
    can_reset: bool,
    can_done: bool,
    can_diff: bool,
    can_open: bool,
    blocked: bool,
    blocked_note: Option<String>,
) -> Element {
    let state = use_context::<AppState>();
    let id = card_id;
    // Marking asks for an optional message (a card that waits on something
    // outside Usine should say what); unmarking is a plain one-click send —
    // there's nothing to say about a card that no longer waits on anything.
    // Editing reopens the same dialog prefilled, below the toggle.
    let block_label = if blocked {
        "Mark unblocked"
    } else {
        "Mark blocked"
    };
    // A card blocked before this existed — or blocked without typing anything —
    // has nothing to "edit".
    let note_label = if blocked_note.is_some() {
        "Edit blocked message"
    } else {
        "Add blocked message"
    };
    // One title clone per handler (each closure moves its own copy).
    let reset_title = title.clone();
    let done_title = title.clone();
    let del_title = title.clone();
    let block_title = title.clone();
    let edit_title = title.clone();

    rsx! {
        if can_diff {
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    open_diff_dialog(id);
                },
                "Show diff"
            }
        }
        if can_open {
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    state.send(ExecutorCommand::OpenWorktree { card_id: id, target: OpenTarget::Terminal });
                },
                "Open in terminal"
            }
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    state.send(ExecutorCommand::OpenWorktree { card_id: id, target: OpenTarget::Editor });
                },
                "Open in editor"
            }
        }
        if can_reset {
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    request_confirm(ConfirmRequest {
                        title: "Back to starting block".into(),
                        message: format!(
                            "Send “{reset_title}” back to the starting block? This discards its current progress; any questions and answers so far are appended to the prompt."
                        ),
                        confirm_label: "Send back".into(),
                        danger: false,
                        action: ConfirmAction::Send(ExecutorCommand::BackToStart { card_id: id }),
                    });
                },
                "Back to starting block"
            }
        }
        if can_done {
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    request_confirm(ConfirmRequest {
                        title: "Mark as done".into(),
                        message: format!(
                            "Mark “{done_title}” as done? This stops any active run and preview."
                        ),
                        confirm_label: "Mark done".into(),
                        danger: false,
                        action: ConfirmAction::Send(ExecutorCommand::MarkDone { card_id: id }),
                    });
                },
                "Mark as done"
            }
        }
        button {
            class: "menu-item",
            onclick: move |_| {
                dismiss();
                if blocked {
                    state
                        .send(ExecutorCommand::SetBlocked {
                            card_id: id,
                            blocked: false,
                            note: None,
                        });
                } else {
                    request_confirm(ConfirmRequest {
                        title: "Mark blocked".into(),
                        message: format!(
                            "Mark “{block_title}” as blocked? It stops counting toward the attention badges and its board buttons are hidden until you unmark it.",
                        ),
                        confirm_label: "Mark blocked".into(),
                        danger: false,
                        action: ConfirmAction::BlockCard(id),
                    });
                }
            },
            "{block_label}"
        }
        // A blocked card's message is only editable here: unmarking drops it
        // (`Card::set_blocked`), so "unblock, re-block, retype" used to be the
        // only way to fix a typo. Saving blank clears the message and leaves the
        // card blocked.
        if blocked {
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    request_confirm_with_note(
                        ConfirmRequest {
                            title: note_label.into(),
                            message: format!(
                                "Message shown on “{edit_title}” while it stays blocked. Leave it empty to drop the message.",
                            ),
                            confirm_label: "Save".into(),
                            danger: false,
                            action: ConfirmAction::BlockCard(id),
                        },
                        blocked_note.clone().unwrap_or_default(),
                    );
                },
                "{note_label}"
            }
        }
        button {
            class: "menu-item danger",
            onclick: move |_| {
                dismiss();
                request_confirm(ConfirmRequest {
                    title: "Delete card".into(),
                    message: format!("Delete “{del_title}”? This can't be undone."),
                    confirm_label: "Delete".into(),
                    danger: true,
                    action: ConfirmAction::DeleteCard(id),
                });
            },
            "Delete"
        }
    }
}

/// A PR-under-review's actions. "Show diff" works in every state (the PR head is
/// fetched on demand); opening and running need a checkout, which every state but
/// `Reviewed` either has or can make.
///
/// The last entry is the way out, and which one that is depends on whether we
/// owe the author a fix: a published pledge is discarded (retracted on the PR,
/// PR stays on the board), anything else is dismissed (permanent, silent).
#[component]
fn ReviewMenuItems(review_id: Uuid, pr_number: u64, has_checkout: bool, has_fix: bool) -> Element {
    let state = use_context::<AppState>();
    let id = review_id;

    rsx! {
        button {
            class: "menu-item",
            onclick: move |_| {
                dismiss();
                open_review_diff(id);
            },
            "Show diff"
        }
        if has_checkout {
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    state.send(ExecutorCommand::OpenReviewWorktree { review_id: id, target: OpenTarget::Terminal });
                },
                "Open in terminal"
            }
            button {
                class: "menu-item",
                onclick: move |_| {
                    dismiss();
                    state.send(ExecutorCommand::OpenReviewWorktree { review_id: id, target: OpenTarget::Editor });
                },
                "Open in editor"
            }
        }
        if has_fix {
            // The review is on GitHub promising a fix: the way out is the gate's
            // discard, which retracts that promise and leaves the PR on the board.
            button {
                class: "menu-item danger",
                onclick: move |_| {
                    dismiss();
                    super::confirm_discard_review_fix(id);
                },
                "Discard the fix"
            }
        } else {
            button {
                class: "menu-item danger",
                onclick: move |_| {
                    dismiss();
                    super::confirm_dismiss_review(id, pr_number);
                },
                "Dismiss"
            }
        }
    }
}
