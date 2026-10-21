//! The detail panel of one session: its conversation, the answer panel when
//! the agent asked something, and the composer for the next message.

use std::path::{Path, PathBuf};

use dioxus::html::HasFileData;
use dioxus::prelude::*;
use usine_core::{
    ExecutorCommand, Provider, Session, SessionAccess, SessionStatus, SessionTurn, SessionTurnKind,
};
use uuid::Uuid;

use super::{ago, home_dir, open_file, reveal, shorten_home};
use crate::state::AppState;
use crate::ui::detail::{
    attach_from_clipboard, image_mime, AttachButton, AttachmentChips, AttachmentPreview,
    InterventionPanel, TranscriptView,
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
                        span { class: "badge blocked", title: "Out of the attention counts until you unblock it", "Blocked" }
                    }
                    if !cost.is_zero() {
                        span { class: "badge cost", "{cost}" }
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
                            onclick: move |_| state.send(ExecutorCommand::SetSessionBlocked { session_id: id, blocked: !blocked }),
                            if blocked { "Mark unblocked" } else { "Mark blocked" }
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
                    ActivitySection { id }
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
                    let folder = folder.clone();
                    let open_path = move |p: String| {
                        let path = folder.join(p.split(':').next().unwrap_or(&p));
                        if path.exists() {
                            reveal(&path);
                        }
                    };
                    match turn.kind {
                        SessionTurnKind::User | SessionTurnKind::Answer => rsx! {
                            div { key: "{i}", class: "turn turn-user",
                                if turn.kind == SessionTurnKind::Answer {
                                    div { class: "turn-label", "Your answer" }
                                }
                                if !turn.text.is_empty() {
                                    div { class: "turn-text", "{turn.text}" }
                                }
                                TurnAttachments { paths: turn.attachments.clone() }
                            }
                        },
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
                                div { class: "turn-text", "{turn.text}" }
                            }
                        },
                        SessionTurnKind::Error => rsx! {
                            div { key: "{i}", class: "turn turn-error", "{turn.text}" }
                        },
                    }
                }
            }
            if session.is_running() {
                div { class: "turn-live",
                    LiveActivity { id, folder: folder.clone() }
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

/// How many activity lines the running turn shows above its status line.
const TAIL_LINES: usize = 5;

/// The running turn's last few activity lines — tool calls and interim prose —
/// so the conversation reads live without opening "Activity". Reads the
/// transcript itself so a new line re-renders only this, not the conversation.
#[component]
fn LiveActivity(id: Uuid, folder: PathBuf) -> Element {
    let state = use_context::<AppState>();
    let items = {
        let map = state.transcripts.read();
        let all = map.get(&id).map(Vec::as_slice).unwrap_or_default();
        activity_tail(all, &folder, TAIL_LINES)
    };
    let newest = items.last().map(|it| it.idx);
    // Follow new lines only when the reader is already at the bottom: someone
    // scrolled up into earlier turns stays where they are.
    use_effect(use_reactive!(|newest| {
        if newest.is_none() {
            return;
        }
        dioxus::document::eval(&format!(
            "setTimeout(function(){{var end=document.getElementById('session-end-{id}');if(!end)return;\
             var body=end.closest('.detail-body');if(!body)return;\
             var gap=end.getBoundingClientRect().top-body.getBoundingClientRect().bottom;\
             if(gap<120)end.scrollIntoView({{block:'end'}});}},0);"
        ));
    }));
    if items.is_empty() {
        return rsx! {};
    }
    let last = items.len() - 1;
    rsx! {
        div { class: "activity-tail",
            for (i , item) in items.iter().enumerate() {
                div {
                    key: "{item.idx}",
                    class: if i == last { "{item.kind.class()}" } else { "{item.kind.class()} older" },
                    title: "{item.text}",
                    "{item.text}"
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailKind {
    /// A tool call (`→ …`), arrow dropped.
    Tool,
    /// The agent's interim prose.
    Text,
    /// A run event worth seeing (`↪ picked up: …`, `⚠ …`).
    Note,
}

impl TailKind {
    fn class(self) -> &'static str {
        match self {
            TailKind::Tool => "tool",
            TailKind::Text => "text",
            TailKind::Note => "note",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TailItem {
    /// The line's index in the transcript, a stable key as the window slides.
    idx: usize,
    kind: TailKind,
    text: String,
}

/// The last `n` showable lines of the current run, oldest first. The run
/// starts at the last `▶` line; bookkeeping lines are skipped, prose loses its
/// `usine-status:` lines (they are the status note) and keeps its first line,
/// and tool lines show paths relative to `folder`.
fn activity_tail(lines: &[(i64, String)], folder: &Path, n: usize) -> Vec<TailItem> {
    let prefix = format!("{}/", folder.display());
    let mut out = Vec::new();
    for (idx, (_, line)) in lines.iter().enumerate().rev() {
        if out.len() == n || line.starts_with("▶ ") {
            break;
        }
        if line.starts_with("● session")
            || line.starts_with('⏸')
            || line.starts_with("↪ sent to the running agent")
        {
            continue;
        }
        let item = if let Some(tool) = line.strip_prefix("→ ") {
            let tool = tool.lines().next().unwrap_or("").trim();
            let tool = if prefix.len() > 1 {
                tool.replace(&prefix, "")
            } else {
                tool.to_string()
            };
            (TailKind::Tool, tool)
        } else if ["↪", "⚠", "✔", "✖", "■"]
            .iter()
            .any(|m| line.starts_with(m))
        {
            (TailKind::Note, line.trim().to_string())
        } else {
            let prose = usine_core::strip_status_lines(line);
            let first = prose
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("");
            // Codex reasoning summaries open with a `**Title**` (or a
            // heading); show the words, not the markdown markers.
            let first = first
                .trim_start_matches('#')
                .trim()
                .trim_matches('*')
                .trim();
            (TailKind::Text, usine_core::truncate_chars(first, 200))
        };
        if item.1.is_empty() {
            continue;
        }
        out.push(TailItem {
            idx,
            kind: item.0,
            text: item.1,
        });
    }
    out.reverse();
    out
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

#[component]
fn ActivitySection(id: Uuid) -> Element {
    let mut open = use_signal(|| false);
    rsx! {
        div { class: "section session-activity",
            button {
                class: "btn link",
                onclick: move |_| open.toggle(),
                if open() { "▾ Activity" } else { "▸ Activity" }
            }
            if open() {
                TranscriptView { id }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(raw: &[&str]) -> Vec<(i64, String)> {
        raw.iter().map(|l| (0, l.to_string())).collect()
    }

    fn texts(items: &[TailItem]) -> Vec<(TailKind, &str)> {
        items.iter().map(|i| (i.kind, i.text.as_str())).collect()
    }

    #[test]
    fn tail_covers_only_the_current_run() {
        let all = lines(&[
            "▶ starting",
            "→ Read old.rs",
            "✔ finished",
            "▶ resuming the conversation",
            "→ Bash cargo check",
        ]);
        let tail = activity_tail(&all, Path::new("/w"), 5);
        assert_eq!(texts(&tail), vec![(TailKind::Tool, "Bash cargo check")]);
        assert_eq!(tail[0].idx, 4);
    }

    #[test]
    fn tail_skips_bookkeeping_and_status_only_prose() {
        let all = lines(&[
            "▶ starting",
            "● session abc",
            "⏸ message queued for when this turn ends",
            "↪ sent to the running agent",
            "↪ picked up: also this",
            "usine-status: checking the build",
            "Looking at the parser.\nusine-status: reading",
        ]);
        assert_eq!(
            texts(&activity_tail(&all, Path::new("/w"), 5)),
            vec![
                (TailKind::Note, "↪ picked up: also this"),
                (TailKind::Text, "Looking at the parser."),
            ]
        );
    }

    #[test]
    fn tail_cuts_long_prose_to_its_first_line() {
        let long = format!("\n{}\nsecond line", "x".repeat(300));
        let tail = activity_tail(&lines(&[&long]), Path::new("/w"), 5);
        assert_eq!(tail[0].text.chars().count(), 200);
        assert!(tail[0].text.ends_with('…'));
    }

    #[test]
    fn tail_drops_markdown_markers_from_titles() {
        let all = lines(&["**Inspecting the parser**\n\nThe tokenizer…", "## Plan"]);
        assert_eq!(
            texts(&activity_tail(&all, Path::new("/w"), 5)),
            vec![
                (TailKind::Text, "Inspecting the parser"),
                (TailKind::Text, "Plan"),
            ]
        );
    }

    #[test]
    fn tail_shows_paths_relative_to_the_folder() {
        let all = lines(&[
            "→ Read /w/proj/src/main.rs",
            "→ Bash cd /w/proj && ls /w/projx",
        ]);
        assert_eq!(
            texts(&activity_tail(&all, Path::new("/w/proj"), 5)),
            vec![
                (TailKind::Tool, "Read src/main.rs"),
                (TailKind::Tool, "Bash cd /w/proj && ls /w/projx"),
            ]
        );
    }

    #[test]
    fn tail_keeps_the_newest_n() {
        let all = lines(&["▶ starting", "→ A", "→ B", "→ C", "→ D"]);
        assert_eq!(
            texts(&activity_tail(&all, Path::new("/w"), 2)),
            vec![(TailKind::Tool, "C"), (TailKind::Tool, "D")]
        );
    }
}
