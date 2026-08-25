//! The PR-review board: reviewing *other* contributors' PRs. A per-project view
//! entered from the sidebar's review icon, with four lanes (To review → Reviewing
//! → Awaiting validation → Reviewed) mirroring the normal card board's layout.
//!
//! Like the card board, the cards here stay simple: they show state and the one
//! action that state affords. Anything needing input — validating the drafted
//! comments — opens the detail panel (`detail::review`) or, for the comments
//! themselves, the diff viewer.
//!
//! The card markup deliberately mirrors `super::card::CardView` element for
//! element (title row + actions menu, meta badges, action row, corner tools), so
//! the two boards read as the same product rather than two that happen to share
//! a stylesheet.

use dioxus::prelude::*;
use usine_core::{PreviewStatus, ReviewColumn, ReviewStatus, ReviewTask};
use uuid::Uuid;

use super::icons::{IconExternal, IconPlay, IconStop};
use crate::state::{AppState, SelectedView};

#[component]
pub fn ReviewBoard() -> Element {
    let state = use_context::<AppState>();
    let SelectedView::Project(project_id) = *state.selected_view.read() else {
        return rsx! {
            div { class: "review-area",
                div { class: "hint", "Select a project to review its PRs." }
            }
        };
    };
    let name = state.project_name(project_id);
    let mut tasks = state.review_tasks_for(project_id);
    // Live Ctrl+F/Cmd+F filter, on PR titles here.
    if let Some(q) = super::search::query() {
        tasks.retain(|t| super::search::matches(&t.pr_title, &q));
    }

    rsx! {
        div { class: "review-area",
            div { class: "review-topbar",
                div { class: "review-topbar-title",
                    button {
                        class: "btn icon",
                        title: "Back to the card board",
                        onclick: move |_| state.exit_review_mode(),
                        "←"
                    }
                    span { class: "review-title", "Reviewing PRs — {name}" }
                }
                button {
                    class: "btn",
                    title: "Check for new PRs to review",
                    onclick: move |_| state.scan_reviews(project_id),
                    "↻ Refresh"
                }
            }
            div { class: "board review-board",
                for col in ReviewColumn::board() {
                    ReviewColumnView {
                        key: "{col:?}",
                        column: col,
                        tasks: tasks.iter().filter(|t| t.column() == col).cloned().collect::<Vec<_>>(),
                    }
                }
            }
        }
    }
}

#[component]
fn ReviewColumnView(column: ReviewColumn, tasks: Vec<ReviewTask>) -> Element {
    let title = column.title();
    let count = tasks.len();

    rsx! {
        div { class: "column",
            div { class: "column-header",
                span { "{title}" }
                span { class: "column-count", "{count}" }
            }
            div { class: "column-body",
                for task in tasks.iter() {
                    ReviewTaskCard { key: "{task.id}", task: task.clone() }
                }
                if count == 0 {
                    div { class: "column-empty", "—" }
                }
            }
        }
    }
}

/// What a PR's preview controls offer, from its live status. Mirrors the card
/// board's `PreviewTools` — a review checkout can run the app just like a card's
/// worktree can, which is often the fastest way to judge a UI change.
#[derive(Clone, PartialEq)]
enum PreviewTools {
    /// The project has no run command configured, or the PR is already published.
    Hidden,
    Idle,
    Starting,
    Running {
        url: Option<String>,
    },
}

/// One PR on the review board. Selecting it opens the detail panel; the buttons
/// that need no extra input dispatch straight to the executor.
#[component]
fn ReviewTaskCard(task: ReviewTask) -> Element {
    let state = use_context::<AppState>();
    let id = task.id;
    let selected = *state.selected_review.read() == Some(id);
    // Same gap the card board covers: `StartReview` / `PublishReview` do their
    // git and forge work *before* the status changes, so between the click and
    // the resulting update the task still reads as idle. Without this the Review
    // button sits there looking unclicked for the whole fetch + worktree build.
    let busy = state.busy.read().contains(&id);

    // The review run is parked in the run queue waiting for a concurrency
    // slot: the task reads `Reviewing`, but nothing is executing — show its
    // place in line instead of a spinner.
    let queued_pos = state.queue_position(id);

    let st = &task.status;
    let status_label = st.status_label();
    let running = st.is_running();
    let failed = st.is_failed();
    let can_start = matches!(st, ReviewStatus::ToReview);
    let can_validate = matches!(st, ReviewStatus::AwaitingValidation { .. });
    // A write agent owns the checkout while the fix runs; once it parks at the
    // gate the checkout is the user's again (and running the app against the fix
    // before pushing it is exactly what the preview is for).
    let fixing = matches!(st, ReviewStatus::Fixing { .. });
    let fix_ready = fix_gate_ready(st);
    let published = matches!(st, ReviewStatus::Reviewed);
    // The PR left GitHub (merged or closed) before the review finished — the
    // only thing left to do is acknowledge it.
    let retired = matches!(st, ReviewStatus::MergedWithoutReview { .. });

    // A published or retired task's checkout is torn down, so there's nothing
    // left to run or open; everything before that has (or can get) one on demand.
    let has_checkout = !published && !retired;
    let run_configured = state
        .projects
        .read()
        .iter()
        .find(|p| p.id == task.project_id)
        .map(|p| p.config.run_script.is_some())
        .unwrap_or(false);
    let preview = if has_checkout && run_configured && !fixing {
        match state.preview(id) {
            Some((PreviewStatus::SettingUp, _)) => PreviewTools::Starting,
            Some((PreviewStatus::Running, urls)) => PreviewTools::Running {
                url: urls.first().map(|u| u.url.clone()),
            },
            _ => PreviewTools::Idle,
        }
    } else {
        PreviewTools::Hidden
    };

    let card_class = if failed {
        "card review-card failed"
    } else if selected {
        "card review-card selected"
    } else {
        "card review-card"
    };
    let card_class = if busy {
        format!("{card_class} is-busy")
    } else {
        card_class.to_string()
    };
    let menu_title = task.pr_title.clone();
    // The board has no steering box of its own — starting (or retrying) from a card
    // reuses whatever steering the detail panel last sent, so the two agree.
    let start_guidance = task.guidance.clone();
    let retry_guidance = task.guidance.clone();
    let pr_number = task.pr_number;
    let checks = task.checks;
    let conflicting = task.mergeable.is_conflicting();

    rsx! {
        div {
            class: "{card_class}",
            tabindex: "0",
            "role": "button",
            onclick: move |_| state.select_review(Some(id)),
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Enter || e.key() == Key::Character(" ".to_string()) {
                    e.prevent_default();
                    state.select_review(Some(id));
                }
            },
            div { class: "card-top",
                // Keep keyboard activation of the nested buttons working: stop their
                // keydowns from bubbling to the card handler above (which prevents the
                // default action and would otherwise cancel a button's Enter activation).
                onkeydown: move |e| e.stop_propagation(),
                div { class: "card-title",
                    a {
                        class: "review-pr-num",
                        href: "{task.url}",
                        target: "_blank",
                        rel: "noreferrer",
                        title: "Open pull request on GitHub",
                        // Don't let a click on the link also select the card
                        // (its Enter activation is already shielded by the
                        // card-top keydown handler above).
                        onclick: move |e| e.stop_propagation(),
                        "#{task.pr_number}"
                    }
                    "{task.pr_title}"
                }
                div { class: "card-menu-wrap",
                    button {
                        class: "card-menu-btn",
                        title: "PR actions",
                        "aria-label": "PR actions",
                        onclick: move |e| {
                            e.stop_propagation();
                            let c = e.client_coordinates();
                            super::cardmenu::open_card_menu(super::cardmenu::CardMenuRequest {
                                kind: super::cardmenu::MenuKind::Review {
                                    pr_number,
                                    // A torn-down checkout can't be opened or run,
                                    // but it can still be re-fetched for a diff.
                                    has_checkout,
                                },
                                target_id: id,
                                title: menu_title.clone(),
                                x: c.x,
                                y: c.y,
                            });
                        },
                        svg {
                            width: "16",
                            height: "16",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            polyline { points: "6 9 12 15 18 9" }
                        }
                    }
                }
            }
            div { class: "card-meta",
                span { class: "review-author", "@{task.author}" }
                if (running || busy) && queued_pos.is_none() {
                    span { class: "spinner" }
                }
                if let Some(n) = queued_pos {
                    span {
                        class: "badge queued",
                        title: "Waiting for a free run slot (see max concurrent runs in Settings)",
                        "queued #{n}"
                    }
                } else {
                    span { class: "badge status", "{status_label}" }
                }
                // CI and mergeability are worth knowing *before* spending a review
                // pass — a red or conflicting PR usually isn't ready for one.
                if checks.is_reportable() {
                    span {
                        class: "badge {checks.css_class()}",
                        title: "{checks.label()}",
                        "{checks.glyph()} CI"
                    }
                }
                if conflicting {
                    span { class: "badge conflict", title: "Conflicts with the base branch", "conflicts" }
                }
                if !task.cost.is_zero() {
                    span { class: "badge cost", "{task.cost}" }
                }
            }
            div { class: "card-actions",
                // Shield the action buttons' keydowns from the card handler too.
                onkeydown: move |e| e.stop_propagation(),
                if can_start {
                    button {
                        class: "btn primary",
                        title: "Check out the PR and run the review agent",
                        onclick: move |e| {
                            e.stop_propagation();
                            state.start_review(id, start_guidance.clone());
                        },
                        "Review"
                    }
                    button {
                        class: "btn",
                        title: "Approve this PR without running the review agent",
                        onclick: move |e| {
                            e.stop_propagation();
                            super::confirm_approve_review(state, id, pr_number);
                        },
                        "Approve"
                    }
                    button {
                        class: "btn subtle",
                        title: "Dismiss this PR from the review board",
                        onclick: move |e| {
                            e.stop_propagation();
                            super::confirm_dismiss_review(id, pr_number);
                        },
                        "Dismiss"
                    }
                }
                if can_validate {
                    button {
                        class: "btn primary",
                        title: "Check the drafted comments against the diff",
                        onclick: move |e| {
                            e.stop_propagation();
                            state.select_review(Some(id));
                            super::diffdialog::open_review_diff(id);
                        },
                        "Validate comments"
                    }
                }
                if fix_ready {
                    button {
                        class: "btn primary",
                        title: "Read the committed fix before it's pushed",
                        onclick: move |e| {
                            e.stop_propagation();
                            state.select_review(Some(id));
                            super::diffdialog::open_review_diff(id);
                        },
                        "Review the fix"
                    }
                }
                if published {
                    span { class: "reviewed-tag", "✓ published" }
                }
                if retired {
                    button {
                        class: "btn subtle",
                        title: "Dismiss this PR from the review board",
                        onclick: move |e| {
                            e.stop_propagation();
                            super::confirm_dismiss_review(id, pr_number);
                        },
                        "Dismiss"
                    }
                }
                if failed && !fix_ready && !fix_faulted(st) {
                    button {
                        class: "btn",
                        title: "Retry the review",
                        onclick: move |e| {
                            e.stop_propagation();
                            state.start_review(id, retry_guidance.clone());
                        },
                        "Retry"
                    }
                }
                // Pushed to the bottom-right corner, away from the state's primary action.
                if preview != PreviewTools::Hidden {
                    div { class: "card-tools",
                        ReviewPreviewControls { review_id: id, tools: preview }
                    }
                }
            }
        }
    }
}

/// The PR's inline preview controls: run the contributor's branch straight from
/// its review checkout. Identical in shape to the card board's controls, pointed
/// at the review commands.
#[component]
fn ReviewPreviewControls(review_id: Uuid, tools: PreviewTools) -> Element {
    let state = use_context::<AppState>();

    match tools {
        PreviewTools::Hidden => rsx! {},
        PreviewTools::Idle => rsx! {
            button {
                class: "card-icon-btn",
                title: "Run this PR",
                "aria-label": "Run this PR",
                onclick: move |e| {
                    e.stop_propagation();
                    state.start_review_preview(review_id);
                },
                IconPlay {}
            }
        },
        PreviewTools::Starting => rsx! {
            span {
                class: "card-icon-btn is-busy",
                title: "Starting app…",
                "aria-label": "Starting app",
                span { class: "spinner" }
            }
            button {
                class: "card-icon-btn",
                title: "Stop app",
                "aria-label": "Stop app",
                onclick: move |e| {
                    e.stop_propagation();
                    state.stop_review_preview(review_id);
                },
                IconStop {}
            }
        },
        PreviewTools::Running { url } => rsx! {
            if let Some(url) = url {
                a {
                    class: "card-icon-btn",
                    href: "{url}",
                    target: "_blank",
                    rel: "noreferrer",
                    title: "Open app",
                    "aria-label": "Open app",
                    onclick: move |e| e.stop_propagation(),
                    IconExternal {}
                }
            }
            button {
                class: "card-icon-btn",
                title: "Stop app",
                "aria-label": "Stop app",
                onclick: move |e| {
                    e.stop_propagation();
                    state.stop_review_preview(review_id);
                },
                IconStop {}
            }
        },
    }
}

/// Whether a committed fix is waiting at the push gate — including one whose
/// push was rejected, which is retried from the same place.
fn fix_gate_ready(status: &ReviewStatus) -> bool {
    match status {
        ReviewStatus::FixReady { .. } => true,
        ReviewStatus::Failed { previous, .. } => fix_gate_ready(previous),
        _ => false,
    }
}

/// Whether a fault came out of the fix flow rather than the review pass. Such a
/// task's review is already on GitHub, so the board must not offer to re-run it
/// (the panel offers the gate's actions instead).
fn fix_faulted(status: &ReviewStatus) -> bool {
    match status {
        ReviewStatus::Failed { previous, .. } => fix_faulted(previous),
        other => other.fix_state().is_some(),
    }
}
