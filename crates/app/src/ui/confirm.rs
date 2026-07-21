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

pub(crate) fn request_confirm(req: ConfirmRequest) {
    *CONFIRM.write() = Some(req);
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
                // Focus the dialog on open so Escape works and focus is trapped here.
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                h3 { class: "modal-title", "{req.title}" }
                div { class: "modal-body", "{req.message}" }
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
