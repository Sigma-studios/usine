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
                    cards: column_cards(&cards, col),
                }
            }
        }
    }
}

/// One column's cards in display order. The working columns keep the list's
/// creation order (oldest first); Done shows the most recently touched card on
/// top, since the transition into Done stamps `updated_at` — i.e. "recently
/// finished first" instead of a pile sorted by age. On top of that, every
/// column sinks the cards the user marked blocked below the live work.
fn column_cards(cards: &[Card], col: Column) -> Vec<Card> {
    let mut out: Vec<Card> = cards
        .iter()
        .filter(|c| c.column() == col)
        .cloned()
        .collect();
    if col == Column::Done {
        out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    }
    // `sort_by_key` is stable, so each group keeps the order chosen above.
    out.sort_by_key(|c| c.blocked);
    out
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
        button {
            class: "add-card",
            onclick: move |_| {
                // Fetch first so the picker fills as the modal appears.
                state.fetch_adopt_sources(target);
                super::adoptdialog::open_adopt_dialog(target);
            },
            "⤵ Adopt branch…"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usine_core::{CardConfig, CardState};
    use uuid::Uuid;

    /// A card with a recognisable title, so assertions read as the column does.
    fn card(title: &str, state: CardState, blocked: bool, stamp: i64) -> Card {
        let mut c = Card::new(Uuid::new_v4(), title, "", CardConfig::default());
        c.state = state;
        c.blocked = blocked;
        c.created_at = stamp;
        c.updated_at = stamp;
        c
    }

    fn titles(cards: &[Card]) -> Vec<&str> {
        cards.iter().map(|c| c.title.as_str()).collect()
    }

    #[test]
    fn blocked_cards_sink_in_a_working_column() {
        // Given in creation order, blocked interleaved with live work.
        let cards = vec![
            card("a", CardState::StartingBlock, true, 1),
            card("b", CardState::StartingBlock, false, 2),
            card("c", CardState::StartingBlock, true, 3),
            card("d", CardState::StartingBlock, false, 4),
        ];
        let out = column_cards(&cards, Column::StartingBlock);
        assert_eq!(titles(&out), ["b", "d", "a", "c"]);
    }

    #[test]
    fn done_keeps_recency_within_each_group() {
        let cards = vec![
            card("old", CardState::Done, false, 1),
            card("blocked-old", CardState::Done, true, 2),
            card("new", CardState::Done, false, 3),
            card("blocked-new", CardState::Done, true, 4),
        ];
        let out = column_cards(&cards, Column::Done);
        assert_eq!(titles(&out), ["new", "old", "blocked-new", "blocked-old"]);
    }

    #[test]
    fn uniform_columns_keep_their_order() {
        for blocked in [false, true] {
            let cards = vec![
                card("a", CardState::StartingBlock, blocked, 1),
                card("b", CardState::StartingBlock, blocked, 2),
            ];
            let out = column_cards(&cards, Column::StartingBlock);
            assert_eq!(titles(&out), ["a", "b"]);

            let done = vec![
                card("old", CardState::Done, blocked, 1),
                card("new", CardState::Done, blocked, 2),
            ];
            let out = column_cards(&done, Column::Done);
            assert_eq!(titles(&out), ["new", "old"]);
        }
    }
}
