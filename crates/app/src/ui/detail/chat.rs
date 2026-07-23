//! The shared "Agent Chat" section: one textarea, two ways to use it. "Request
//! changes" bounces the work back through the panel's own dispatch (each call
//! site wires its command); "Ask questions" runs a read-only Q&A turn that
//! returns the card to where it sits, with the answer rendered here.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;

/// Props: `hint` explains what "request changes" means at this stage;
/// `on_request` dispatches the panel-specific change command with the typed
/// text. `request_enabled_when_blank` lets the plan panel submit answered
/// questions with no free-form text (its request folds both). A caller that
/// renders the section from several alternating parked states can pass its own
/// `text` signal so a typed-but-unsent draft survives the moves between them.
#[component]
pub(super) fn AgentChatSection(
    card_id: Uuid,
    hint: String,
    on_request: EventHandler<String>,
    #[props(default = false)] request_enabled_when_blank: bool,
    text: Option<Signal<String>>,
) -> Element {
    let state = use_context::<AppState>();
    let own_text = use_signal(String::new);
    let mut text = text.unwrap_or(own_text);
    let exchange = state.answers.read().get(&card_id).cloned();
    let blank = text.read().trim().is_empty();

    rsx! {
        div { class: "section",
            h3 { "Agent Chat" }
            div { class: "hint", "{hint}" }
            if let Some((question, answer)) = exchange {
                if !question.is_empty() {
                    div { class: "hint", "You asked" }
                    div { class: "plan-box", "{question}" }
                }
                div { class: "hint", "Answer" }
                div { class: "plan-box", "{answer}" }
            }
            div { class: "field",
                textarea {
                    placeholder: "What should change — or what do you want to know?",
                    value: "{text}",
                    oninput: move |e| text.set(e.value()),
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
                        }
                    },
                    "Request changes"
                }
                button {
                    class: "btn",
                    disabled: blank,
                    onclick: move |_| {
                        let q = text.read().trim().to_string();
                        if !q.is_empty() {
                            state.send(ExecutorCommand::AskQuestion { card_id, question: q });
                            text.set(String::new());
                        }
                    },
                    "Ask questions"
                }
            }
        }
    }
}
