//! The "Adopt branch" modal: turn an existing hand-made branch into a card
//! that starts in the self-review pipeline. Same host pattern as `confirm`: a
//! global holds the open request (the target project), and the host renders
//! the overlay at the app root.
//!
//! Picking a branch fires a probe; its result (commits ahead, a dirty
//! checkout, an open PR) drives the prefills and warnings. The description is
//! mandatory — it becomes the card's task statement, which every downstream
//! agent run (review, fixes) reads as the statement of intent.

use dioxus::prelude::*;
use usine_core::DirtyAction;
use uuid::Uuid;

use crate::state::AppState;

static ADOPT: GlobalSignal<Option<Uuid>> = Signal::global(|| None);

/// Open the dialog for a project. The caller fetches the branch list first
/// (`AppState::fetch_adopt_sources`) so the picker fills as the modal appears.
pub(crate) fn open_adopt_dialog(project_id: Uuid) {
    *ADOPT.write() = Some(project_id);
}

fn dismiss() {
    *ADOPT.write() = None;
}

/// A default card title from the branch name: the leaf, separators spaced out
/// (`feat/add-oauth` → "add oauth"). Just a prefill — the user can overwrite.
fn title_from_ref(source_ref: &str) -> String {
    source_ref
        .rsplit('/')
        .next()
        .unwrap_or(source_ref)
        .replace(['-', '_'], " ")
}

#[component]
pub fn AdoptDialogHost() -> Element {
    let project_id = *ADOPT.read();
    let Some(project_id) = project_id else {
        return rsx! {};
    };
    rsx! {
        AdoptDialog { key: "{project_id}", project_id }
    }
}

#[component]
fn AdoptDialog(project_id: Uuid) -> Element {
    let state = use_context::<AppState>();
    let mut source = use_signal(String::new);
    let mut title = use_signal(String::new);
    let mut description = use_signal(String::new);
    // Once the user typed in a field, branch picks stop overwriting it.
    let mut title_edited = use_signal(|| false);
    let mut desc_edited = use_signal(|| false);
    let mut retire = use_signal(|| true);
    let mut include_dirty = use_signal(|| true);

    let refs = state
        .adopt_sources
        .read()
        .get(&project_id)
        .cloned()
        .unwrap_or_default();
    let picked = source.read().clone();
    // Only a probe answering the *current* pick counts; an earlier pick's
    // response arriving late is stale.
    let probe = state
        .adopt_probes
        .read()
        .get(&project_id)
        .filter(|p| !picked.is_empty() && p.source_ref == picked)
        .cloned();

    // Prefill the description from the probe's commit subjects, unless the
    // user already wrote their own.
    use_effect(move || {
        let probes = state.adopt_probes.read();
        let current = source.read().clone();
        let Some(p) = probes
            .get(&project_id)
            .filter(|p| !current.is_empty() && p.source_ref == current)
        else {
            return;
        };
        if !*desc_edited.peek() && !p.subjects.is_empty() {
            let text = p
                .subjects
                .iter()
                .map(|s| format!("- {s}"))
                .collect::<Vec<_>>()
                .join("\n");
            description.set(text);
        }
    });

    let remote_only = picked.starts_with("origin/");
    let refusal = probe.as_ref().and_then(|p| p.refusal.clone());
    let dirty = probe.as_ref().and_then(|p| p.dirty_checkout.clone());
    let open_pr = probe.as_ref().and_then(|p| p.open_pr.clone());
    let commits = probe.as_ref().map(|p| p.commits_ahead);
    // An empty branch is only adoptable via a dirty include — mirror the
    // executor's refusal so the button doesn't invite a doomed submit.
    let empty_diff = commits == Some(0) && !(dirty.is_some() && *include_dirty.read());
    let can_submit = !picked.is_empty()
        && refusal.is_none()
        && !empty_diff
        && !title.read().trim().is_empty()
        && !description.read().trim().is_empty();

    let submit = move |_| {
        let dirty_action = if *include_dirty.peek() {
            DirtyAction::Include
        } else {
            DirtyAction::Ignore
        };
        state.adopt_branch(
            project_id,
            source.peek().clone(),
            title.peek().trim().to_string(),
            description.peek().trim().to_string(),
            // A remote-only ref has no local branch to retire.
            *retire.peek() && !source.peek().starts_with("origin/"),
            dirty_action,
        );
        dismiss();
    };

    rsx! {
        div { class: "modal-overlay confirm-overlay", onclick: move |_| dismiss(),
            div {
                class: "modal",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        dismiss();
                    }
                },
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                h3 { class: "modal-title", "Adopt a branch" }
                div { class: "adopt-form",
                    div { class: "field",
                        label { r#for: "adopt-branch", "Branch" }
                        select {
                            id: "adopt-branch",
                            onchange: move |e| {
                                let picked = e.value();
                                if !*title_edited.peek() {
                                    title.set(title_from_ref(&picked));
                                }
                                if !*desc_edited.peek() {
                                    description.set(String::new());
                                }
                                source.set(picked.clone());
                                if !picked.is_empty() {
                                    state.probe_adopt_source(project_id, picked);
                                }
                            },
                            option { value: "", selected: picked.is_empty(), "Pick a branch…" }
                            for r in refs.iter() {
                                option { value: "{r}", selected: picked == *r, "{r}" }
                            }
                        }
                    }
                    if refs.is_empty() {
                        div { class: "hint", "No adoptable branches found — only branches beside the base (and outside Usine) qualify." }
                    }
                    if let Some(reason) = refusal {
                        div { class: "hint warn", "{reason}" }
                    }
                    if let Some(n) = commits {
                        if n > 0 {
                            div { class: "hint", "{n} commit(s) ahead of the base branch." }
                        } else {
                            div { class: "hint warn", "This branch has no commits beyond the base branch." }
                        }
                    }
                    if let Some(pr) = open_pr {
                        div { class: "hint warn",
                            "PR #{pr.number} is already open on this branch — the card will eventually open a second one."
                        }
                    }
                    if let Some(path) = dirty {
                        div { class: "field",
                            label { "Uncommitted changes in {path.display()}" }
                            label { class: "adopt-choice",
                                input {
                                    r#type: "radio",
                                    name: "adopt-dirty",
                                    checked: *include_dirty.read(),
                                    onchange: move |_| include_dirty.set(true),
                                }
                                "Include them (copied and committed on the card's branch; your checkout stays as it is)"
                            }
                            label { class: "adopt-choice",
                                input {
                                    r#type: "radio",
                                    name: "adopt-dirty",
                                    checked: !*include_dirty.read(),
                                    onchange: move |_| include_dirty.set(false),
                                }
                                "Ignore them (adopt only the committed work)"
                            }
                        }
                    }
                    div { class: "field",
                        label { r#for: "adopt-title", "Card title" }
                        input {
                            id: "adopt-title",
                            value: "{title}",
                            oninput: move |e| {
                                title_edited.set(true);
                                title.set(e.value());
                            },
                        }
                    }
                    div { class: "field",
                        label { r#for: "adopt-desc", "Description (required — what this work does; the review and fix agents read it)" }
                        textarea {
                            id: "adopt-desc",
                            value: "{description}",
                            oninput: move |e| {
                                desc_edited.set(true);
                                description.set(e.value());
                            },
                        }
                    }
                    label { class: "adopt-choice",
                        input {
                            r#type: "checkbox",
                            checked: *retire.read() && !remote_only,
                            disabled: remote_only,
                            onchange: move |_| {
                                let v = *retire.peek();
                                retire.set(!v);
                            },
                        }
                        if remote_only {
                            "Retire the original branch (nothing local to retire for a remote branch)"
                        } else {
                            "Retire the original local branch after adopting"
                        }
                    }
                }
                div { class: "modal-actions",
                    button { class: "btn", onclick: move |_| dismiss(), "Cancel" }
                    button {
                        class: "btn primary",
                        disabled: !can_submit,
                        onclick: submit,
                        "Adopt into self-review"
                    }
                }
            }
        }
    }
}
