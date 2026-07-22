//! The concluded-investigation panel: renders the conclusion, offers a
//! follow-up round ("dig deeper"), and can convert the card — in place — into
//! an implementation whose prompt carries the findings.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;

#[component]
pub(super) fn ConclusionPanel(card_id: Uuid, conclusion: String) -> Element {
    let state = use_context::<AppState>();
    let mut follow_up = use_signal(String::new);

    rsx! {
        div { class: "section",
            h3 { "Conclusion" }
            div { class: "plan-box", "{conclusion}" }
        }

        div { class: "section",
            h3 { "Dig deeper" }
            div { class: "hint",
                "Ask a follow-up — the agent re-investigates with this conclusion and every earlier round as context."
            }
            div { class: "field",
                textarea {
                    placeholder: "What should the agent look into next?",
                    value: "{follow_up}",
                    oninput: move |e| follow_up.set(e.value()),
                }
            }
            button {
                class: "btn",
                onclick: move |_| {
                    let fb = follow_up.read().trim().to_string();
                    if !fb.is_empty() {
                        state.send(ExecutorCommand::FollowUpInvestigation { card_id, feedback: fb });
                        follow_up.set(String::new());
                    }
                },
                "Investigate further"
            }
        }

        div { class: "section",
            h3 { "Turn into implementation" }
            div { class: "hint",
                "Continue this card as an implementation: the findings are folded into the task description and the card returns to the starting block, where you shape the prompt before starting."
            }
            button {
                class: "btn primary",
                onclick: move |_| state.send(ExecutorCommand::ConvertToImplementation { card_id }),
                "Turn into implementation"
            }
        }
    }
}
