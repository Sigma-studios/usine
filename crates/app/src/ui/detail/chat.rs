//! The shared "Agent Chat" section: one textarea, two ways to use it. "Request
//! changes" bounces the work back through the panel's own dispatch (each call
//! site wires its command); "Ask questions" runs a read-only Q&A turn that
//! returns the card to where it sits, with the answer rendered here.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::drafts;

/// The one line above the box, at every gate that renders this section. It used
/// to be a two-line paragraph rewritten per stage — five near-identical
/// variants saying the same thing the two buttons already say. What differs per
/// stage is what "request changes" *does*, which now rides on that button's
/// tooltip.
pub(super) const CHAT_HINT: &str = "Send this back to the agent, or just ask about the work.";

/// Props: `hint` overrides [`CHAT_HINT`] (only the plan panel needs to, since
/// its request button also carries the answered questions);
/// `on_request` dispatches the panel-specific change command with the typed
/// text. `request_enabled_when_blank` lets the plan panel submit answered
/// questions with no free-form text (its request folds both), and
/// `request_label` lets it retitle the button to what the send actually does
/// (e.g. "Send answers") — with `request_label_nonblank` overriding it while
/// the box holds text, for sends that fold the typed feedback in. The textarea
/// is a draft: typed-but-unsent text survives deselects, state changes, and
/// moves between the parked states that all render this section.
#[component]
pub(super) fn AgentChatSection(
    card_id: Uuid,
    #[props(default = CHAT_HINT.to_string())] hint: String,
    on_request: EventHandler<String>,
    #[props(default = false)] request_enabled_when_blank: bool,
    request_label: Option<String>,
    request_label_nonblank: Option<String>,
    /// What the request button actually does at this stage — the per-stage
    /// specifics the shared hint no longer spells out.
    request_title: Option<String>,
) -> Element {
    let state = use_context::<AppState>();
    let mut text = drafts::use_draft(card_id, "chat", String::new);
    // The box is *uncontrolled* (`initial_value` → `defaultValue`), so a
    // re-render can never rewind what has been typed since the keystroke that
    // caused it — see `src/stress.rs`. The cost is that clearing the signal no
    // longer clears the DOM: `defaultValue` doesn't touch a user-dirtied field.
    // Both sends therefore bump this generation, and the element is keyed with
    // it, so the clear lands as a fresh, empty element.
    let mut generation = use_signal(|| 0u32);
    // The whole Q&A log: newest expanded, earlier ones collapsed behind a
    // one-line summary of the question. A write run marks the log superseded,
    // which collapses all of them without dropping any.
    let log = state
        .answers
        .read()
        .get(&card_id)
        .cloned()
        .unwrap_or_default();
    let blank = text.read().trim().is_empty();
    let request_title = request_title
        .unwrap_or_else(|| "Sends the work back to the agent with this text".to_string());
    let request_label = match (blank, request_label_nonblank) {
        (false, Some(l)) => l,
        _ => request_label.unwrap_or_else(|| "Request changes".to_string()),
    };

    rsx! {
        div { class: "section",
            h3 { "Agent Chat" }
            div { class: "hint", "{hint}" }
            if !log.exchanges.is_empty() {
                div { class: "qa-log",
                    // Newest first, and only it is ever rendered `open` — an
                    // older row the user expanded keeps `open: false` in the
                    // vdom, so no re-render force-collapses it.
                    for (i , ex) in log.exchanges.iter().rev().enumerate() {
                        details {
                            key: "{ex.asked_at}",
                            class: "qa-item",
                            open: i == 0 && !log.superseded,
                            summary { title: "{ex.question}", "{summary_line(&ex.question)}" }
                            if !ex.question.is_empty() {
                                div { class: "hint", "You asked" }
                                div { class: "plan-box", "{ex.question}" }
                            }
                            div { class: "hint", "Answer" }
                            div { class: "plan-box", "{ex.answer}" }
                        }
                    }
                }
            }
            div { class: "field",
                if crate::stress::fix_a() {
                    // A lone child's `key` is ignored — Dioxus only honors keys
                    // among siblings in a list — so the single-item `for` is what
                    // makes the generation bump actually remount the element.
                    for g in [generation()] {
                        textarea {
                            key: "{g}",
                            // Stable hook for the debug keystroke-drop harness.
                            id: "chat-input",
                            placeholder: "What should change — or what do you want to know?",
                            initial_value: "{text.peek()}",
                            oninput: move |e| {
                                let v = e.value();
                                crate::stress::record_chat_input(&v);
                                text.set(v);
                            },
                        }
                    }
                } else {
                    textarea {
                        id: "chat-input",
                        placeholder: "What should change — or what do you want to know?",
                        value: "{text}",
                        oninput: move |e| {
                            let v = e.value();
                            crate::stress::record_chat_input(&v);
                            text.set(v);
                        },
                    }
                }
            }
            div { class: "row",
                button {
                    class: "btn",
                    disabled: blank && !request_enabled_when_blank,
                    onclick: move |_| {
                        let t = text.read().trim().to_string();
                        if !t.is_empty() || request_enabled_when_blank {
                            on_request.call(t);
                            text.set(String::new());
                            generation += 1;
                        }
                    },
                    title: "{request_title}",
                    "{request_label}"
                }
                button {
                    class: "btn",
                    disabled: blank,
                    title: "A read-only run: the agent answers here and the card stays exactly where it is",
                    onclick: move |_| {
                        let q = text.read().trim().to_string();
                        if !q.is_empty() {
                            state.send(ExecutorCommand::AskQuestion { card_id, question: q });
                            text.set(String::new());
                            generation += 1;
                        }
                    },
                    "Ask questions"
                }
            }
        }
    }
}

/// The collapsed row's label: the text on one line, capped. A legacy entry
/// recorded before questions were kept falls back to a neutral label. Shared
/// with the panel's collapsed task, which wants the same one-line treatment.
pub(super) fn summary_line(question: &str) -> String {
    let flat = question.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "Answer".to_string();
    }
    if flat.chars().count() > 90 {
        format!("{}…", flat.chars().take(89).collect::<String>())
    } else {
        flat
    }
}
