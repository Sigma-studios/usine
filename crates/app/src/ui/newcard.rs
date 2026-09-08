//! The "which project?" modal. On the Home (global) view the starting block's
//! buttons have no project to act on, so they ask for one first instead of
//! silently picking whichever project happens to sort first. Same host pattern
//! as `confirm` / `adoptdialog`: a global holds the pending request, and the
//! host renders the overlay at the app root.

use dioxus::prelude::*;
use usine_core::{Card, Project};
use uuid::Uuid;

use crate::state::AppState;

/// What the picked project is for. Both starting-block buttons need a project,
/// and neither has one on the Home view.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum NewCardIntent {
    Blank,
    Adopt,
}

static PICK: GlobalSignal<Option<NewCardIntent>> = Signal::global(|| None);

pub(crate) fn open_project_picker(intent: NewCardIntent) {
    *PICK.write() = Some(intent);
}

fn dismiss() {
    *PICK.write() = None;
}

/// Which project the picker starts on: the one the most recently created card
/// belongs to (still-existing projects only), else the first project. Beats
/// `projects[0]` for the common "I keep adding cards to the same repo" case.
fn default_project(projects: &[Project], cards: &[Card]) -> Option<Uuid> {
    let newest = cards
        .iter()
        .filter(|c| projects.iter().any(|p| p.id == c.project_id))
        .max_by_key(|c| c.created_at)
        .map(|c| c.project_id);
    newest.or_else(|| projects.first().map(|p| p.id))
}

#[component]
pub fn NewCardHost() -> Element {
    let intent = *PICK.read();
    let Some(intent) = intent else {
        return rsx! {};
    };
    rsx! {
        ProjectPicker { intent }
    }
}

#[component]
fn ProjectPicker(intent: NewCardIntent) -> Element {
    let state = use_context::<AppState>();
    let projects = state.projects.read().clone();
    let mut picked = use_signal(|| {
        default_project(&projects, &state.cards.read())
            .map(|id| id.to_string())
            .unwrap_or_default()
    });
    let chosen = picked.read().clone();
    let path = projects
        .iter()
        .find(|p| p.id.to_string() == chosen)
        .map(|p| p.path.display().to_string());
    let (title, confirm_label) = match intent {
        NewCardIntent::Blank => ("New card", "Create card"),
        NewCardIntent::Adopt => ("Adopt a branch", "Continue"),
    };
    let target = Uuid::parse_str(&chosen).ok();

    rsx! {
        div { class: "modal-overlay confirm-overlay", onclick: move |_| dismiss(),
            div {
                class: "modal",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                // Don't dismiss when clicking inside the dialog.
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        dismiss();
                    }
                },
                // Focus the dialog on open so Escape works and focus is trapped here.
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                h3 { class: "modal-title", "{title}" }
                div { class: "field",
                    label { r#for: "newcard-project", "Project" }
                    select {
                        id: "newcard-project",
                        onchange: move |e| picked.set(e.value()),
                        option { value: "", selected: chosen.is_empty(), "Pick a project…" }
                        for p in projects.iter() {
                            option {
                                value: "{p.id}",
                                selected: chosen == p.id.to_string(),
                                "{p.name}"
                            }
                        }
                    }
                }
                // Same-named folders are told apart by path in the sidebar; do
                // the same here so the choice is unambiguous.
                if let Some(path) = path {
                    div { class: "hint", "{path}" }
                }
                div { class: "modal-actions",
                    button { class: "btn", onclick: move |_| dismiss(), "Cancel" }
                    button {
                        class: "btn primary",
                        disabled: target.is_none(),
                        onclick: move |_| {
                            let Some(pid) = target else { return };
                            dismiss();
                            match intent {
                                NewCardIntent::Blank => {
                                    state.create_card(pid, String::new(), String::new())
                                }
                                NewCardIntent::Adopt => {
                                    // Fetch first so the picker fills as the modal appears.
                                    state.fetch_adopt_sources(pid);
                                    super::adoptdialog::open_adopt_dialog(pid);
                                }
                            }
                        },
                        "{confirm_label}"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use usine_core::CardConfig;

    fn project(name: &str) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: PathBuf::from("/tmp").join(name),
            config: Default::default(),
        }
    }

    fn card(project_id: Uuid, stamp: i64) -> Card {
        let mut c = Card::new(project_id, "t", "", CardConfig::default());
        c.created_at = stamp;
        c
    }

    #[test]
    fn newest_cards_project_wins() {
        let (a, b) = (project("a"), project("b"));
        let cards = vec![card(a.id, 10), card(b.id, 20), card(a.id, 5)];
        assert_eq!(default_project(&[a.clone(), b.clone()], &cards), Some(b.id));
    }

    #[test]
    fn falls_back_to_the_first_project() {
        let (a, b) = (project("a"), project("b"));
        assert_eq!(default_project(&[a.clone(), b], &[]), Some(a.id));
        assert_eq!(default_project(&[], &[]), None);
    }

    #[test]
    fn cards_of_removed_projects_are_ignored() {
        let (a, gone) = (project("a"), project("gone"));
        let cards = vec![card(a.id, 10), card(gone.id, 99)];
        assert_eq!(
            default_project(std::slice::from_ref(&a), &cards),
            Some(a.id)
        );
    }
}
