//! The "nothing to do" panel: a plan or implement run concluded the card needs
//! no code change (already implemented, or not actionable) and said why. Not an
//! error — the user agrees and marks it done, overrules it with a note, or
//! sends the card back to the starting block to reword the ask.

use dioxus::prelude::*;
use usine_core::{ExecutorCommand, NoWorkKind, NoWorkPhase};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::confirm::{request_confirm, ConfirmAction, ConfirmRequest};
use crate::ui::drafts;
use crate::ui::textfield::use_push_back;
use crate::ui::widgets::ArtifactText;

#[component]
pub(super) fn NothingToDoPanel(
    card_id: Uuid,
    phase: NoWorkPhase,
    kind: NoWorkKind,
    explanation: String,
) -> Element {
    let state = use_context::<AppState>();
    let mut note = drafts::use_draft(card_id, "nothing.note", String::new);
    let mut pushback = use_push_back(note.read().clone());
    let subtitle = match kind {
        NoWorkKind::AlreadyDone => "The agent found this is already implemented.",
        NoWorkKind::NotActionable => "The agent judged this request isn't actionable as written.",
    };
    let (proceed_label, proceed_title) = match phase {
        NoWorkPhase::Plan => (
            "Plan anyway",
            "Re-runs the design agent with its verdict and your note as context",
        ),
        NoWorkPhase::Implement => (
            "Implement anyway",
            "Re-runs the implementation with its verdict and your note as context",
        ),
    };
    let done_title = match phase {
        NoWorkPhase::Plan => "Agree with the agent: the card is finished, nothing to merge.",
        NoWorkPhase::Implement => {
            "Agree with the agent: the card is finished, nothing to merge. Its empty worktree and branch are removed (kept if you edited them)."
        }
    };
    // The verdict cites existing code, not a diff, so a file reference opens
    // the file in the editor (as the investigation conclusion does).
    let open_path = move |path: String| {
        state.send(ExecutorCommand::OpenPath { card_id, path });
    };

    rsx! {
        div { class: "section",
            h3 { "Nothing to do" }
            div { class: "hint", "{subtitle}" }
            if explanation.trim().is_empty() {
                div { class: "hint", "The agent gave no explanation — see the transcript." }
            } else {
                ArtifactText { text: explanation.clone(), on_path: open_path }
                div { class: "hint", "The agent's full reasoning is in the transcript." }
            }
            div { class: "row",
                button {
                    class: "btn primary",
                    title: "{done_title}",
                    onclick: move |_| state.send(ExecutorCommand::MarkDone { card_id }),
                    "Mark done"
                }
            }
        }

        div { class: "section", "data-submit-scope": "1",
            h3 { "Disagree?" }
            div { class: "field",
                for g in [pushback.key()] {
                    textarea {
                        key: "{g}",
                        placeholder: "Tell the agent what you actually want (optional)",
                        initial_value: "{note.peek()}",
                        oninput: move |e| {
                            pushback.typed(&e.value());
                            note.set(e.value());
                        },
                        onpaste: move |_| super::edit::attach_from_clipboard(state, card_id),
                    }
                }
            }
            div { class: "row",
                button {
                    class: "btn",
                    "data-submit": "1",
                    title: format!("{proceed_title} ({})", crate::ui::shortcuts::submit_hint()),
                    onclick: move |_| {
                        let text = note.read().trim().to_string();
                        state.send(ExecutorCommand::ProceedAnyway { card_id, note: text });
                        drafts::clear(card_id, "nothing.note", note, String::new());
                    },
                    "{proceed_label}"
                }
                button {
                    class: "btn subtle",
                    title: "Send the card back to the starting block to reword its prompt",
                    onclick: move |_| request_confirm(ConfirmRequest {
                        title: "Back to starting block".into(),
                        message: "Send this card back to the starting block to edit its prompt? Any questions and answers so far are appended to it.".into(),
                        confirm_label: "Send back".into(),
                        danger: false,
                        action: ConfirmAction::Send(ExecutorCommand::BackToStart { card_id }),
                    }),
                    "Back to start (edit prompt)"
                }
            }
        }
    }
}
