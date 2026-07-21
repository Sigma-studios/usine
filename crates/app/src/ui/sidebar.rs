use dioxus::prelude::*;

use crate::state::{AppState, SelectedView};

#[component]
pub fn Sidebar() -> Element {
    let state = use_context::<AppState>();
    let view = *state.selected_view.read();
    let projects = state.projects.read().clone();

    let home_class = if matches!(view, SelectedView::Global) {
        "nav-item home active"
    } else {
        "nav-item home"
    };

    rsx! {
        div { class: "sidebar",
            div { class: "sidebar-brand",
                img { class: "brand-logo", src: crate::LOGO_URI.as_str(), alt: "Usine" }
                span { class: "brand-name", "Usine" }
            }
            div { class: "sidebar-top",
                button {
                    class: "{home_class}",
                    title: "Global view",
                    onclick: move |_| state.select_view(SelectedView::Global),
                    "Home"
                }
                button {
                    class: "cog",
                    title: "Settings",
                    "aria-label": "Settings",
                    onclick: move |_| super::settings::open_settings(),
                    svg {
                        width: "16",
                        height: "16",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "2",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        circle { cx: "12", cy: "12", r: "3" }
                        path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" }
                    }
                }
            }
            div { class: "sidebar-projects",
                for project in projects.iter() {
                    {
                        let pid = project.id;
                        let name = project.name.clone();
                        let name_for_msg = name.clone();
                        let active = matches!(view, SelectedView::Project(id) if id == pid);
                        let class = if active { "nav-item active" } else { "nav-item" };
                        let path = project.path.display().to_string();
                        let review_count = state.project_review_count(pid);
                        rsx! {
                            div {
                                key: "{pid}",
                                class: "{class}",
                                title: "{path}",
                                onclick: move |_| state.select_view(SelectedView::Project(pid)),
                                span { class: "proj-dot" }
                                span { class: "proj-name", "{name}" }
                                button {
                                    // Always visible (not hover-only) so completed
                                    // reviews stay reachable even with nothing pending.
                                    class: "proj-action proj-review",
                                    title: if review_count > 0 { "PRs awaiting your review" } else { "Review PRs" },
                                    "aria-label": "Review PRs",
                                    onclick: move |e| {
                                        e.stop_propagation();
                                        state.enter_review_mode(pid);
                                    },
                                    svg {
                                        width: "14",
                                        height: "14",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        path { d: "M1 12s4-7 11-7 11 7 11 7-4 7-11 7-11-7-11-7z" }
                                        circle { cx: "12", cy: "12", r: "3" }
                                    }
                                    // Pinned to the eye's top-right corner: the count is
                                    // about reviews, so it rides the icon that opens them.
                                    if review_count > 0 {
                                        span { class: "proj-badge", "{review_count}" }
                                    }
                                }
                                button {
                                    class: "proj-action",
                                    title: "Project settings",
                                    "aria-label": "Project settings",
                                    onclick: move |e| {
                                        e.stop_propagation();
                                        super::settings::open_project_settings(pid);
                                    },
                                    svg {
                                        width: "14",
                                        height: "14",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        circle { cx: "12", cy: "12", r: "3" }
                                        path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 0 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" }
                                    }
                                }
                                button {
                                    class: "proj-trash",
                                    title: "Remove project",
                                    "aria-label": "Remove project",
                                    onclick: move |e| {
                                        e.stop_propagation();
                                        super::confirm::request_confirm(super::confirm::ConfirmRequest {
                                            title: "Remove project".into(),
                                            message: format!("Remove “{name_for_msg}” and its cards from Usine? Your files and git history are not touched."),
                                            confirm_label: "Remove".into(),
                                            danger: true,
                                            action: super::confirm::ConfirmAction::DeleteProject(pid),
                                        });
                                    },
                                    svg {
                                        width: "13",
                                        height: "13",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        polyline { points: "3 6 5 6 21 6" }
                                        path { d: "M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" }
                                    }
                                }
                            }
                        }
                    }
                }
                button {
                    class: "add-project",
                    onclick: move |_| {
                        spawn(async move {
                            if let Some(handle) = rfd::AsyncFileDialog::new().pick_folder().await {
                                state.add_project(handle.path().to_path_buf());
                            }
                        });
                    },
                    "+ Project"
                }
            }
            div { class: "sidebar-bottom",
                if crate::state::demo_mode() {
                    div { class: "demo-badge", "● Demo mode" }
                }
            }
        }
    }
}
