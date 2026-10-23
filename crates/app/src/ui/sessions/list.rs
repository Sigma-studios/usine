//! The Sessions view's main area: every session, grouped by what it wants from
//! the user.

use dioxus::prelude::*;
use usine_core::{ExecutorCommand, Session, SessionStatus, SessionTurnKind};

use super::{ago, home_dir, open_new_session, request_block_session, session_dot, shorten_home};
use crate::state::AppState;
use crate::ui::confirm::{request_confirm, ConfirmAction, ConfirmRequest};

/// Which list section a session sits in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Section {
    NeedsYou,
    Unread,
    Running,
    Earlier,
    Blocked,
    Archived,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::NeedsYou => "Needs you",
            Section::Unread => "Unread",
            Section::Running => "Running",
            Section::Earlier => "Earlier",
            Section::Blocked => "Blocked",
            Section::Archived => "Archived",
        }
    }
}

pub(crate) fn section_of(s: &Session) -> Section {
    if s.archived {
        Section::Archived
    } else if s.blocked {
        Section::Blocked
    } else if matches!(s.status, SessionStatus::NeedsAnswer(_)) {
        Section::NeedsYou
    } else if s.is_running() {
        Section::Running
    } else if s.unread {
        Section::Unread
    } else {
        Section::Earlier
    }
}

/// The sessions in display order: by section, newest activity first within
/// each.
pub(crate) fn grouped(sessions: Vec<Session>) -> Vec<(Section, Vec<Session>)> {
    let order = [
        Section::NeedsYou,
        Section::Unread,
        Section::Running,
        Section::Earlier,
        Section::Blocked,
        Section::Archived,
    ];
    order
        .into_iter()
        .filter_map(|sec| {
            let mut items: Vec<Session> = sessions
                .iter()
                .filter(|s| section_of(s) == sec)
                .cloned()
                .collect();
            items.sort_by_key(|s| std::cmp::Reverse(s.last_activity_at));
            (!items.is_empty()).then_some((sec, items))
        })
        .collect()
}

/// One line previewing a session: what a running agent last said it is doing
/// (and how many subagents work for it), that it is resuming cut-off agents,
/// that some were left cut off, else its latest turn.
fn preview(s: &Session) -> (String, usize) {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if s.is_running() {
        let agents = s.running_agents().len();
        let resuming = s.last_cutoff.as_ref().map_or(0, |c| c.descriptions.len());
        let last_notice = s
            .turns
            .last()
            .is_some_and(|t| t.kind == SessionTurnKind::Notice);
        if resuming > 0 && agents == 0 && last_notice {
            return (format!("↻ resuming {resuming} agent{}", plural(resuming)), 0);
        }
        let with_agents = |line: String| match agents {
            0 => line,
            n => format!("{line} · {n} agent{}", plural(n)),
        };
        match &s.status_note {
            Some(note) => return (with_agents(format!("⟳ {}", note.text)), 0),
            None if agents > 0 => {
                return (format!("⟳ {agents} agent{} working", plural(agents)), 0)
            }
            None => {}
        }
    } else {
        let cut = s.cut_off_agents().len();
        if cut > 0 && !matches!(s.status, SessionStatus::NeedsAnswer(_)) {
            return (format!("⚠ {cut} agent{} cut off", plural(cut)), 0);
        }
    }
    let Some(turn) = s.turns.last() else {
        return (String::new(), 0);
    };
    let prefix = match turn.kind {
        SessionTurnKind::User | SessionTurnKind::Answer => "You: ",
        SessionTurnKind::Question => "Asks: ",
        SessionTurnKind::Error => "⚠ ",
        SessionTurnKind::Agent | SessionTurnKind::Notice => "",
    };
    let line = turn
        .text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    (format!("{prefix}{line}"), turn.attachments.len())
}

#[component]
pub(crate) fn SessionList() -> Element {
    let state = use_context::<AppState>();
    let mut show_archived = use_signal(|| false);
    let sessions: Vec<Session> = state.sessions.read().values().cloned().collect();
    let archived_count = sessions.iter().filter(|s| s.archived).count();
    let empty = sessions.iter().all(|s| s.archived);
    let groups: Vec<(Section, Vec<Session>)> = grouped(sessions)
        .into_iter()
        .filter(|(sec, _)| *sec != Section::Archived || show_archived())
        .collect();
    let selected = *state.selected_session.read();
    let now = usine_core::now_millis();
    let home = home_dir();

    rsx! {
        div { class: "sessions-area",
            div { class: "sessions-header",
                h2 { "Sessions" }
                span { class: "hint", "One-off agent chats in any folder. You're notified when one finishes or asks." }
                div { class: "spacer" }
                if archived_count > 0 {
                    button {
                        class: "btn subtle",
                        onclick: move |_| show_archived.toggle(),
                        if show_archived() { "Hide archived" } else { "Archived ({archived_count})" }
                    }
                }
                button {
                    class: "btn primary",
                    title: "New session (⌘⇧N)",
                    onclick: move |_| open_new_session(),
                    "New session"
                }
            }
            if empty && !show_archived() {
                div { class: "sessions-empty",
                    p { "No sessions yet." }
                    p { class: "hint",
                        "Start Claude or Codex in any folder — no project or git needed. Usine tells you when it's done."
                    }
                    button { class: "btn primary", onclick: move |_| open_new_session(), "Start a session" }
                }
            }
            div { class: "sessions-list",
                for (sec , items) in groups {
                    div { key: "{sec:?}", class: "sessions-section",
                        div { class: "sessions-section-title", "{sec.title()}" }
                        for s in items {
                            SessionRow {
                                key: "{s.id}",
                                session: s.clone(),
                                active: selected == Some(s.id),
                                when: ago(s.last_activity_at, now),
                                folder: shorten_home(&s.folder, home.as_deref()),
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SessionRow(session: Session, active: bool, when: String, folder: String) -> Element {
    let state = use_context::<AppState>();
    let id = session.id;
    let (line, n_files) = preview(&session);
    let dot = session_dot(&session);
    let mut class = String::from("session-row");
    if active {
        class.push_str(" active");
    }
    if session.unread && !session.archived && !session.blocked {
        class.push_str(" unread");
    }
    // Tinted like a blocked board card. Archived wins, as in `session_dot`.
    if session.blocked && !session.archived {
        class.push_str(" blocked");
    }
    let model = crate::ui::widgets::model_label(&session.spec.model).to_string();
    let provider = session.provider.label();
    let archived = session.archived;
    let blocked = session.blocked;
    let running = session.is_running();
    let title = session.title.clone();
    let block_title = session.title.clone();
    let blocked_note = session
        .blocked_note
        .clone()
        .filter(|_| blocked && !archived);

    rsx! {
        div {
            class: "{class}",
            onclick: move |_| state.select_session(Some(id)),
            span { class: "{dot}" }
            div { class: "session-row-main",
                div { class: "session-row-top",
                    span { class: "session-title", "{session.title}" }
                    span { class: "session-when", "{when}" }
                }
                div { class: "session-row-meta",
                    span { class: "session-folder", title: "{session.folder.display()}", "{folder}" }
                    span { "· {provider} · {model}" }
                }
                if !line.is_empty() || n_files > 0 {
                    div { class: "session-preview",
                        if n_files > 0 {
                            span { class: "session-files", "📎 {n_files} " }
                        }
                        "{line}"
                    }
                }
                if let Some(note) = blocked_note {
                    div { class: "blocked-note", title: "{note}", "{note}" }
                }
            }
            div { class: "session-row-actions",
                if !archived {
                    button {
                        class: if blocked { "proj-action unblock" } else { "proj-action" },
                        title: if blocked { "Mark unblocked" } else { "Mark blocked" },
                        "aria-label": if blocked { "Mark unblocked" } else { "Mark blocked" },
                        onclick: move |e| {
                            e.stop_propagation();
                            if blocked {
                                state.send(ExecutorCommand::SetSessionBlocked { session_id: id, blocked: false, note: None });
                            } else {
                                request_block_session(id, &block_title);
                            }
                        },
                        svg {
                            width: "13",
                            height: "13",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            rect { x: "3", y: "11", width: "18", height: "11", rx: "2" }
                            // Open shackle to lift the marker, closed to set it.
                            path { d: if blocked { "M7 11V7a5 5 0 0 1 9.9-1" } else { "M7 11V7a5 5 0 0 1 10 0v4" } }
                        }
                    }
                }
                button {
                    class: "proj-action",
                    title: if archived { "Unarchive" } else { "Archive" },
                    "aria-label": if archived { "Unarchive" } else { "Archive" },
                    disabled: running,
                    onclick: move |e| {
                        e.stop_propagation();
                        state.send(ExecutorCommand::ArchiveSession { session_id: id, archived: !archived });
                    },
                    svg {
                        width: "13",
                        height: "13",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        polyline { points: "21 8 21 21 3 21 3 8" }
                        rect { x: "1", y: "3", width: "22", height: "5" }
                        line { x1: "10", y1: "12", x2: "14", y2: "12" }
                    }
                }
                button {
                    class: "proj-trash",
                    title: "Delete session",
                    "aria-label": "Delete session",
                    onclick: move |e| {
                        e.stop_propagation();
                        request_confirm(ConfirmRequest {
                            title: "Delete session".into(),
                            message: format!(
                                "Delete “{title}” and its conversation from Usine? The folder and its files are not touched."
                            ),
                            confirm_label: "Delete".into(),
                            danger: true,
                            action: ConfirmAction::Send(ExecutorCommand::DeleteSession { session_id: id }),
                        });
                    },
                    svg {
                        width: "13",
                        height: "13",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        polyline { points: "3 6 5 6 21 6" }
                        path { d: "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use usine_core::{Effort, Intervention, ModelSpec, Provider, SessionAccess};
    use uuid::Uuid;

    fn session(status: SessionStatus, unread: bool, archived: bool, at: i64) -> Session {
        let mut s = Session::new(
            Uuid::new_v4(),
            PathBuf::from("/tmp"),
            Provider::Claude,
            ModelSpec::new("opus", Effort::High),
            SessionAccess::FullAccess,
            "x",
            vec![],
        );
        s.status = status;
        s.unread = unread;
        s.archived = archived;
        s.last_activity_at = at;
        s
    }

    #[test]
    fn sections_follow_what_the_session_wants() {
        let q = SessionStatus::NeedsAnswer(Intervention {
            request_id: "r".into(),
            question: "q".into(),
            options: vec![],
        });
        assert_eq!(
            section_of(&session(q.clone(), true, false, 0)),
            Section::NeedsYou
        );
        assert_eq!(section_of(&session(q, true, true, 0)), Section::Archived);
        assert_eq!(
            section_of(&session(SessionStatus::Running, false, false, 0)),
            Section::Running
        );
        assert_eq!(
            section_of(&session(SessionStatus::Finished, true, false, 0)),
            Section::Unread
        );
        assert_eq!(
            section_of(&session(SessionStatus::Finished, false, false, 0)),
            Section::Earlier
        );
    }

    #[test]
    fn blocked_sessions_get_their_own_section_below_earlier() {
        let blocked = |status, unread, archived| {
            let mut s = session(status, unread, archived, 0);
            s.blocked = true;
            s
        };
        let q = SessionStatus::NeedsAnswer(Intervention {
            request_id: "r".into(),
            question: "q".into(),
            options: vec![],
        });
        assert_eq!(
            section_of(&blocked(SessionStatus::Finished, false, false)),
            Section::Blocked
        );
        assert_eq!(
            section_of(&blocked(SessionStatus::Finished, true, false)),
            Section::Blocked
        );
        assert_eq!(
            section_of(&blocked(SessionStatus::Running, false, false)),
            Section::Blocked
        );
        assert_eq!(section_of(&blocked(q, true, false)), Section::Blocked);
        assert_eq!(
            section_of(&blocked(SessionStatus::Finished, false, true)),
            Section::Archived
        );

        let earlier = session(SessionStatus::Finished, false, false, 0);
        let b = blocked(SessionStatus::Finished, false, false);
        let mut a = session(SessionStatus::Finished, false, true, 0);
        a.blocked = true;
        let order: Vec<Section> = grouped(vec![a, b, earlier])
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert_eq!(
            order,
            [Section::Earlier, Section::Blocked, Section::Archived]
        );
    }

    #[test]
    fn a_running_session_previews_its_status_note() {
        let mut s = session(SessionStatus::Running, false, false, 0);
        assert_eq!(preview(&s).0, "You: x");
        s.status_note = Some(usine_core::StatusNote {
            text: "Running tests".into(),
            ts: 0,
        });
        assert_eq!(preview(&s).0, "⟳ Running tests");
        s.status = SessionStatus::Finished;
        assert_eq!(preview(&s).0, "You: x");
    }

    fn agent(id: &str, desc: &str) -> usine_core::TaskEvent {
        usine_core::TaskEvent::Started {
            id: id.into(),
            parent: None,
            kind: usine_core::BgTaskKind::Agent,
            description: desc.into(),
            subagent_type: None,
            backgrounded: true,
        }
    }

    #[test]
    fn previews_count_running_resuming_and_cut_off_agents() {
        let mut s = session(SessionStatus::Running, false, false, 0);
        s.apply_task(&agent("a", "Build"));
        s.apply_task(&agent("b", "Test"));
        assert_eq!(preview(&s).0, "⟳ 2 agents working");
        s.status_note = Some(usine_core::StatusNote {
            text: "Waiting on agents".into(),
            ts: 0,
        });
        assert_eq!(preview(&s).0, "⟳ Waiting on agents · 2 agents");

        // The run ended cutting both off, and the session resumes them.
        s.apply_task(&usine_core::TaskEvent::CutOff {
            ids: vec!["a".into(), "b".into()],
        });
        let lost: Vec<usine_core::BgTask> = s.cut_off_agents().into_iter().cloned().collect();
        s.record_cutoff(&lost);
        s.push_turn(usine_core::SessionTurn::new(SessionTurnKind::Notice, "↻ 2 cut off"));
        assert_eq!(preview(&s).0, "↻ resuming 2 agents");

        // Paused by the guard: the session settles with them cut off.
        s.status = SessionStatus::Finished;
        assert_eq!(preview(&s).0, "⚠ 2 agents cut off");
    }

    #[test]
    fn groups_are_ordered_and_newest_first() {
        let old = session(SessionStatus::Finished, false, false, 1);
        let new = session(SessionStatus::Finished, false, false, 2);
        let unread = session(SessionStatus::Finished, true, false, 0);
        let groups = grouped(vec![old.clone(), unread.clone(), new.clone()]);
        assert_eq!(groups[0].0, Section::Unread);
        assert_eq!(groups[1].0, Section::Earlier);
        assert_eq!(groups[1].1[0].id, new.id);
        assert_eq!(groups[1].1[1].id, old.id);
    }
}
