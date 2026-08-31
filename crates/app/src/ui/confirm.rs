//! In-app confirmation modal, themed to match the rest of the UI (replaces the
//! native OS prompts). A single global holds the pending request; `ConfirmHost`
//! renders the overlay at the app root, like the toast host.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;

/// What to do if the user confirms.
#[derive(Clone)]
pub(crate) enum ConfirmAction {
    Send(ExecutorCommand),
    DeleteCard(Uuid),
    DeleteProject(Uuid),
    /// Submit a drafted PR review, then tear down both validation surfaces (the
    /// shared edit buffer and the diff viewer) — leaving either one open on a
    /// review that has already been posted invites a double submission.
    PublishReview(ExecutorCommand),
    /// Mark a card blocked, carrying the optional message typed in the dialog.
    /// Raised for *marking* and for editing an already-blocked card's message —
    /// both send `blocked: true`. Unmarking doesn't ask.
    BlockCard(Uuid),
}

#[derive(Clone)]
pub(crate) struct ConfirmRequest {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub danger: bool,
    pub action: ConfirmAction,
}

static CONFIRM: GlobalSignal<Option<ConfirmRequest>> = Signal::global(|| None);

/// The message typed in a `BlockCard` dialog. Global for the same reason
/// `CONFIRM` is: the host outlives every individual dialog, so the buffer has to
/// be reset by whoever opens one.
static NOTE: GlobalSignal<String> = Signal::global(String::new);

pub(crate) fn request_confirm(req: ConfirmRequest) {
    *NOTE.write() = String::new();
    *CONFIRM.write() = Some(req);
}

/// Same as [`request_confirm`], but starts the message field from `note` —
/// "Edit blocked message" edits what's already on the card rather than making
/// you type it again.
pub(crate) fn request_confirm_with_note(req: ConfirmRequest, note: String) {
    request_confirm(req);
    *NOTE.write() = note;
}

fn dismiss() {
    *CONFIRM.write() = None;
}

#[component]
pub fn ConfirmHost() -> Element {
    let state = use_context::<AppState>();
    let req = CONFIRM.read().clone();
    let Some(req) = req else {
        return rsx! {};
    };
    let confirm_class = if req.danger {
        "btn danger"
    } else {
        "btn primary"
    };
    let action = req.action.clone();
    // Only this action collects text, so the field is driven by the variant
    // rather than by a `note` field every other request site would have to fill.
    let with_note = matches!(req.action, ConfirmAction::BlockCard(_));

    rsx! {
        div { class: "modal-overlay confirm-overlay", onclick: move |_| dismiss(),
            div {
                class: "modal",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                // Don't dismiss when clicking inside the dialog.
                onclick: move |e| e.stop_propagation(),
                // Escape closes the dialog (matches the overlay-click behaviour).
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        dismiss();
                    }
                },
                // Focus the dialog on open so Escape works and focus is trapped
                // here — unless the message field takes focus instead (the two
                // would race). Escape still works from the field: the keydown
                // bubbles up to this handler.
                onmounted: move |e: MountedEvent| {
                    if with_note {
                        return;
                    }
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                h3 { class: "modal-title", "{req.title}" }
                div { class: "modal-body", "{req.message}" }
                if with_note {
                    div { class: "field",
                        label { r#for: "confirm-note", "Message (optional)" }
                        textarea {
                            id: "confirm-note",
                            value: "{NOTE}",
                            oninput: move |e| *NOTE.write() = e.value(),
                            // Focus the field, then park the caret after any
                            // prefilled text — "Edit blocked message" opens with
                            // the existing note, and a caret left at 0 would make
                            // typing prepend to it.
                            onmounted: move |e: MountedEvent| {
                                spawn(async move {
                                    let _ = e.data().set_focus(true).await;
                                    dioxus::document::eval(
                                        "requestAnimationFrame(function(){\
                                           var el = document.getElementById('confirm-note');\
                                           if (el) { el.selectionStart = el.selectionEnd = el.value.length; }\
                                         });",
                                    );
                                });
                            },
                        }
                    }
                }
                div { class: "modal-actions",
                    button { class: "btn", onclick: move |_| dismiss(), "Cancel" }
                    button {
                        class: "{confirm_class}",
                        onclick: move |_| {
                            match action.clone() {
                                ConfirmAction::Send(cmd) => state.send(cmd),
                                ConfirmAction::DeleteCard(id) => state.delete_card(id),
                                ConfirmAction::DeleteProject(id) => state.delete_project(id),
                                ConfirmAction::PublishReview(cmd) => {
                                    state.send(cmd);
                                    super::finish_review_validation();
                                }
                                ConfirmAction::BlockCard(id) => {
                                    state.send(ExecutorCommand::SetBlocked {
                                        card_id: id,
                                        blocked: true,
                                        // Trim / blank -> None lives in `Card::set_blocked`.
                                        note: Some(NOTE.peek().clone()),
                                    })
                                }
                            }
                            dismiss();
                        },
                        "{req.confirm_label}"
                    }
                }
            }
        }
    }
}
