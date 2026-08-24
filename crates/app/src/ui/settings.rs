//! Global settings modal (opened via the cog next to Home). For now it holds
//! only the default models used to seed new projects and cards.

use dioxus::prelude::*;
use usine_core::{CardConfig, PreviewPort};
use uuid::Uuid;

use super::widgets::{
    parse_provider, provider_value, ModelEffortPicker, OptionalModelEffortPicker,
};
use crate::state::AppState;

static SETTINGS_OPEN: GlobalSignal<bool> = Signal::global(|| false);

/// Which project's settings modal is open (its review-contributors picker), if any.
static PROJECT_SETTINGS: GlobalSignal<Option<Uuid>> = Signal::global(|| None);

pub(crate) fn open_project_settings(id: Uuid) {
    *PROJECT_SETTINGS.write() = Some(id);
}

fn close_project_settings() {
    *PROJECT_SETTINGS.write() = None;
}

pub(crate) fn open_settings() {
    *SETTINGS_OPEN.write() = true;
}

fn close_settings() {
    *SETTINGS_OPEN.write() = false;
}

/// A copy-pasteable "open a terminal here" example for the current OS, shown as
/// the settings placeholder. Only a hint — any command with `{path}` works.
fn terminal_command_example() -> &'static str {
    if cfg!(target_os = "macos") {
        "open -a Terminal {path}"
    } else if cfg!(target_os = "windows") {
        "wt -d {path}"
    } else {
        "gnome-terminal --working-directory={path}"
    }
}

/// Which tab of the global settings modal is showing.
#[derive(Clone, Copy, PartialEq)]
enum SettingsTab {
    Models,
    Config,
}

#[component]
pub fn SettingsModal() -> Element {
    if !*SETTINGS_OPEN.read() {
        return rsx! {};
    }
    let state = use_context::<AppState>();
    let settings = state.settings.read().clone();
    let provider = settings.default_provider;
    let terminal_command = settings.terminal_command.clone().unwrap_or_default();
    let editor_command = settings.editor_command.clone().unwrap_or_default();
    let max_concurrent_runs = settings.max_concurrent_runs;
    let terminal_example = terminal_command_example();
    let mut tab = use_signal(|| SettingsTab::Models);

    rsx! {
        div { class: "modal-overlay", onclick: move |_| close_settings(),
            div {
                class: "modal settings-modal",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                // Escape closes the modal (matches the overlay-click behaviour).
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        close_settings();
                    }
                },
                // Focus the dialog on open so Escape works straight away.
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                h3 { class: "modal-title", "Settings" }
                div { class: "settings-tabs",
                    button {
                        class: if tab() == SettingsTab::Models { "settings-tab active" } else { "settings-tab" },
                        onclick: move |_| tab.set(SettingsTab::Models),
                        "Models"
                    }
                    button {
                        class: if tab() == SettingsTab::Config { "settings-tab active" } else { "settings-tab" },
                        onclick: move |_| tab.set(SettingsTab::Config),
                        "Config"
                    }
                }
                if tab() == SettingsTab::Models {
                div { class: "section",
                    h3 { "Default models for new cards" }
                    div { class: "field",
                        label { r#for: "settings-provider", "Provider" }
                        select {
                            id: "settings-provider",
                            value: provider_value(provider),
                            onchange: {
                                let settings = settings.clone();
                                move |e: Event<FormData>| {
                                    let mut s = settings.clone();
                                    let p = parse_provider(&e.value());
                                    let defaults = CardConfig::default_for(p);
                                    s.default_provider = p;
                                    s.default_plan = defaults.plan;
                                    s.default_implement = defaults.implement;
                                    s.default_review = defaults.review;
                                    state.save_settings(s);
                                }
                            },
                            option { value: "claude", selected: provider_value(provider) == "claude", "Claude" }
                            option { value: "codex", selected: provider_value(provider) == "codex", "Codex" }
                        }
                    }
                    div { class: "field",
                        label { "Plan phase" }
                        ModelEffortPicker {
                            provider,
                            spec: settings.default_plan.clone(),
                            on_change: {
                                let settings = settings.clone();
                                move |spec| {
                                    let mut s = settings.clone();
                                    s.default_plan = spec;
                                    state.save_settings(s);
                                }
                            },
                        }
                    }
                    div { class: "field",
                        label { "Implement phase" }
                        ModelEffortPicker {
                            provider,
                            spec: settings.default_implement.clone(),
                            on_change: {
                                let settings = settings.clone();
                                move |spec| {
                                    let mut s = settings.clone();
                                    s.default_implement = spec;
                                    state.save_settings(s);
                                }
                            },
                        }
                    }
                    div { class: "field",
                        label { "Review phase" }
                        OptionalModelEffortPicker {
                            provider,
                            spec: settings.default_review.clone(),
                            inherit_label: "Same as implement",
                            on_change: {
                                let settings = settings.clone();
                                move |spec| {
                                    let mut s = settings.clone();
                                    s.default_review = spec;
                                    state.save_settings(s);
                                }
                            },
                        }
                        div { class: "hint", "Self-review, PR-comment triage, and contributor PR reviews." }
                    }
                    div { class: "hint", "Applies to projects and cards created from now on." }
                }
                }
                if tab() == SettingsTab::Config {
                div { class: "section",
                    h3 { "Open in terminal / editor" }
                    div { class: "field",
                        label { r#for: "settings-terminal-command", "Terminal command" }
                        input {
                            id: "settings-terminal-command",
                            r#type: "text",
                            placeholder: "{terminal_example}",
                            value: "{terminal_command}",
                            onchange: {
                                let settings = settings.clone();
                                move |e: Event<FormData>| {
                                    let mut s = settings.clone();
                                    let v = e.value().trim().to_string();
                                    s.terminal_command = if v.is_empty() { None } else { Some(v) };
                                    state.save_settings(s);
                                }
                            },
                        }
                    }
                    div { class: "field",
                        label { r#for: "settings-editor-command", "Editor command" }
                        input {
                            id: "settings-editor-command",
                            r#type: "text",
                            placeholder: "zed {{path}}",
                            value: "{editor_command}",
                            onchange: {
                                let settings = settings.clone();
                                move |e: Event<FormData>| {
                                    let mut s = settings.clone();
                                    let v = e.value().trim().to_string();
                                    s.editor_command = if v.is_empty() { None } else { Some(v) };
                                    state.save_settings(s);
                                }
                            },
                        }
                    }
                    div { class: "hint", "Runs on the card's worktree (or the project checkout if it has none). {{path}} is replaced by the directory; if you leave it out it's added at the end, so `zed` or `code` alone works too. Blank disables the action." }
                }
                div { class: "section",
                    h3 { "Concurrency" }
                    div { class: "field",
                        label { r#for: "settings-max-runs", "Max concurrent runs" }
                        input {
                            id: "settings-max-runs",
                            r#type: "number",
                            min: "0",
                            step: "1",
                            value: "{max_concurrent_runs}",
                            onchange: {
                                let settings = settings.clone();
                                move |e: Event<FormData>| {
                                    if let Ok(v) = e.value().trim().parse::<u32>() {
                                        let mut s = settings.clone();
                                        s.max_concurrent_runs = v;
                                        state.save_settings(s);
                                    }
                                }
                            },
                        }
                    }
                    div { class: "hint", "Caps concurrently running agent runs and validation checks across all projects (default 5); further runs queue and start automatically. 0 = unlimited. Agent Chat questions, PR-comment triage, and previews aren't counted." }
                }
                }
                div { class: "modal-actions",
                    button { class: "btn primary", onclick: move |_| close_settings(), "Done" }
                }
            }
        }
    }
}

/// A small "ⓘ" glyph next to a field title; the explanatory text shows as a CSS
/// tooltip on hover, keeping the settings panel uncluttered. We render the tip as
/// a child element rather than the native `title` attribute, which macOS's
/// WKWebView (Dioxus desktop) doesn't surface.
#[component]
fn InfoIcon(tip: String) -> Element {
    rsx! {
        span { class: "info-icon",
            "ⓘ"
            span { class: "info-tip", "{tip}" }
        }
    }
}

/// Which tab of the per-project settings modal is showing.
#[derive(Clone, Copy, PartialEq)]
enum ProjectTab {
    Reviews,
    Commands,
}

/// Per-project settings: a tabbed dialog over the PR-review workflow and the
/// worktree commands (setup/validate/run/teardown) configuration.
#[component]
pub fn ProjectSettingsModal() -> Element {
    let Some(pid) = *PROJECT_SETTINGS.read() else {
        return rsx! {};
    };
    let state = use_context::<AppState>();
    let Some(project) = state.projects.read().iter().find(|p| p.id == pid).cloned() else {
        return rsx! {};
    };
    let name = project.name.clone();
    let mut tab = use_signal(|| ProjectTab::Reviews);

    rsx! {
        div { class: "modal-overlay", onclick: move |_| close_project_settings(),
            div {
                class: "modal settings-modal",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        close_project_settings();
                    }
                },
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                h3 { class: "modal-title", "{name} — project settings" }
                div { class: "settings-tabs",
                    button {
                        class: if tab() == ProjectTab::Reviews { "settings-tab active" } else { "settings-tab" },
                        onclick: move |_| tab.set(ProjectTab::Reviews),
                        "Reviews"
                    }
                    button {
                        class: if tab() == ProjectTab::Commands { "settings-tab active" } else { "settings-tab" },
                        onclick: move |_| tab.set(ProjectTab::Commands),
                        "Commands"
                    }
                }
                if tab() == ProjectTab::Reviews {
                    ReviewsTab { pid }
                }
                if tab() == ProjectTab::Commands {
                    CommandsTab { pid }
                }
            }
        }
    }
}

/// The "Reviews" tab: which contributors' PRs to surface on the review board, and
/// the reviewer requested by default when opening this project's own PRs.
#[component]
fn ReviewsTab(pid: Uuid) -> Element {
    let state = use_context::<AppState>();
    let Some(project) = state.projects.read().iter().find(|p| p.id == pid).cloned() else {
        return rsx! {};
    };

    // Populate the collaborator list once on open; the refresh button re-runs it.
    use_hook(move || {
        if state.reviewers.peek().get(&pid).is_none() {
            state.fetch_reviewers(pid);
        }
    });
    let people = state
        .reviewers
        .read()
        .get(&pid)
        .cloned()
        .unwrap_or_default();
    let contributors = project.config.review_contributors.clone();
    let current_reviewer = project.config.reviewer.clone().unwrap_or_default();
    // Collaborators not yet selected — the options offered by the add dropdown.
    let available: Vec<String> = people
        .iter()
        .filter(|l| !contributors.contains(l))
        .cloned()
        .collect();

    rsx! {
        div { class: "section",
            div { class: "field-header",
                div { class: "field-title",
                    h3 { "Contributors to review" }
                    InfoIcon { tip: "Usine polls every 5 minutes for these contributors' open PRs you haven't reviewed." }
                }
                button {
                    class: "btn icon",
                    title: "Refresh collaborators",
                    onclick: move |_| state.fetch_reviewers(pid),
                    "↻"
                }
            }
            // Selected contributors as removable chips.
            div { class: "chips",
                if contributors.is_empty() {
                    span { class: "chips-empty", "None selected yet." }
                }
                for login in contributors.iter() {
                    {
                        let login = login.clone();
                        let project = project.clone();
                        let login_rm = login.clone();
                        rsx! {
                            span { key: "{login}", class: "chip",
                                "{login}"
                                button {
                                    class: "chip-remove",
                                    title: "Remove",
                                    "aria-label": "Remove {login}",
                                    onclick: move |_| {
                                        let mut p = project.clone();
                                        p.config.review_contributors.retain(|l| l != &login_rm);
                                        state.save_project(p);
                                    },
                                    "×"
                                }
                            }
                        }
                    }
                }
            }
            if people.is_empty() {
                // No collaborator list to pick from — plain comma-separated entry.
                div { class: "field",
                    input {
                        r#type: "text",
                        placeholder: "octocat, hubot",
                        value: "{contributors.join(\", \")}",
                        onchange: {
                            let project = project.clone();
                            move |e: Event<FormData>| {
                                let mut p = project.clone();
                                p.config.review_contributors = e
                                    .value()
                                    .split(',')
                                    .map(|s| s.trim().to_string())
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                state.save_project(p);
                            }
                        },
                    }
                }
                div { class: "hint", "No collaborators found — enter GitHub logins separated by commas, or ↻ to fetch them." }
            } else {
                // Pick from the collaborator list to add.
                select {
                    class: "add-select",
                    value: "",
                    onchange: {
                        let project = project.clone();
                        move |e: Event<FormData>| {
                            let login = e.value();
                            if login.is_empty() {
                                return;
                            }
                            let mut p = project.clone();
                            if !p.config.review_contributors.contains(&login) {
                                p.config.review_contributors.push(login);
                                state.save_project(p);
                            }
                        }
                    },
                    option { value: "", disabled: true, selected: true,
                        if available.is_empty() { "All collaborators selected" } else { "+ Add contributor…" }
                    }
                    for login in available.iter() {
                        option { key: "{login}", value: "{login}", "{login}" }
                    }
                }
            }
        }
        div { class: "section",
            div { class: "field-header",
                div { class: "field-title",
                    h3 { "Default reviewer" }
                    InfoIcon { tip: "Pre-selected as the requested reviewer when opening a PR for this project's cards." }
                }
            }
            if people.is_empty() {
                div { class: "field",
                    input {
                        r#type: "text",
                        placeholder: "GitHub username",
                        value: "{current_reviewer}",
                        onchange: {
                            let project = project.clone();
                            move |e: Event<FormData>| {
                                let mut p = project.clone();
                                let v = e.value().trim().to_string();
                                p.config.reviewer = if v.is_empty() { None } else { Some(v) };
                                state.save_project(p);
                            }
                        },
                    }
                }
            } else {
                select {
                    class: "add-select",
                    value: "{current_reviewer}",
                    onchange: {
                        let project = project.clone();
                        move |e: Event<FormData>| {
                            let v = e.value();
                            let mut p = project.clone();
                            p.config.reviewer = if v.is_empty() { None } else { Some(v) };
                            state.save_project(p);
                        }
                    },
                    option {
                        value: "",
                        selected: current_reviewer.is_empty(),
                        "— no default reviewer —"
                    }
                    for login in people.iter() {
                        option {
                            key: "{login}",
                            value: "{login}",
                            selected: *login == current_reviewer,
                            "{login}"
                        }
                    }
                }
            }
        }
        div { class: "section",
            div { class: "field-header",
                div { class: "field-title",
                    h3 { "Pull request base" }
                    InfoIcon { tip: "The branch PRs target and card worktrees fork from. Leave blank to auto-detect." }
                }
            }
            div { class: "field",
                input {
                    r#type: "text",
                    placeholder: "auto ({project.config.base_branch})",
                    value: "{project.config.pinned_base_branch.clone().unwrap_or_default()}",
                    onchange: {
                        let project = project.clone();
                        move |e: Event<FormData>| {
                            let mut p = project.clone();
                            let v = e.value().trim().to_string();
                            p.config.pinned_base_branch = if v.is_empty() { None } else { Some(v) };
                            state.save_project(p);
                        }
                    },
                }
                div { class: "hint", "Leave blank to auto-detect (dev → main → master)." }
            }
        }
    }
}

/// The "Commands" tab: the commands Usine runs inside a card's worktree over
/// its lifecycle — setup, the validation gate, running the app for preview,
/// and teardown — plus the ports whose URLs the preview surfaces. All are
/// stack-agnostic shell commands; setup/teardown are auto-detected from the
/// repo when left blank.
#[component]
fn CommandsTab(pid: Uuid) -> Element {
    let state = use_context::<AppState>();
    let Some(project) = state.projects.read().iter().find(|p| p.id == pid).cloned() else {
        return rsx! {};
    };
    let run_script = project.config.run_script.clone().unwrap_or_default();
    let validate_script = project.config.validate_script.clone().unwrap_or_default();
    let setup_script = project
        .config
        .worktree_setup_script
        .clone()
        .unwrap_or_default();
    let teardown_script = project
        .config
        .worktree_teardown_script
        .clone()
        .unwrap_or_default();

    rsx! {
        div { class: "section",
            div { class: "field-header",
                div { class: "field-title",
                    h3 { "Worktree commands" }
                    InfoIcon { tip: "Commands Usine runs inside a card's isolated worktree — setting it up, validating the work before a PR, and running the app for preview. All run with sh -lc in the worktree." }
                }
            }
            div { class: "field",
                label { "Setup command (optional)" }
                input {
                    r#type: "text",
                    placeholder: "auto-detect setup-worktree.sh",
                    value: "{setup_script}",
                    onchange: {
                        let project = project.clone();
                        move |e: Event<FormData>| {
                            let mut p = project.clone();
                            let v = e.value().trim().to_string();
                            p.config.worktree_setup_script = if v.is_empty() { None } else { Some(v) };
                            state.save_project(p);
                        }
                    },
                }
                div { class: "hint", "Makes a fresh worktree runnable — install deps, isolated DB, per-worktree ports." }
            }
            div { class: "field",
                label { "Validate command (optional)" }
                input {
                    r#type: "text",
                    placeholder: "e.g. make check",
                    value: "{validate_script}",
                    onchange: {
                        let project = project.clone();
                        move |e: Event<FormData>| {
                            let mut p = project.clone();
                            let v = e.value().trim().to_string();
                            p.config.validate_script = if v.is_empty() { None } else { Some(v) };
                            state.save_project(p);
                        }
                    },
                }
                div { class: "hint", "Runs in the card's worktree before a PR is opened. Non-zero exit fails validation and sends the output to the agent to fix." }
            }
            div { class: "field",
                label { "Run command" }
                input {
                    r#type: "text",
                    placeholder: "e.g. ./run-dev.sh",
                    value: "{run_script}",
                    onchange: {
                        let project = project.clone();
                        move |e: Event<FormData>| {
                            let mut p = project.clone();
                            let v = e.value().trim().to_string();
                            p.config.run_script = if v.is_empty() { None } else { Some(v) };
                            state.save_project(p);
                        }
                    },
                }
                div { class: "hint", "Launches the app in the worktree. Required to enable the “Test app” button." }
            }
            div { class: "field",
                label { class: "adopt-choice",
                    input {
                        r#type: "checkbox",
                        checked: project.config.auto_preview,
                        onchange: {
                            let project = project.clone();
                            move |_| {
                                let mut p = project.clone();
                                p.config.auto_preview = !p.config.auto_preview;
                                state.save_project(p);
                            }
                        },
                    }
                    "Start the app automatically during agent runs"
                }
                div { class: "hint", "When off, the agent can still request the app mid-run when it needs to test in it." }
            }
            div { class: "field",
                label { "Preview ports" }
                div { class: "port-list",
                    if project.config.preview_ports.is_empty() {
                        div { class: "hint", "No ports yet — add one to get a clickable URL." }
                    }
                    for (i, port) in project.config.preview_ports.iter().enumerate() {
                        {
                            let label = port.label.clone();
                            let base = port.base;
                            let p_label = project.clone();
                            let p_port = project.clone();
                            let p_rm = project.clone();
                            rsx! {
                                div { key: "{i}", class: "port-row",
                                    input {
                                        class: "port-label",
                                        r#type: "text",
                                        placeholder: "label (e.g. app)",
                                        value: "{label}",
                                        onchange: move |e: Event<FormData>| {
                                            let mut p = p_label.clone();
                                            if let Some(pp) = p.config.preview_ports.get_mut(i) {
                                                pp.label = e.value().trim().to_string();
                                                state.save_project(p);
                                            }
                                        },
                                    }
                                    span { class: "port-colon", ":" }
                                    input {
                                        class: "port-num",
                                        r#type: "number",
                                        min: "1",
                                        max: "65535",
                                        placeholder: "3000",
                                        value: "{base}",
                                        onchange: move |e: Event<FormData>| {
                                            // Ignore an unparseable port; the field snaps back to
                                            // the stored value on the next render.
                                            if let Ok(n) = e.value().trim().parse::<u16>() {
                                                let mut p = p_port.clone();
                                                if let Some(pp) = p.config.preview_ports.get_mut(i) {
                                                    pp.base = n;
                                                    state.save_project(p);
                                                }
                                            }
                                        },
                                    }
                                    button {
                                        class: "btn icon port-remove",
                                        title: "Remove port",
                                        "aria-label": "Remove port",
                                        onclick: move |_| {
                                            let mut p = p_rm.clone();
                                            if i < p.config.preview_ports.len() {
                                                p.config.preview_ports.remove(i);
                                                state.save_project(p);
                                            }
                                        },
                                        "×"
                                    }
                                }
                            }
                        }
                    }
                }
                button {
                    class: "btn add-port",
                    onclick: {
                        let project = project.clone();
                        move |_| {
                            let mut p = project.clone();
                            p.config.preview_ports.push(PreviewPort {
                                label: String::new(),
                                base: 3000,
                            });
                            state.save_project(p);
                        }
                    },
                    "+ Add port"
                }
                div { class: "hint", "Each becomes a button opening localhost:(port + the worktree's port offset). The label is only the button text." }
            }
            div { class: "field",
                label { "Teardown command (optional)" }
                input {
                    r#type: "text",
                    placeholder: "auto-detect teardown-worktree.sh",
                    value: "{teardown_script}",
                    onchange: {
                        let project = project.clone();
                        move |e: Event<FormData>| {
                            let mut p = project.clone();
                            let v = e.value().trim().to_string();
                            p.config.worktree_teardown_script = if v.is_empty() { None } else { Some(v) };
                            state.save_project(p);
                        }
                    },
                }
            }
        }
    }
}
