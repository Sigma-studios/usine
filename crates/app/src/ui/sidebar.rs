use dioxus::prelude::*;

use crate::state::{AppState, SelectedView};
use crate::ui::{Panel, PanelResizer};

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
                        let muted = project.config.notifications_muted;
                        let class = match (active, muted) {
                            (true, true) => "nav-item active muted",
                            (true, false) => "nav-item active",
                            (false, true) => "nav-item muted",
                            (false, false) => "nav-item",
                        };
                        let path = project.path.display().to_string();
                        // A muted project raises no attention signals: zeroing
                        // the counts leaves an idle dot, no count and no eye badge.
                        let (review_count, (attention_count, urgent_count)) = if muted {
                            (0, (0, 0))
                        } else {
                            (state.project_review_count(pid), state.project_attention_counts(pid))
                        };
                        // Three-state health dot: red = failed / agent question,
                        // accent = waiting on you, dim = idle.
                        let dot_class = if urgent_count > 0 {
                            "proj-dot urgent"
                        } else if attention_count > 0 {
                            "proj-dot"
                        } else {
                            "proj-dot idle"
                        };
                        rsx! {
                            div {
                                key: "{pid}",
                                class: "{class}",
                                title: "{path}",
                                onclick: move |_| state.select_view(SelectedView::Project(pid)),
                                span { class: "{dot_class}" }
                                span { class: "proj-name", "{name}" }
                                // Indicator only: a click falls through to the row.
                                if muted {
                                    span {
                                        class: "proj-mute",
                                        title: "Notifications muted",
                                        "aria-label": "Notifications muted",
                                        svg {
                                            width: "13",
                                            height: "13",
                                            view_box: "0 0 24 24",
                                            fill: "none",
                                            stroke: "currentColor",
                                            stroke_width: "2",
                                            stroke_linecap: "round",
                                            stroke_linejoin: "round",
                                            path { d: "M13.73 21a2 2 0 0 1-3.46 0" }
                                            path { d: "M18.63 13A17.89 17.89 0 0 1 18 8" }
                                            path { d: "M6.26 6.26A5.86 5.86 0 0 0 6 8c0 7-3 9-3 9h14" }
                                            path { d: "M18 8a6 6 0 0 0-9.33-5" }
                                            line { x1: "1", y1: "1", x2: "23", y2: "23" }
                                        }
                                    }
                                }
                                if attention_count > 0 {
                                    span {
                                        class: "proj-count",
                                        title: if attention_count == 1 { "1 card waiting on you".to_string() } else { format!("{attention_count} cards waiting on you") },
                                        "{attention_count}"
                                    }
                                }
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
            PanelResizer { panel: Panel::Sidebar }
        }
    }
}
