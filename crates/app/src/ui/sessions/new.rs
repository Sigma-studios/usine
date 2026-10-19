//! The new-session dialog: folder, provider/model/access, first message (with
//! attachments). Same host pattern as `newcard`: a global opens it, the host
//! renders it at the app root.
//!
//! The session id is minted when the dialog opens, so files can be staged
//! under it before the session exists; cancelling detaches them again.

use std::path::PathBuf;

use dioxus::prelude::*;
use usine_core::{ExecutorCommand, ModelSpec, Provider, SessionAccess};
use uuid::Uuid;

use super::detail::drop_files;
use super::{
    claude_code_folders, dedup_existing, expand_tilde, folder_suggestions, home_dir, shorten_home,
};
use crate::state::AppState;
use crate::ui::detail::{attach_from_clipboard, AttachButton, AttachmentChips};
use crate::ui::widgets::{parse_provider, provider_value, ModelEffortPicker};

/// The open dialog's pre-minted session id; `None` when closed.
static OPEN: GlobalSignal<Option<Uuid>> = Signal::global(|| None);

/// Whether the open dialog has sent its `CreateSession` and is waiting for the
/// session to exist. It stays open until then, so a rejected start (the folder
/// vanished, the provider failed before persisting) keeps the typed message.
static STARTING: GlobalSignal<bool> = Signal::global(|| false);

/// The executor rejected a command for `id`: if it is the dialog's pending
/// start, re-enable the dialog (the error itself is toasted).
pub(crate) fn session_start_rejected(id: Uuid) {
    if *OPEN.peek() == Some(id) && *STARTING.peek() {
        *STARTING.write() = false;
    }
}

/// Open the new-session dialog (a no-op when it is already open).
pub(crate) fn open_new_session() {
    if OPEN.peek().is_none() {
        *STARTING.write() = false;
        *OPEN.write() = Some(Uuid::new_v4());
    }
}

#[component]
pub(crate) fn NewSessionHost() -> Element {
    let Some(draft_id) = *OPEN.read() else {
        return rsx! {};
    };
    rsx! {
        NewSessionDialog { key: "{draft_id}", draft_id }
    }
}

#[component]
fn NewSessionDialog(draft_id: Uuid) -> Element {
    let state = use_context::<AppState>();
    let home = home_dir();
    let defaults = state.settings.peek().session_defaults.clone();
    let mut provider = use_signal(|| {
        defaults
            .as_ref()
            .map(|d| d.provider)
            .unwrap_or(state.settings.peek().default_provider)
    });
    let mut spec = use_signal(|| {
        defaults
            .as_ref()
            .map(|d| d.spec.clone())
            .unwrap_or_else(|| state.settings.peek().default_implement.clone())
    });
    let mut access = use_signal(|| defaults.as_ref().map(|d| d.access).unwrap_or_default());
    let suggestions = use_hook(|| {
        let sessions: Vec<_> = state.sessions.peek().values().cloned().collect();
        let projects: Vec<PathBuf> = state
            .projects
            .peek()
            .iter()
            .map(|p| p.path.clone())
            .collect();
        folder_suggestions(&sessions, &projects)
    });
    let mut folder = use_signal(|| {
        suggestions
            .first()
            .map(|p| shorten_home(p, home.as_deref()))
            .unwrap_or_default()
    });
    let mut more = use_signal(|| None::<Vec<PathBuf>>);
    let mut prompt = use_signal(String::new);
    let mut dragging = use_signal(|| false);

    let resolved = expand_tilde(&folder.read(), home.as_deref());
    let folder_ok = resolved.is_absolute() && resolved.is_dir();
    let has_files = !state.card_attachments(draft_id).is_empty();
    let starting = STARTING();
    let can_start = !starting && folder_ok && (!prompt.read().trim().is_empty() || has_files);
    let hint = crate::ui::shortcuts::submit_hint();

    // Closing without starting drops the files staged under the draft id.
    // Not while a start is pending: those files are the session's now.
    let cancel = move || {
        if *STARTING.peek() {
            return;
        }
        for p in state.card_attachments(draft_id) {
            state.detach_image(draft_id, p);
        }
        *OPEN.write() = None;
    };

    let start = move || {
        let folder = expand_tilde(&folder.peek(), home_dir().as_deref());
        state.send(ExecutorCommand::CreateSession {
            id: draft_id,
            folder,
            provider: *provider.peek(),
            spec: spec.peek().clone(),
            access: *access.peek(),
            prompt: prompt.peek().clone(),
        });
        *STARTING.write() = true;
    };

    // The session exists: the start went through, hand over to its detail.
    use_effect(move || {
        if STARTING() && state.sessions.read().contains_key(&draft_id) {
            *STARTING.write() = false;
            *OPEN.write() = None;
            state.show_sessions(Some(draft_id));
        }
    });

    let current_provider = provider();
    let current_spec = spec();
    let current_access = access();
    let home_for_list = home.clone();

    rsx! {
        div { class: "modal-overlay confirm-overlay", onclick: move |_| cancel(),
            div {
                class: "modal new-session",
                "data-submit-scope": "1",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        cancel();
                    }
                },
                ondragover: move |e| {
                    e.prevent_default();
                    dragging.set(true);
                },
                ondragleave: move |_| dragging.set(false),
                ondrop: move |e| {
                    e.prevent_default();
                    dragging.set(false);
                    drop_files(state, draft_id, &e);
                },
                h3 { class: "modal-title", "New session" }
                div { class: "field",
                    label { r#for: "session-folder", "Folder" }
                    div { class: "row",
                        input {
                            id: "session-folder",
                            value: "{folder}",
                            placeholder: "~/path/to/folder",
                            oninput: move |e| folder.set(e.value()),
                        }
                        button {
                            class: "btn",
                            onclick: move |_| {
                                spawn(async move {
                                    let mut dialog = rfd::AsyncFileDialog::new();
                                    if let Some(h) = home_dir() {
                                        dialog = dialog.set_directory(h);
                                    }
                                    if let Some(handle) = dialog.pick_folder().await {
                                        folder.set(shorten_home(handle.path(), home_dir().as_deref()));
                                    }
                                });
                            },
                            "Browse…"
                        }
                    }
                    if !folder.read().trim().is_empty() && !folder_ok {
                        div { class: "hint warn", "Not an existing folder." }
                    }
                    if !suggestions.is_empty() {
                        div { class: "folder-suggestions",
                            for p in suggestions.iter() {
                                {
                                    let label = shorten_home(p, home_for_list.as_deref());
                                    let pick = label.clone();
                                    let active = resolved == *p;
                                    rsx! {
                                        button {
                                            key: "{label}",
                                            class: if active { "folder-chip active" } else { "folder-chip" },
                                            title: "{p.display()}",
                                            onclick: move |_| folder.set(pick.clone()),
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                    match more() {
                        None => rsx! {
                            button {
                                class: "btn link",
                                title: "Folders your recent Claude Code sessions ran in",
                                onclick: {
                                    let known = suggestions.clone();
                                    move |_| {
                                        let found = home_dir()
                                            .map(|h| claude_code_folders(&h, 12))
                                            .unwrap_or_default();
                                        more.set(Some(dedup_existing(found, &known)));
                                    }
                                },
                                "More from Claude Code…"
                            }
                        },
                        Some(list) if list.is_empty() => rsx! {
                            div { class: "hint", "No other recent Claude Code folders." }
                        },
                        Some(list) => rsx! {
                            div { class: "folder-suggestions",
                                for p in list.iter() {
                                    {
                                        let label = shorten_home(p, home_for_list.as_deref());
                                        let pick = label.clone();
                                        rsx! {
                                            button {
                                                key: "{label}",
                                                class: "folder-chip",
                                                title: "{p.display()}",
                                                onclick: move |_| folder.set(pick.clone()),
                                                "{label}"
                                            }
                                        }
                                    }
                                }
                            }
                        },
                    }
                }
                div { class: "field",
                    label { "Agent" }
                    div { class: "row",
                        select {
                            value: provider_value(current_provider),
                            onchange: move |e| {
                                let p: Provider = parse_provider(&e.value());
                                if p != *provider.peek() {
                                    provider.set(p);
                                    // Another provider's models don't apply.
                                    spec.set(usine_core::CardConfig::default_for(p).implement);
                                }
                            },
                            for p in Provider::all() {
                                option { value: provider_value(p), selected: p == current_provider, "{p.label()}" }
                            }
                        }
                        ModelEffortPicker {
                            provider: current_provider,
                            spec: current_spec,
                            on_change: move |s: ModelSpec| spec.set(s),
                        }
                    }
                }
                div { class: "field",
                    label { "Access" }
                    div { class: "segmented",
                        button {
                            class: if current_access == SessionAccess::FullAccess { "seg active" } else { "seg" },
                            title: "Edit files and run commands without asking",
                            onclick: move |_| access.set(SessionAccess::FullAccess),
                            "Full access"
                        }
                        button {
                            class: if current_access == SessionAccess::ReadOnly { "seg active" } else { "seg" },
                            title: "Only read the folder",
                            onclick: move |_| access.set(SessionAccess::ReadOnly),
                            "Read-only"
                        }
                    }
                }
                div { class: "field",
                    label { r#for: "session-prompt", "Message" }
                    AttachmentChips { card_id: draft_id }
                    div { class: if dragging() { "field attach-field drop-active" } else { "field attach-field" },
                        textarea {
                            id: "session-prompt",
                            rows: "5",
                            placeholder: "What should the agent do? (paste, drop or attach files)",
                            initial_value: "{prompt.peek()}",
                            oninput: move |e| prompt.set(e.value()),
                            onpaste: move |_| attach_from_clipboard(state, draft_id),
                            onmounted: move |e: MountedEvent| {
                                spawn(async move {
                                    let _ = e.data().set_focus(true).await;
                                });
                            },
                        }
                        AttachButton {
                            card_id: draft_id,
                            icon: true,
                            title: "Sent with the first message".to_string(),
                        }
                    }
                }
                div { class: "modal-actions",
                    button { class: "btn", disabled: starting, onclick: move |_| cancel(), "Cancel" }
                    button {
                        class: "btn primary",
                        "data-submit": "1",
                        title: "Start ({hint})",
                        disabled: !can_start,
                        onclick: move |_| {
                            if can_start {
                                start();
                            }
                        },
                        if starting { "Starting…" } else { "Start session" }
                    }
                }
            }
        }
    }
}
