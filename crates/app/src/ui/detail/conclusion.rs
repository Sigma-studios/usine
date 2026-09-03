//! The concluded-investigation panel: renders the conclusion, offers a
//! follow-up round ("dig deeper"), and can convert the card — in place — into
//! an implementation whose prompt carries the findings.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::drafts;
use crate::ui::textfield::use_push_back;

#[component]
pub(super) fn ConclusionPanel(card_id: Uuid, conclusion: String) -> Element {
    let state = use_context::<AppState>();
    let mut follow_up = drafts::use_draft(card_id, "investigate.followup", String::new);
    // Nothing to send is nothing to do — same rule the chat section's two
    // buttons already follow, rather than a live button that no-ops.
    let blank = follow_up.read().trim().is_empty();
    // Uncontrolled (see `ui/textfield.rs`), so clearing the box after a send
    // only reaches the DOM through a remount.
    let mut pushback = use_push_back(follow_up.read().clone());

    rsx! {
        div { class: "section",
            h3 { "Conclusion" }
            div { class: "plan-box", "{conclusion}" }
        }

        div { class: "section",
            h3 { "Dig deeper" }
            div { class: "field",
                for g in [pushback.key()] {
                    textarea {
                        key: "{g}",
                        placeholder: "What should the agent look into next?",
                        initial_value: "{follow_up.peek()}",
                        oninput: move |e| {
                            pushback.typed(&e.value());
                            follow_up.set(e.value());
                        },
                        // Paste an image to attach it (text paste is unaffected).
                        onpaste: move |_| {
                            if let Some(png) = super::edit::clipboard_image_png() {
                                state.attach_image_bytes(card_id, png);
                            }
                        },
                    }
                }
            }
            button {
                class: "btn",
                disabled: blank,
                title: "The agent re-investigates with this conclusion and every earlier round as context",
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
            button {
                class: "btn primary",
                title: "Continues this card as an implementation: the findings are folded into the task description and the card returns to the starting block, where you shape the prompt before starting",
                onclick: move |_| state.send(ExecutorCommand::ConvertToImplementation { card_id }),
                "Turn into implementation"
            }
        }
    }
}
