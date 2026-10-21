//! The detail panel of one session: its conversation, the answer panel when
//! the agent asked something, and the composer for the next message.

use std::path::{Path, PathBuf};

use dioxus::html::HasFileData;
use dioxus::prelude::*;
use usine_core::{
    ExecutorCommand, Provider, Session, SessionAccess, SessionStatus, SessionTurn, SessionTurnKind,
};
use uuid::Uuid;

use super::work::{section_bounds, WorkSection};
use super::{ago, home_dir, open_file, request_block_session, reveal, shorten_home};
use crate::state::AppState;
use crate::ui::confirm::{request_confirm_with_note, ConfirmAction, ConfirmRequest};
use crate::ui::detail::{
    attach_from_clipboard, image_mime, AttachButton, AttachmentChips, AttachmentPreview,
    InterventionPanel,
};
use crate::ui::drafts;
use crate::ui::widgets::{model_label, provider_value, ArtifactText};
use crate::ui::{Panel, PanelResizer};

/// Tooltip of the composer's paperclip.
const SESSION_ATTACH_TITLE: &str = "Sent with your next message — Claude reads images and files \
    by path; Codex also receives images directly";

#[component]
pub(crate) fn SessionDetail() -> Element {
    let state = use_context::<AppState>();
    let sel = *state.selected_session.read();
    let Some(session) = sel.and_then(|id| state.session(id)) else {
        return rsx! {};
    };
    let id = session.id;
    let home = home_dir();
    let folder = session.folder.clone();
    let folder_label = shorten_home(&folder, home.as_deref());
    let provider = session.provider.label();
    let provider_class = provider_value(session.provider);
    let model = model_label(&session.spec.model).to_string();
    let effort = session.spec.effort.label();
    let access = session.access;
    let status = session.status.label();
    let status_class = match &session.status {
        SessionStatus::Running => "badge status",
        SessionStatus::NeedsAnswer(_) => "badge status warn",
        SessionStatus::Finished => "badge status",
        SessionStatus::Failed { .. } => "badge status danger",
    };
    let cost = session.cost;
    let running = session.is_running();
    let failed = matches!(session.status, SessionStatus::Failed { .. });
    let archived = session.archived;
    let blocked = session.blocked;
    let blocked_note = session.blocked_note.clone();
    // Only point at the note when there is one to read.
    let blocked_title = if blocked_note.is_some() {
        "Out of the attention counts until you unblock it; the message below is your own note"
    } else {
        "Out of the attention counts until you unblock it"
    };
    let note_label = if blocked_note.is_some() {
        "Edit blocked message"
    } else {
        "Add blocked message"
    };
    let block_title = session.title.clone();
    let crowded = folder_in_use(&state, &session);

    rsx! {
        div { class: "detail session-detail",
            PanelResizer { panel: Panel::Detail }
            div { class: "detail-header",
                div { class: "detail-title-row",
                    // Keyed per session so switching sessions remounts the input
                    // (and drops a half-typed rename of the previous one).
                    for t in [session.title.clone()] {
                        input {
                            key: "{id}",
                            class: "detail-title-input",
                            initial_value: "{t}",
                            "aria-label": "Session title",
                            onchange: move |e| {
                                let title = e.value();
                                if !title.trim().is_empty() {
                                    state.send(ExecutorCommand::RenameSession { session_id: id, title });
                                }
                            },
                        }
                    }
                    button {
                        class: "detail-close",
                        "aria-label": "Close panel",
                        onclick: move |_| state.select_session(None),
                        "×"
                    }
                }
                div { class: "session-folder-row",
                    span { class: "session-folder-path", title: "{folder.display()}", "{folder_label}" }
                    button {
                        class: "btn subtle small",
                        title: "Reveal in Finder",
                        onclick: {
                            let folder = folder.clone();
                            move |_| reveal(&folder)
                        },
                        "Reveal"
                    }
                    button {
                        class: "btn subtle small",
                        title: "Copy the folder path",
                        onclick: {
                            let folder = folder.clone();
                            move |_| {
                                if let Ok(mut cb) = arboard::Clipboard::new() {
                                    let _ = cb.set_text(folder.display().to_string());
                                    crate::toast::push_toast(usine_core::Severity::Info, "Path copied");
                                }
                            }
                        },
                        "Copy path"
                    }
                }
                div { class: "card-meta",
                    if running {
                        span { class: "spinner" }
                    }
                    span { class: "badge provider {provider_class}", "{provider}" }
                    span { class: "badge", "{model} · {effort}" }
                    span {
                        class: if access == SessionAccess::ReadOnly { "badge" } else { "badge warn" },
                        title: if access == SessionAccess::ReadOnly { "The agent can only read this folder" } else { "The agent can edit files and run commands without asking" },
                        "{access.label()}"
                    }
                    span { class: "{status_class}", "{status}" }
                    if blocked {
                        span { class: "badge blocked", title: "{blocked_title}", "Blocked" }
                    }
                    if !cost.is_zero() {
                        span { class: "badge cost", "{cost}" }
                    }
                }
                if blocked {
                    if let Some(note) = blocked_note.clone() {
                        div { class: "blocked-note", "{note}" }
                    }
                }
                div { class: "row session-actions",
                    if running {
                        button {
                            class: "btn",
                            onclick: move |_| state.send(ExecutorCommand::CancelSession { session_id: id }),
                            "Stop"
                        }
                    }
                    if failed {
                        button {
                            class: "btn primary",
                            title: "Re-send your last message",
                            onclick: move |_| state.send(ExecutorCommand::RetrySession { session_id: id }),
                            "Retry"
                        }
                    }
                    if !archived {
                        button {
                            class: "btn subtle",
                            onclick: {
                                let block_title = block_title.clone();
                                move |_| {
                                    if blocked {
                                        state.send(ExecutorCommand::SetSessionBlocked { session_id: id, blocked: false, note: None });
                                    } else {
                                        request_block_session(id, &block_title);
                                    }
                                }
                            },
                            if blocked { "Mark unblocked" } else { "Mark blocked" }
                        }
                    }
                    // Unmarking drops the message, so this is the only way to
                    // fix it in place. Saving blank keeps the session blocked.
                    if blocked && !archived {
                        button {
                            class: "btn subtle",
                            onclick: {
                                let block_title = block_title.clone();
                                let blocked_note = blocked_note.clone();
                                move |_| {
                                    request_confirm_with_note(
                                        ConfirmRequest {
                                            title: note_label.into(),
                                            message: format!(
                                                "Message shown on “{block_title}” while it stays blocked. Leave it empty to drop the message.",
                                            ),
                                            confirm_label: "Save".into(),
                                            danger: false,
                                            action: ConfirmAction::BlockSession(id),
                                        },
                                        blocked_note.clone().unwrap_or_default(),
                                    );
                                }
                            },
                            "{note_label}"
                        }
                    }
                    if !running {
                        button {
                            class: "btn subtle",
                            onclick: move |_| state.send(ExecutorCommand::ArchiveSession { session_id: id, archived: !archived }),
                            if archived { "Unarchive" } else { "Archive" }
                        }
                    }
                }
                if crowded {
                    div { class: "hint warn", "Another agent is working in this folder." }
                }
            }
            // Keyed on the scroll container so another session starts at the top
            // with fresh per-session form state (see `CardDetail`).
            for k in [id] {
                div { key: "{k}", class: "detail-body",
                    Conversation { session: session.clone() }
                }
            }
            for k in [id] {
                div { key: "{k}", class: "session-footer",
                    if let SessionStatus::NeedsAnswer(iv) = &session.status {
                        div { class: "session-answer",
                            InterventionPanel {
                                card_id: id,
                                question: iv.question.clone(),
                                options: iv.options.clone(),
                                on_answer: move |text: String| state.answer_session(id, text),
                            }
                            AttachmentChips { card_id: id }
                            div { class: "row",
                                AttachButton { card_id: id, title: SESSION_ATTACH_TITLE.to_string() }
                            }
                        }
                    } else {
                        Composer {
                            session_id: id,
                            running,
                            steerable: running && session.provider == Provider::Claude,
                            has_queue: !session.queued.is_empty(),
                        }
                    }
                }
            }
        }
    }
}

/// Whether another agent works in this session's folder right now: a running
/// session, or a running card whose project or worktree is that folder.
fn folder_in_use(state: &AppState, session: &Session) -> bool {
    let same = |p: &Path| p == session.folder;
    let other_session = state
        .sessions
        .read()
        .values()
        .any(|s| s.id != session.id && s.is_running() && same(&s.folder));
    if other_session {
        return true;
    }
    let projects = state.projects.read();
    state.cards.read().iter().any(|c| {
        c.state.is_running()
            && (c.worktree_path.as_deref().is_some_and(same)
                || projects
                    .iter()
                    .any(|p| p.id == c.project_id && same(&p.path)))
    })
}

#[component]
fn Conversation(session: Session) -> Element {
    let folder = session.folder.clone();
    // Scroll to the newest turn as the conversation grows.
    let n = session.turns.len();
    let id = session.id;
    let state = use_context::<AppState>();
    // Lines are persisted but only streamed while a run is live: ask for the
    // stored ones once, so earlier work blocks fill in (see `TranscriptView`).
    use_hook(move || {
        if !state.transcripts_loaded.peek().contains(&id) {
            state.send(ExecutorCommand::LoadTranscript { card_id: id });
        }
    });
    let running = session.is_running();
    let bounds = section_bounds(&session.turns);
    let last_bound = bounds.len().checked_sub(1);
    use_effect(use_reactive!(|n| {
        let _ = n;
        dioxus::document::eval(&format!(
            "setTimeout(function(){{var el=document.getElementById('session-end-{id}');\
             if(el)el.scrollIntoView({{block:'end'}});}},0);"
        ));
    }));
    rsx! {
        div { class: "session-turns",
            for (i , turn) in session.turns.iter().enumerate() {
                {
                    let work_folder = folder.clone();
                    let folder = folder.clone();
                    let open_path = move |p: String| {
                        let path = folder.join(p.split(':').next().unwrap_or(&p));
                        if path.exists() {
                            reveal(&path);
                        }
                    };
                    match turn.kind {
                        SessionTurnKind::User | SessionTurnKind::Answer => {
                            let work = bounds.iter().position(|b| b.after_turn == i);
                            rsx! {
                                div { key: "{i}", class: "turn turn-user",
                                    if turn.kind == SessionTurnKind::Answer {
                                        div { class: "turn-label", "Your answer" }
                                    }
                                    if !turn.text.is_empty() {
                                        div { class: "turn-text", "{turn.text}" }
                                    }
                                    TurnAttachments { paths: turn.attachments.clone() }
                                }
                                if let Some(w) = work {
                                    WorkSection {
                                        key: "w{i}",
                                        id,
                                        folder: work_folder.clone(),
                                        start: bounds[w].start,
                                        end: bounds[w].end,
                                        live: running && Some(w) == last_bound,
                                        final_text: bounds[w].final_text.clone(),
                                    }
                                }
                            }
                        }
                        SessionTurnKind::Agent => rsx! {
                            div { key: "{i}", class: "turn turn-agent",
                                if turn.text.trim().is_empty() {
                                    div { class: "hint", "The agent finished without a reply." }
                                } else {
                                    ArtifactText { text: turn.text.clone(), on_path: open_path }
                                }
                            }
                        },
                        SessionTurnKind::Question => rsx! {
                            div { key: "{i}", class: "turn turn-question",
                                div { class: "turn-label", "The agent asks" }
                                ArtifactText { text: turn.text.clone(), on_path: open_path, bare: true }
                            }
                        },
                        SessionTurnKind::Error => rsx! {
                            div { key: "{i}", class: "turn turn-error", "{turn.text}" }
                        },
                    }
                }
            }
            if running {
                div { class: "turn-live",
                    div { class: "turn turn-working",
                        span { class: "spinner" }
                        if let Some(note) = &session.status_note {
                            span { class: "status-note", "{note.text}" }
                            span { class: "status-note-age", "· {ago(note.ts, usine_core::now_millis())}" }
                        } else {
                            span { "Working…" }
                        }
                    }
                }
            }
            if !session.queued.is_empty() {
                // A pending question counts as in flight: the queue follows the
                // answer's turn.
                QueuedTurns {
                    session_id: id,
                    turns: session.queued.clone(),
                    running: matches!(session.status, SessionStatus::Running | SessionStatus::NeedsAnswer(_)),
                }
            }
            div { id: "session-end-{id}" }
        }
    }
}

/// Messages sent while a turn ran, waiting to go out together as the next
/// turn; each can be taken back.
#[component]
fn QueuedTurns(session_id: Uuid, turns: Vec<SessionTurn>, running: bool) -> Element {
    let state = use_context::<AppState>();
    rsx! {
        div { class: "queued-turns",
            div { class: "turn-label",
                if running { "Queued — sent when this turn ends" } else { "Queued — will be sent with your next message" }
            }
            for turn in turns {
                {
                    let ts = turn.ts;
                    rsx! {
                        div { key: "{ts}", class: "turn turn-user queued",
                            div { class: "queued-row",
                                div { class: "queued-body",
                                    if !turn.text.is_empty() {
                                        div { class: "turn-text", "{turn.text}" }
                                    }
                                    TurnAttachments { paths: turn.attachments.clone() }
                                }
                                button {
                                    class: "chip-remove",
                                    title: "Remove from the queue",
                                    "aria-label": "Remove from the queue",
                                    onclick: move |_| state.send(ExecutorCommand::UnqueueSessionMessage { session_id, ts }),
                                    "×"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The files sent with one turn: images open a preview, other files open in
/// their default app, and any existing file can be revealed. Read-only — a
/// sent file can't be taken back.
#[component]
fn TurnAttachments(paths: Vec<PathBuf>) -> Element {
    let mut preview = use_signal(|| None::<PathBuf>);
    if paths.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "chips turn-attachments",
            for path in paths {
                {
                    let label = usine_core::infra::paths::attachment_label(&path);
                    let exists = path.exists();
                    let is_image = exists && image_mime(&path).is_some();
                    let to_preview = path.clone();
                    let to_reveal = path.clone();
                    let to_open = path.clone();
                    rsx! {
                        span { key: "{path.display()}", class: if exists { "chip" } else { "chip missing" },
                            if is_image {
                                span {
                                    class: "chip-label clickable",
                                    title: "Preview",
                                    onclick: move |_| preview.set(Some(to_preview.clone())),
                                    "{label}"
                                }
                            } else if exists {
                                span {
                                    class: "chip-label clickable",
                                    title: "Open",
                                    onclick: move |_| open_file(&to_open),
                                    "{label}"
                                }
                            } else {
                                span { class: "chip-label", title: "{path.display()}", "{label}" }
                            }
                            if exists {
                                button {
                                    class: "chip-remove",
                                    title: "Reveal in Finder",
                                    onclick: move |_| reveal(&to_reveal),
                                    "↗"
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(path) = preview() {
            AttachmentPreview { path, on_close: move |_| preview.set(None) }
        }
    }
}

/// Map dropped files to their paths and stage them under `id`. A platform
/// that hands no native paths makes this a no-op; paste and the picker still
/// work.
pub(crate) fn drop_files(state: AppState, id: Uuid, e: &DragEvent) {
    let paths: Vec<PathBuf> = e
        .files()
        .iter()
        .map(|f| f.path())
        .filter(|p| p.is_file())
        .collect();
    if !paths.is_empty() {
        state.attach_images(id, paths);
    }
}

/// The message box: text (draft-backed), pasted/picked/dropped files staged
/// as chips, ⌘↩ to send. While a turn runs it stays open: a text-only message
/// to a Claude run goes to the agent live ("Send now"); anything else is
/// queued for when the turn ends.
#[component]
fn Composer(session_id: Uuid, running: bool, steerable: bool, has_queue: bool) -> Element {
    let state = use_context::<AppState>();
    let mut text = drafts::use_draft(session_id, "session-message", String::new);
    // Uncontrolled field (see `chat.rs`); a send clears it via a remount.
    let mut generation = use_signal(|| 0u32);
    let mut dragging = use_signal(|| false);
    let has_files = !state.card_attachments(session_id).is_empty();
    let blank = text.read().trim().is_empty();
    let can_send = !blank || has_files || (!running && has_queue);
    let live = steerable && !has_files;
    let (label, tip, placeholder) = if live {
        (
            "Send now",
            "The agent reads it at its next step",
            "Message the agent — it reads this at its next step",
        )
    } else if running {
        (
            "Queue",
            "Sent when the agent finishes this turn",
            "Add a follow-up — sent when the agent finishes this turn",
        )
    } else {
        (
            "Send",
            "",
            "Reply, or ask for something else… (paste or drop files to attach)",
        )
    };
    let hint = crate::ui::shortcuts::submit_hint();
    let field_class = if dragging() {
        "field attach-field drop-active"
    } else {
        "field attach-field"
    };

    rsx! {
        div {
            class: "session-composer",
            "data-submit-scope": "1",
            ondragover: move |e| {
                e.prevent_default();
                dragging.set(true);
            },
            ondragleave: move |_| dragging.set(false),
            ondrop: move |e| {
                e.prevent_default();
                dragging.set(false);
                drop_files(state, session_id, &e);
            },
            AttachmentChips { card_id: session_id }
            div { class: "{field_class}",
                for g in [generation()] {
                    textarea {
                        key: "{g}",
                        id: "session-input",
                        placeholder: "{placeholder}",
                        initial_value: "{text.peek()}",
                        oninput: move |e| text.set(e.value()),
                        onpaste: move |_| attach_from_clipboard(state, session_id),
                    }
                }
                AttachButton { card_id: session_id, icon: true, title: SESSION_ATTACH_TITLE.to_string() }
            }
            div { class: "row",
                if running && has_files && steerable {
                    span { class: "hint", "Files can't reach a running agent — this will be queued." }
                }
                div { class: "spacer" }
                button {
                    class: "btn primary",
                    "data-submit": "1",
                    title: if tip.is_empty() { format!("Send ({hint})") } else { format!("{tip} ({hint})") },
                    disabled: !can_send,
                    onclick: move |_| {
                        if !can_send {
                            return;
                        }
                        let t = text.read().trim().to_string();
                        state.send_session_message(session_id, t);
                        drafts::clear(session_id, "session-message", text, String::new());
                        generation += 1;
                    },
                    "{label}"
                }
            }
        }
    }
}
