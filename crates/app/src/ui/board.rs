use dioxus::prelude::*;
use usine_core::{Card, Column, Project};
use uuid::Uuid;

use super::card::CardView;
use super::icons::IconChevron;
use super::newcard::NewCardIntent;
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
    let filter = super::search::query();
    if let Some(q) = &filter {
        cards.retain(|c| super::search::matches(&c.title, q));
    }

    // Read the folded list once here rather than per lane, so a settings echo
    // re-renders the board in one pass.
    let collapsed = state.settings.read().collapsed_columns.clone();

    rsx! {
        div { class: "board",
            for (col , col_cards) in visible_columns(&cards, filter.is_some()) {
                ColumnView {
                    key: "{col:?}",
                    column: col,
                    cards: col_cards,
                    collapsed: collapsed.contains(&col),
                }
            }
        }
    }
}

/// Flip `col`'s entry in the persisted list of folded lanes. Absent = open,
/// which is what makes "open by default" fall out of an empty (or missing)
/// record.
fn toggle_collapsed(list: &mut Vec<Column>, col: Column) {
    if let Some(i) = list.iter().position(|c| *c == col) {
        list.remove(i);
    } else {
        list.push(col);
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
    // The Review lane holds the whole pre-PR gate — cards waiting on the user
    // next to cards the agent is still working — so float what wants you above
    // what doesn't. The other lanes are uniform enough not to need it.
    if col == Column::SelfReview {
        out.sort_by_key(|c| !c.needs_attention());
    }
    // `sort_by_key` is stable, so each group keeps the order chosen above.
    out.sort_by_key(|c| c.blocked);
    out
}

/// The lanes the board actually draws, with their cards. An empty lane is left
/// out entirely: eight full-width columns plus the sidebar and the detail panel
/// is more board than a laptop shows at once, and the lanes that are empty
/// (typically half of them) are the ones costing the most for the least. A lane
/// comes back on its own the moment a card lands in it, so there is nothing to
/// persist. The starting block always shows — it holds "+ Add card", which is
/// where an empty board begins.
///
/// While the find bar filters the board (`keep_all`), every lane stays: there
/// an empty lane is the answer — the zero counts read as "no matches here"
/// instead of an empty project.
fn visible_columns(cards: &[Card], keep_all: bool) -> Vec<(Column, Vec<Card>)> {
    Column::board()
        .into_iter()
        .filter_map(|col| {
            let col_cards = column_cards(cards, col);
            let keep = keep_all || !col_cards.is_empty() || col == Column::StartingBlock;
            keep.then_some((col, col_cards))
        })
        .collect()
}

/// One lane. When `collapsed` it keeps its header — title, count, chevron — so
/// it still reports what landed in it, and only drops its cards; the whole
/// folded header is the button that reopens it.
#[component]
fn ColumnView(column: Column, cards: Vec<Card>, collapsed: bool) -> Element {
    let state = use_context::<AppState>();
    let title = column.title();
    let count = cards.len();
    let is_start = column == Column::StartingBlock;

    let toggle = move |_| {
        let mut s = state.settings.read().clone();
        toggle_collapsed(&mut s.collapsed_columns, column);
        state.save_settings(s);
    };

    let class = if collapsed {
        "column collapsed"
    } else {
        "column"
    };

    rsx! {
        div { class: "{class}",
            if collapsed {
                // Folded: the header itself is the button that reopens the lane,
                // so the chevron is drawn as a plain span — a button inside a
                // button is invalid markup and would toggle twice on one click.
                button {
                    class: "column-header column-header-btn",
                    title: "Expand {title}",
                    aria_label: "Expand {title}",
                    aria_expanded: "false",
                    onclick: toggle,
                    span { class: "column-title", "{title}" }
                    span { class: "column-header-right",
                        span { class: "column-count", "{count}" }
                        span { class: "column-toggle", IconChevron {} }
                    }
                }
            } else {
                div { class: "column-header", title: "{title}",
                    span { class: "column-title", "{title}" }
                    div { class: "column-header-right",
                        span { class: "column-count", "{count}" }
                        button {
                            class: "card-icon-btn column-toggle",
                            title: "Collapse {title}",
                            aria_label: "Collapse {title}",
                            aria_expanded: "true",
                            onclick: toggle,
                            IconChevron {}
                        }
                    }
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
}

/// Where a new card goes when the starting-block buttons are clicked.
#[derive(Debug, PartialEq)]
enum NewCardTarget {
    Project(Uuid),
    Ask,
}

/// `None` = no projects at all (the caller shows the "Add a project" hint). A
/// project view names its own target; the Home view asks, unless there is only
/// one project and therefore nothing to ask about.
fn new_card_target(view: SelectedView, projects: &[Project]) -> Option<NewCardTarget> {
    match view {
        SelectedView::Project(id) => Some(NewCardTarget::Project(id)),
        SelectedView::Global => match projects {
            [] => None,
            [only] => Some(NewCardTarget::Project(only.id)),
            _ => Some(NewCardTarget::Ask),
        },
    }
}

/// Creates a blank card and opens it for editing, or adopts an existing branch
/// or PR.
/// In a project view both act on that project; from the Home view they ask
/// which project first (unless there is only one). Lives at the bottom of the
/// column.
#[component]
fn AddCardButton() -> Element {
    let state = use_context::<AppState>();
    let projects = state.projects.read().clone();
    let view = *state.selected_view.read();
    let Some(target) = new_card_target(view, &projects) else {
        return rsx! {
            div { class: "hint center", "Add a project to create cards." }
        };
    };
    let target = match target {
        NewCardTarget::Project(id) => Some(id),
        NewCardTarget::Ask => None,
    };

    rsx! {
        button {
            class: "add-card",
            onclick: move |_| match target {
                Some(id) => state.create_card(id, String::new(), String::new()),
                None => super::newcard::open_project_picker(NewCardIntent::Blank),
            },
            "+ Add card"
        }
        button {
            class: "add-card",
            onclick: move |_| match target {
                Some(id) => {
                    // Fetch first so the picker fills as the modal appears.
                    state.fetch_adopt_sources(id);
                    super::adoptdialog::open_adopt_dialog(id);
                }
                None => super::newcard::open_project_picker(NewCardIntent::Adopt),
            },
            "⤵ Adopt branch/PR…"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use usine_core::{CardConfig, CardState};

    fn project(name: &str) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: PathBuf::from("/tmp").join(name),
            config: Default::default(),
        }
    }

    #[test]
    fn a_project_view_targets_its_own_project() {
        let (a, b) = (project("a"), project("b"));
        assert_eq!(
            new_card_target(SelectedView::Project(b.id), &[a, b.clone()]),
            Some(NewCardTarget::Project(b.id))
        );
    }

    #[test]
    fn home_asks_only_when_there_is_a_choice() {
        let (a, b) = (project("a"), project("b"));
        assert_eq!(new_card_target(SelectedView::Global, &[]), None);
        assert_eq!(
            new_card_target(SelectedView::Global, std::slice::from_ref(&a)),
            Some(NewCardTarget::Project(a.id))
        );
        assert_eq!(
            new_card_target(SelectedView::Global, &[a, b]),
            Some(NewCardTarget::Ask)
        );
    }

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
    fn empty_lanes_are_left_out_but_the_starting_block_stays() {
        // One card in Implementing: that lane and the starting block draw, the
        // six other lanes are gone rather than collapsed.
        let cards = vec![card(
            "work",
            CardState::Implementing(usine_core::RunSub::Running),
            false,
            1,
        )];
        let shown = visible_columns(&cards, false);
        let cols: Vec<Column> = shown.iter().map(|(c, _)| *c).collect();
        assert_eq!(cols, [Column::StartingBlock, Column::Implementing]);
        assert_eq!(titles(&shown[1].1), ["work"]);
        // The starting block draws empty — it holds "+ Add card".
        assert!(shown[0].1.is_empty());

        // An empty board is just the starting block.
        let cols: Vec<Column> = visible_columns(&[], false)
            .iter()
            .map(|(c, _)| *c)
            .collect();
        assert_eq!(cols, [Column::StartingBlock]);
    }

    #[test]
    fn a_filtered_board_keeps_every_lane() {
        // The find bar filtered everything away: the lanes stay so the zero
        // counts read as "no matches", not as an empty project.
        let shown = visible_columns(&[], true);
        let cols: Vec<Column> = shown.iter().map(|(c, _)| *c).collect();
        assert_eq!(cols, Column::board().to_vec());
        assert!(shown.iter().all(|(_, c)| c.is_empty()));
    }

    #[test]
    fn folding_a_lane_is_a_toggle_on_the_persisted_list() {
        // Empty list = every lane open, which is the fresh-install state.
        let mut list = Vec::new();
        toggle_collapsed(&mut list, Column::Done);
        assert_eq!(list, [Column::Done]);
        // A second lane folds without disturbing the first.
        toggle_collapsed(&mut list, Column::StartingBlock);
        assert_eq!(list, [Column::Done, Column::StartingBlock]);
        // Reopening removes only that lane.
        toggle_collapsed(&mut list, Column::Done);
        assert_eq!(list, [Column::StartingBlock]);
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
    fn the_review_lane_floats_the_cards_waiting_on_the_user() {
        use usine_core::ReviewSub;
        // The pre-PR gate is one lane now, so a card the agent is still
        // self-reviewing must not bury one that is waiting to be looked at.
        let cards = vec![
            card(
                "reviewing",
                CardState::AwaitingReview(ReviewSub::Reviewing),
                false,
                1,
            ),
            card(
                "ready-for-pr",
                CardState::AwaitingReview(ReviewSub::ReadyForPr),
                false,
                2,
            ),
            card(
                "applying",
                CardState::AwaitingReview(ReviewSub::ApplyingFixes),
                false,
                3,
            ),
            card(
                "pick-fixes",
                CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts: vec![] }),
                false,
                4,
            ),
        ];
        let out = column_cards(&cards, Column::SelfReview);
        assert_eq!(
            titles(&out),
            ["ready-for-pr", "pick-fixes", "reviewing", "applying"]
        );

        // Blocked still sinks below everything, attention or not.
        let cards = vec![
            card(
                "blocked-ready",
                CardState::AwaitingReview(ReviewSub::ReadyForPr),
                true,
                1,
            ),
            card(
                "reviewing",
                CardState::AwaitingReview(ReviewSub::Reviewing),
                false,
                2,
            ),
        ];
        let out = column_cards(&cards, Column::SelfReview);
        assert_eq!(titles(&out), ["reviewing", "blocked-ready"]);
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
