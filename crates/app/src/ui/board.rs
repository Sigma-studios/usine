use dioxus::prelude::*;
use usine_core::{Card, Column};

use super::card::CardView;
use super::ReviewBoard;
use crate::state::{AppState, BoardMode, SelectedView};

/// The central board area: the normal card lanes, or the PR-review lanes when
/// review mode is active. Switching keeps the sidebar (a sibling) untouched.
#[component]
pub fn BoardArea() -> Element {
    let state = use_context::<AppState>();
    match state.board_mode() {
        BoardMode::Review => rsx! { ReviewBoard {} },
        BoardMode::Normal => rsx! { Board {} },
    }
}

#[component]
pub fn Board() -> Element {
    let state = use_context::<AppState>();
    let mut cards = state.visible_cards();
    // Live Ctrl+F/Cmd+F filter; column counts follow since they're computed
    // from the list handed to each column.
    if let Some(q) = super::search::query() {
        cards.retain(|c| super::search::matches(&c.title, &q));
    }

    rsx! {
        div { class: "board",
            for col in Column::board() {
                ColumnView {
                    key: "{col:?}",
                    column: col,
                    cards: cards.iter().filter(|c| c.column() == col).cloned().collect::<Vec<_>>(),
                }
            }
        }
    }
}

#[component]
fn ColumnView(column: Column, cards: Vec<Card>) -> Element {
    let title = column.title();
    let count = cards.len();
    let is_start = column == Column::StartingBlock;

    rsx! {
        div { class: "column",
            div { class: "column-header",
                span { "{title}" }
                span { class: "column-count", "{count}" }
            }
            div { class: "column-body",
                for card in cards.iter() {
                    CardView { key: "{card.id}", card: card.clone() }
                }
                if is_start {
                    AddCardButton {}
                }
            }
        }
    }
}

/// Creates a blank card in the current project (or the first project in the
/// global view) and opens it for editing. Lives at the bottom of the column.
#[component]
fn AddCardButton() -> Element {
    let state = use_context::<AppState>();
    let projects = state.projects.read().clone();
    if projects.is_empty() {
        return rsx! {
            div { class: "hint", "Add a project to create cards." }
        };
    }
    let target = match *state.selected_view.read() {
        SelectedView::Project(id) => id,
        SelectedView::Global => projects[0].id,
    };

    rsx! {
        button {
            class: "add-card",
            onclick: move |_| state.create_card(target, String::new(), String::new()),
            "+ Add card"
        }
    }
}
