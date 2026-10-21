//! The agent's work between two of your messages — its interim prose, its
//! thinking and its tool calls — as one collapsible block under the message
//! that started it. Open while the agent works; once it is done it folds to a
//! header and the agent's last interim remark.

use std::path::{Path, PathBuf};

use dioxus::prelude::*;
use usine_core::{SessionTurn, SessionTurnKind, THINKING_PROGRESS_PREFIX};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::widgets::ArtifactText;

/// How many items an open block shows before "… N earlier".
const WORK_CAP: usize = 200;

/// One block's place in the conversation and in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bound {
    /// The index of your message (or answer) the block follows.
    pub after_turn: usize,
    /// Transcript lines from this stamp on belong to the block…
    pub start: i64,
    /// …up to, not including, this one (the next block's start).
    pub end: Option<i64>,
    /// The agent's reply that closed the block, if any: the transcript logs
    /// it as prose too, and it already shows as its own bubble.
    pub final_text: Option<String>,
}

/// Where each block starts and ends. Every message or answer of yours opens
/// one, starting at the later of its own stamp and every earlier turn's: a
/// queued message keeps its queue-time stamp but is only sent after the reply
/// it waited behind, and a batch's earlier messages get empty blocks that are
/// dropped. A retry adds no message, so it stays in the failed turn's block.
/// A reply's own millisecond stays with the block it closes: the run's last
/// prose is logged in the same stdout burst, so a batch waiting behind that
/// reply starts one millisecond after it.
pub(super) fn section_bounds(turns: &[SessionTurn]) -> Vec<Bound> {
    let mut latest = i64::MIN;
    let mut out: Vec<Bound> = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        let after = match turn.kind {
            SessionTurnKind::Agent => turn.ts.saturating_add(1),
            _ => turn.ts,
        };
        latest = latest.max(after);
        match turn.kind {
            SessionTurnKind::User | SessionTurnKind::Answer => {
                if let Some(prev) = out.last_mut() {
                    prev.end = Some(latest);
                }
                out.push(Bound {
                    after_turn: i,
                    start: latest,
                    end: None,
                    final_text: None,
                });
            }
            SessionTurnKind::Agent => {
                if let Some(prev) = out.last_mut() {
                    prev.final_text = Some(turn.text.clone());
                }
            }
            SessionTurnKind::Question | SessionTurnKind::Error => {}
        }
    }
    out.retain(|b| b.end.is_none_or(|end| end > b.start));
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WorkKind {
    /// A tool call (`→ …`), arrow dropped.
    Tool,
    /// The agent's interim prose.
    Text,
    /// Claude's thinking, prefix dropped.
    Thinking,
    /// A run event worth seeing (`↪ picked up: …`, `⚠ …`).
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkItem {
    /// The line's index in the transcript, a stable key.
    pub idx: usize,
    pub kind: WorkKind,
    pub text: String,
}

/// What one transcript line shows as, if anything. Bookkeeping lines are
/// skipped, prose loses its `usine-status:` lines (they are the status note),
/// and tool lines keep their first line with paths relative to the folder,
/// whose `prefix` is `"<folder>/"`.
pub(super) fn classify(line: &str, prefix: &str) -> Option<(WorkKind, String)> {
    if line.starts_with("▶ ")
        || line.starts_with("● session")
        || line.starts_with('⏸')
        || line.starts_with("↪ sent to the running agent")
    {
        return None;
    }
    let (kind, text) = if let Some(tool) = line.strip_prefix("→ ") {
        let tool = tool.lines().next().unwrap_or("").trim();
        let tool = if prefix.len() > 1 {
            tool.replace(prefix, "")
        } else {
            tool.to_string()
        };
        (WorkKind::Tool, tool)
    } else if let Some(thought) = line.strip_prefix(THINKING_PROGRESS_PREFIX) {
        (WorkKind::Thinking, thought.trim().to_string())
    } else if ["↪", "⚠", "✔", "✖", "■"]
        .iter()
        .any(|m| line.starts_with(m))
    {
        (WorkKind::Note, line.trim().to_string())
    } else {
        (
            WorkKind::Text,
            usine_core::strip_status_lines(line).trim().to_string(),
        )
    };
    (!text.is_empty()).then_some((kind, text))
}

/// A block's items from its slice of the transcript (`first` is the slice's
/// index in the whole), without the closing prose that repeats `final_text`.
pub(super) fn work_items(
    lines: &[(i64, String)],
    first: usize,
    folder: &Path,
    final_text: Option<&str>,
) -> Vec<WorkItem> {
    let prefix = format!("{}/", folder.display());
    let mut out: Vec<WorkItem> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, (_, line))| {
            classify(line, &prefix).map(|(kind, text)| WorkItem {
                idx: first + i,
                kind,
                text,
            })
        })
        .collect();
    if let Some(reply) = final_text.map(|t| usine_core::strip_status_lines(t).trim().to_string()) {
        if let Some(pos) = out.iter().rposition(|it| it.kind == WorkKind::Text) {
            if out[pos].text == reply {
                out.remove(pos);
            }
        }
    }
    out
}

/// What a folded block still shows: the agent's last interim remark — never
/// its thinking or a tool call.
pub(super) fn preview(items: &[WorkItem]) -> Option<&str> {
    items
        .iter()
        .rev()
        .find(|it| it.kind == WorkKind::Text)
        .map(|it| it.text.as_str())
}

/// The block's summary: how long it ran and how many tools it called.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Work {
    items: Vec<WorkItem>,
    /// Stamps of the block's first and last line.
    span: Option<(i64, i64)>,
}

#[component]
pub(super) fn WorkSection(
    id: Uuid,
    folder: PathBuf,
    start: i64,
    end: Option<i64>,
    live: bool,
    final_text: Option<String>,
) -> Element {
    let state = use_context::<AppState>();
    // The block's slice of the transcript. Every streamed line, from any
    // run, wakes this cheap memo; lines only ever get added, so an unchanged
    // slice means unchanged items and the classifying memo below stays put.
    let range = use_memo(use_reactive!(|start, end| {
        let map = state.transcripts.read();
        let all = map.get(&id).map(Vec::as_slice).unwrap_or_default();
        let lo = all.partition_point(|(ts, _)| *ts < start);
        let hi = end.map_or(all.len(), |end| all.partition_point(|(ts, _)| *ts < end));
        (lo, hi.max(lo))
    }));
    let work = use_memo(use_reactive!(|folder, final_text| {
        let (lo, hi) = range();
        let map = state.transcripts.peek();
        let all = map.get(&id).map(Vec::as_slice).unwrap_or_default();
        let slice = all.get(lo..hi).unwrap_or_default();
        Work {
            items: work_items(slice, lo, &folder, final_text.as_deref()),
            span: slice
                .first()
                .zip(slice.last())
                .map(|((a, _), (b, _))| (*a, *b)),
        }
    }));
    let mut manual = use_signal(|| None::<bool>);
    let mut show_all = use_signal(|| false);
    let open = manual().unwrap_or(live);

    let work = work.read();
    let newest = work.items.last().map(|it| it.idx);
    // While the agent works, follow new lines — only when the reader is
    // already at the bottom: someone scrolled up stays where they are.
    use_effect(use_reactive!(|newest, live| {
        if newest.is_none() || !live {
            return;
        }
        dioxus::document::eval(&format!(
            "setTimeout(function(){{var end=document.getElementById('session-end-{id}');if(!end)return;\
             var body=end.closest('.detail-body');if(!body)return;\
             var gap=end.getBoundingClientRect().top-body.getBoundingClientRect().bottom;\
             if(gap<120)end.scrollIntoView({{block:'end'}});}},0);"
        ));
    }));
    if work.items.is_empty() {
        return rsx! {};
    }

    let tools = work
        .items
        .iter()
        .filter(|it| it.kind == WorkKind::Tool)
        .count();
    // A live block's duration would need a ticking clock; it shows once done.
    let mut header = match work.span {
        _ if live => "Working".to_string(),
        Some((a, b)) => format!("Worked {}", fmt_duration(b - a)),
        None => "Worked".to_string(),
    };
    if tools > 0 {
        header.push_str(&format!(
            " · {tools} tool call{}",
            if tools == 1 { "" } else { "s" }
        ));
    }
    let preview = preview(&work.items).map(str::to_string);
    let hidden = if show_all() {
        0
    } else {
        work.items.len().saturating_sub(WORK_CAP)
    };
    let open_path = {
        let folder = folder.clone();
        move |p: String| {
            let path = folder.join(p.split(':').next().unwrap_or(&p));
            if path.exists() {
                super::reveal(&path);
            }
        }
    };

    rsx! {
        div { class: if open { "work-section open" } else { "work-section" },
            button {
                class: "work-header",
                onclick: move |_| manual.set(Some(!open)),
                span { class: "work-caret", if open { "▾" } else { "▸" } }
                span { "{header}" }
            }
            if open {
                div { class: "work-items",
                    if hidden > 0 {
                        button {
                            class: "btn link work-earlier",
                            onclick: move |_| show_all.set(true),
                            "… {hidden} earlier"
                        }
                    }
                    for item in work.items[hidden..].iter() {
                        match item.kind {
                            WorkKind::Text => rsx! {
                                div { key: "{item.idx}", class: "work-text",
                                    ArtifactText { text: item.text.clone(), on_path: open_path.clone() }
                                }
                            },
                            WorkKind::Thinking => rsx! {
                                div { key: "{item.idx}", class: "work-thinking", "{item.text}" }
                            },
                            WorkKind::Tool => rsx! {
                                div { key: "{item.idx}", class: "work-tool", title: "{item.text}", "{item.text}" }
                            },
                            WorkKind::Note => rsx! {
                                div { key: "{item.idx}", class: "work-note", "{item.text}" }
                            },
                        }
                    }
                }
            } else if let Some(text) = preview {
                div { class: "work-preview",
                    ArtifactText { text, on_path: open_path }
                }
            }
        }
    }
}

/// `45s`, `3m`, `1h 05m`.
fn fmt_duration(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(ts: i64, kind: SessionTurnKind, text: &str) -> SessionTurn {
        let mut t = SessionTurn::new(kind, text);
        t.ts = ts;
        t
    }

    fn spans(bounds: &[Bound]) -> Vec<(usize, i64, Option<i64>, Option<&str>)> {
        bounds
            .iter()
            .map(|b| (b.after_turn, b.start, b.end, b.final_text.as_deref()))
            .collect()
    }

    fn lines(raw: &[&str]) -> Vec<(i64, String)> {
        raw.iter().map(|l| (0, l.to_string())).collect()
    }

    fn texts(items: &[WorkItem]) -> Vec<(WorkKind, &str)> {
        items.iter().map(|i| (i.kind, i.text.as_str())).collect()
    }

    use SessionTurnKind::*;

    #[test]
    fn a_plain_run_is_one_block_per_message() {
        let turns = [
            turn(10, User, "fix it"),
            turn(50, Agent, "done"),
            turn(60, User, "thanks, now docs"),
            turn(90, Agent, "docs too"),
        ];
        assert_eq!(
            spans(&section_bounds(&turns)),
            vec![
                (0, 10, Some(60), Some("done")),
                (2, 60, None, Some("docs too")),
            ]
        );
    }

    #[test]
    fn a_queued_batch_starts_after_the_reply_it_waited_for() {
        // Both queued at 20 and 30, pushed after the reply at 50.
        let turns = [
            turn(10, User, "first"),
            turn(50, Agent, "first done"),
            turn(20, User, "queued a"),
            turn(30, User, "queued b"),
            turn(80, Agent, "batch done"),
        ];
        assert_eq!(
            spans(&section_bounds(&turns)),
            vec![
                (0, 10, Some(51), Some("first done")),
                (3, 51, None, Some("batch done")),
            ]
        );
    }

    #[test]
    fn a_reply_logged_in_the_batch_millisecond_stays_in_its_block() {
        let turns = [
            turn(10, User, "first"),
            turn(50, Agent, "first done"),
            turn(20, User, "queued"),
            turn(80, Agent, "batch done"),
        ];
        let bounds = section_bounds(&turns);
        // The reply's prose shares the Agent turn's millisecond.
        let all = vec![
            (49, "→ Bash ls".to_string()),
            (50, "first done".to_string()),
        ];
        let end = bounds[0].end.unwrap();
        let hi = all.partition_point(|(ts, _)| *ts < end);
        assert_eq!(hi, 2);
        assert!(
            work_items(&all[..hi], 0, Path::new("/w"), Some("first done"))
                .iter()
                .all(|it| it.kind != WorkKind::Text)
        );
    }

    #[test]
    fn a_message_sent_mid_run_splits_the_block() {
        let turns = [
            turn(10, User, "task"),
            turn(30, User, "also this"),
            turn(70, Agent, "both done"),
        ];
        assert_eq!(
            spans(&section_bounds(&turns)),
            vec![(0, 10, Some(30), None), (1, 30, None, Some("both done"))]
        );
    }

    #[test]
    fn an_answer_opens_a_block_and_a_retry_does_not() {
        let turns = [
            turn(10, User, "task"),
            turn(20, Question, "which one?"),
            turn(30, Answer, "the second"),
            turn(40, Error, "boom"),
            turn(90, Agent, "done after retry"),
        ];
        assert_eq!(
            spans(&section_bounds(&turns)),
            vec![
                (0, 10, Some(30), None),
                (2, 30, None, Some("done after retry"))
            ]
        );
    }

    #[test]
    fn classify_skips_bookkeeping_and_status_only_prose() {
        let all = lines(&[
            "▶ starting",
            "● session abc",
            "⏸ message queued for when this turn ends",
            "↪ sent to the running agent",
            "↪ picked up: also this",
            "usine-status: checking the build",
            "Looking at the parser.\nusine-status: reading",
            "∴ The tokenizer is fine; the bug is in the lexer.",
            "→ Read /w/src/main.rs",
        ]);
        assert_eq!(
            texts(&work_items(&all, 0, Path::new("/w"), None)),
            vec![
                (WorkKind::Note, "↪ picked up: also this"),
                (WorkKind::Text, "Looking at the parser."),
                (
                    WorkKind::Thinking,
                    "The tokenizer is fine; the bug is in the lexer."
                ),
                (WorkKind::Tool, "Read src/main.rs"),
            ]
        );
    }

    #[test]
    fn prose_keeps_every_line_and_codex_titles() {
        let all = lines(&["**Inspecting the parser**\n\nThe tokenizer…\nThen the lexer."]);
        assert_eq!(
            texts(&work_items(&all, 0, Path::new("/w"), None)),
            vec![(
                WorkKind::Text,
                "**Inspecting the parser**\n\nThe tokenizer…\nThen the lexer."
            )]
        );
    }

    #[test]
    fn tool_paths_are_relative_to_the_folder_only() {
        let all = lines(&[
            "→ Read /w/proj/src/main.rs",
            "→ Bash cd /w/proj && ls /w/projx\nsecond line",
        ]);
        assert_eq!(
            texts(&work_items(&all, 0, Path::new("/w/proj"), None)),
            vec![
                (WorkKind::Tool, "Read src/main.rs"),
                (WorkKind::Tool, "Bash cd /w/proj && ls /w/projx"),
            ]
        );
    }

    #[test]
    fn the_reply_is_not_repeated_in_its_block() {
        let all = lines(&[
            "Tests pass now; updating the docs.",
            "→ Edit /w/README.md",
            "All done.\nusine-status: wrapping up",
            "✔ finished",
        ]);
        let items = work_items(&all, 4, Path::new("/w"), Some("All done."));
        assert_eq!(
            texts(&items),
            vec![
                (WorkKind::Text, "Tests pass now; updating the docs."),
                (WorkKind::Tool, "Edit README.md"),
                (WorkKind::Note, "✔ finished"),
            ]
        );
        assert_eq!(items[0].idx, 4);
        // A reply that differs from the last prose leaves it alone.
        let kept = work_items(&all, 0, Path::new("/w"), Some("Something else"));
        assert_eq!(kept.len(), 4);
    }

    #[test]
    fn the_preview_is_the_last_prose_not_thinking_or_tools() {
        let all = lines(&[
            "Found it in the lexer.",
            "∴ Maybe also check the parser.",
            "→ Bash cargo test",
        ]);
        let items = work_items(&all, 0, Path::new("/w"), None);
        assert_eq!(preview(&items), Some("Found it in the lexer."));
        let quiet = work_items(&lines(&["→ Bash ls", "∴ hmm"]), 0, Path::new("/w"), None);
        assert_eq!(preview(&quiet), None);
    }

    #[test]
    fn durations_read_short() {
        assert_eq!(fmt_duration(4_400), "4s");
        assert_eq!(fmt_duration(185_000), "3m");
        assert_eq!(fmt_duration(3_900_000), "1h 05m");
    }
}
