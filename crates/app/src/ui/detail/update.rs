//! "Update from <base>…": merge the base branch into the card's branch and
//! have an agent adapt the card's work to what landed. Offered at the parked
//! gates around the PR (ready for PR, failed validation, the PR gate, the
//! merge gate); the card returns to the same gate once the run finishes.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::drafts;

/// Collapsed, a single button; open, an optional note for the agent and the
/// confirm. The note is a draft (it survives the panel's remounts), and a
/// panel that mounts with one restored opens straight onto it.
#[component]
pub(super) fn UpdateFromBase(card_id: Uuid, base: String) -> Element {
    let state = use_context::<AppState>();
    let mut note = drafts::use_draft(card_id, "update.note", String::new);
    let mut open = use_signal(|| !note.peek().is_empty());

    if !open() {
        return rsx! {
            div { class: "row",
                button {
                    class: "btn",
                    title: "Merge origin/{base} into this card's branch, then have the agent check the card's work against what landed and adapt it",
                    onclick: move |_| open.set(true),
                    "Update from {base}…"
                }
            }
        };
    }

    rsx! {
        div { class: "section update-from-base",
            h3 { "Update from {base}" }
            div { class: "hint",
                "Merges origin/{base} into this branch, then the agent checks this card's work against what landed (renamed symbols, changed APIs, migrations) and adapts it — or says nothing needs to change. The card comes back here afterwards."
            }
            div { class: "field",
                textarea {
                    placeholder: "Optional: what should the agent look out for?",
                    initial_value: "{note.peek()}",
                    oninput: move |e| note.set(e.value()),
                }
            }
            div { class: "row",
                button {
                    class: "btn primary",
                    onclick: move |_| {
                        let t = note.read().trim().to_string();
                        state.send(ExecutorCommand::UpdateFromBase {
                            card_id,
                            note: (!t.is_empty()).then_some(t),
                        });
                        drafts::clear(card_id, "update.note", note, String::new());
                        open.set(false);
                    },
                    "Merge {base} and check"
                }
                button {
                    class: "btn subtle",
                    onclick: move |_| {
                        drafts::clear(card_id, "update.note", note, String::new());
                        open.set(false);
                    },
                    "Cancel"
                }
            }
        }
    }
}
