//! The "Adopt branch/PR" modal: turn an existing hand-made branch into a card
//! that starts in the self-review pipeline, or take over an open PR (say, one
//! opened on another machine) at the PR-review stage. Same host pattern as
//! `confirm`: a global holds the open request (the target project), and the
//! host renders the overlay at the app root.
//!
//! Picking a branch fires a probe; its result (commits ahead, a dirty
//! checkout, an open PR) drives the prefills and warnings. A PR pick needs no
//! probe — the listing already carries its title, body and author. The
//! description is mandatory — it becomes the card's task statement, which
//! every downstream agent run (review, fixes) reads as the statement of intent.

use dioxus::prelude::*;
use usine_core::{DirtyAction, OpenPr};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::textfield::use_push_back;

static ADOPT: GlobalSignal<Option<Uuid>> = Signal::global(|| None);

/// Open the dialog for a project. The caller fetches the branch list first
/// (`AppState::fetch_adopt_sources`) so the picker fills as the modal appears.
pub(crate) fn open_adopt_dialog(project_id: Uuid) {
    *ADOPT.write() = Some(project_id);
}

fn dismiss() {
    *ADOPT.write() = None;
}

/// What the picker's value names. PR options are valued `pr:<n>` — a colon
/// can't appear in a git branch name, so the prefix never collides.
#[derive(Debug, Clone, PartialEq)]
enum Pick {
    None,
    Branch(String),
    Pr(u64),
}

fn parse_pick(value: &str) -> Pick {
    if value.is_empty() {
        return Pick::None;
    }
    match value.strip_prefix("pr:").map(str::parse) {
        Some(Ok(n)) => Pick::Pr(n),
        _ => Pick::Branch(value.to_string()),
    }
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

/// `#n title — head (author)[, draft]`: enough to recognise a PR in a native
/// select, which can't style its options.
fn pr_label(pr: &OpenPr) -> String {
    let draft = if pr.draft { ", draft" } else { "" };
    format!(
        "#{} {} — {} ({}{draft})",
        pr.number, pr.title, pr.head_ref, pr.author
    )
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
    let prs = state
        .adopt_prs
        .read()
        .get(&project_id)
        .cloned()
        .unwrap_or_default();
    let picked = source.read().clone();
    let pick = parse_pick(&picked);
    let picked_pr = match pick {
        Pick::Pr(n) => prs.iter().find(|p| p.number == n).cloned(),
        _ => None,
    };
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

    // Uncontrolled (see `ui/textfield.rs`): the branch probe prefills both
    // fields while they are mounted, so the prefill only lands via a remount.
    let mut title_push = use_push_back(title.read().clone());
    let mut desc_push = use_push_back(description.read().clone());

    let remote_only = picked.starts_with("origin/");
    let refusal = probe.as_ref().and_then(|p| p.refusal.clone());
    let dirty = probe.as_ref().and_then(|p| p.dirty_checkout.clone());
    let open_pr = probe.as_ref().and_then(|p| p.open_pr.clone());
    let commits = probe.as_ref().map(|p| p.commits_ahead);
    // An empty branch is only adoptable via a dirty include — mirror the
    // executor's refusal so the button doesn't invite a doomed submit.
    let empty_diff = commits == Some(0) && !(dirty.is_some() && *include_dirty.read());
    let fields_ok = !title.read().trim().is_empty() && !description.read().trim().is_empty();
    let can_submit = match &pick {
        Pick::None => false,
        Pick::Branch(_) => refusal.is_none() && !empty_diff && fields_ok,
        // The executor re-checks everything (checked out, diverged…) and
        // answers with an error toast; nothing to probe up front.
        Pick::Pr(_) => picked_pr.is_some() && fields_ok,
    };

    let submit = move |_| {
        if let Pick::Pr(n) = parse_pick(&source.peek()) {
            state.adopt_pr(
                project_id,
                n,
                title.peek().trim().to_string(),
                description.peek().trim().to_string(),
            );
            dismiss();
            return;
        }
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
                h3 { class: "modal-title", "Adopt a branch or PR" }
                div { class: "adopt-form",
                    div { class: "field",
                        label { r#for: "adopt-branch", "Branch or pull request" }
                        select {
                            id: "adopt-branch",
                            onchange: {
                                let prs = prs.clone();
                                move |e: FormEvent| {
                                    let value = e.value();
                                    match parse_pick(&value) {
                                        Pick::Pr(n) => {
                                            let pr = prs.iter().find(|p| p.number == n);
                                            if !*title_edited.peek() {
                                                title.set(pr.map(|p| p.title.clone()).unwrap_or_default());
                                            }
                                            if !*desc_edited.peek() {
                                                // The PR body is its statement of intent;
                                                // an empty one falls back to the title.
                                                let desc = pr
                                                    .map(|p| {
                                                        if p.body.trim().is_empty() {
                                                            p.title.clone()
                                                        } else {
                                                            p.body.clone()
                                                        }
                                                    })
                                                    .unwrap_or_default();
                                                description.set(desc);
                                            }
                                        }
                                        Pick::Branch(ref r) => {
                                            if !*title_edited.peek() {
                                                title.set(title_from_ref(r));
                                            }
                                            if !*desc_edited.peek() {
                                                description.set(String::new());
                                            }
                                            state.probe_adopt_source(project_id, r.clone());
                                        }
                                        Pick::None => {}
                                    }
                                    source.set(value);
                                }
                            },
                            option { value: "", selected: picked.is_empty(), "Pick a branch or PR…" }
                            if !prs.is_empty() {
                                optgroup { label: "Pull requests",
                                    for p in prs.iter() {
                                        option {
                                            value: "pr:{p.number}",
                                            selected: pick == Pick::Pr(p.number),
                                            {pr_label(p)}
                                        }
                                    }
                                }
                            }
                            if !refs.is_empty() {
                                optgroup { label: "Branches",
                                    for r in refs.iter() {
                                        option { value: "{r}", selected: picked == *r, "{r}" }
                                    }
                                }
                            }
                        }
                    }
                    if refs.is_empty() && prs.is_empty() {
                        div { class: "hint",
                            "Nothing to adopt found — only branches beside the base (and outside Usine) qualify, and only open PRs from this repo (not forks) that target the base branch."
                        }
                    }
                    if let Some(pr) = picked_pr.as_ref() {
                        div { class: "hint",
                            "The card takes over PR #{pr.number} at the PR-review stage (no self-review); fixes are pushed to `{pr.head_ref}`."
                        }
                        if !pr.mine {
                            div { class: "hint warn",
                                "This PR was opened by {pr.author}, not you — the card will push its fixes to their branch."
                            }
                        }
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
                            "PR #{pr.number} is open on this branch — pick it under Pull requests to adopt the PR itself."
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
                        for g in [title_push.key()] {
                            input {
                                key: "{g}",
                                id: "adopt-title",
                                initial_value: "{title.peek()}",
                                oninput: move |e| {
                                    title_push.typed(&e.value());
                                    title_edited.set(true);
                                    title.set(e.value());
                                },
                            }
                        }
                    }
                    div { class: "field",
                        label { r#for: "adopt-desc", "Description (required — what this work does; the review and fix agents read it)" }
                        for g in [desc_push.key()] {
                            textarea {
                                key: "{g}",
                                id: "adopt-desc",
                                initial_value: "{description.peek()}",
                                oninput: move |e| {
                                    desc_push.typed(&e.value());
                                    desc_edited.set(true);
                                    description.set(e.value());
                                },
                            }
                        }
                    }
                    if matches!(pick, Pick::Branch(_) | Pick::None) {
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
                }
                div { class: "modal-actions",
                    button { class: "btn", onclick: move |_| dismiss(), "Cancel" }
                    button {
                        class: "btn primary",
                        disabled: !can_submit,
                        onclick: submit,
                        match &pick {
                            Pick::Pr(n) => format!("Adopt PR #{n}"),
                            _ => "Adopt into self-review".to_string(),
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_tell_prs_from_branches() {
        assert_eq!(parse_pick(""), Pick::None);
        assert_eq!(parse_pick("pr:42"), Pick::Pr(42));
        assert_eq!(parse_pick("feat/x"), Pick::Branch("feat/x".into()));
        assert_eq!(
            parse_pick("origin/pr-7"),
            Pick::Branch("origin/pr-7".into())
        );
        // Not a number: can't be one of ours, so it is read as a branch.
        assert_eq!(parse_pick("pr:x"), Pick::Branch("pr:x".into()));
    }
}
