//! The shared "Agent Chat" section: one textarea, two ways to use it. "Request
//! changes" bounces the work back through the panel's own dispatch (each call
//! site wires its command); "Ask questions" runs a read-only Q&A turn that
//! returns the card to where it sits, with the answer rendered here.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::drafts;

/// Props: `hint` explains what "request changes" means at this stage;
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
    hint: String,
    on_request: EventHandler<String>,
    #[props(default = false)] request_enabled_when_blank: bool,
    request_label: Option<String>,
    request_label_nonblank: Option<String>,
) -> Element {
    let state = use_context::<AppState>();
    let mut text = drafts::use_draft(card_id, "chat", String::new);
    let exchange = state.answers.read().get(&card_id).cloned();
    let blank = text.read().trim().is_empty();
    let request_label = match (blank, request_label_nonblank) {
        (false, Some(l)) => l,
        _ => request_label.unwrap_or_else(|| "Request changes".to_string()),
    };

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
                    "{request_label}"
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
