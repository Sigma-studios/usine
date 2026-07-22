use dioxus::prelude::*;
use usine_core::{Card, CardState, DesignSub, ExecutorCommand, PrReviewSub, PreviewStatus};
use uuid::Uuid;

use super::icons::{IconExternal, IconPlay, IconStop};
use crate::state::AppState;

/// What the card's preview controls offer, from its live preview status.
#[derive(Clone, PartialEq)]
enum PreviewTools {
    /// Not eligible: no committed worktree yet, or no run command configured.
    Hidden,
    /// Stopped, never started, or failed — offer to launch it.
    Idle,
    /// Running the setup script; nothing to click yet.
    Starting,
    /// Up — offer to open it (if it reported a URL) and to stop it.
    Running { url: Option<String> },
}

/// One card on the board, with the action buttons appropriate to its state.
/// Buttons that need extra input (Answer, Review plan, Create PR, Select fixes)
/// just open the detail panel; the rest dispatch a command directly.
#[component]
pub fn CardView(card: Card) -> Element {
    let state = use_context::<AppState>();
    let id = card.id;
    let selected = *state.selected_card.read() == Some(id);

    // A lifecycle command is in flight for this card. Its git/forge work runs
    // before the transition, so the card's own state still reads as idle —
    // without this the Approve button would sit there looking unclicked for the
    // whole worktree build, which is what invited the duplicate clicks the
    // executor now has to drop.
    let busy = state.busy.read().contains(&id);

    let st = &card.state;
    let running = st.is_running();
    let failed = st.is_failed();
    let recover_label = match st {
        CardState::Failed { message, .. } if message.starts_with("Interrupted") => "Resume",
        _ => "Retry",
    };
    let needs_answer = st.intervention().is_some();

    let can_start = matches!(st, CardState::StartingBlock);
    let awaiting_approval = matches!(st, CardState::Designing(DesignSub::AwaitingApproval { .. }));
    // Approval is only offered once the agent has no outstanding questions —
    // until then the way forward is to open the panel and answer them. Mirrors
    // the guard in the detail panel (and the server-side check in approve_plan).
    let can_approve = matches!(
        st,
        CardState::Designing(DesignSub::AwaitingApproval { plan })
            if usine_core::parse_plan(plan).1.is_empty()
    );
    let awaiting_review = matches!(st, CardState::AwaitingReview(_));
    // An investigation finished: its conclusion is the deliverable — the primary
    // action is to read it (in the detail panel, where follow-up/convert live).
    let concluded = matches!(st, CardState::Concluded { .. });
    // Nothing to triage until someone has actually left comments — the background
    // poll keeps the count fresh while the card sits in `Idle`. Keyed off the
    // *total*, matching the detail panel's `can_triage`: a comment from a
    // non-assigned reviewer is still worth reading, and gating this on the
    // narrower `reviewer_comment_count` left the panel offering a triage the card
    // itself hid. Which count lights the dock badge is a separate question — that
    // one stays the assigned reviewer's job (see `Card::needs_attention`).
    let can_read = matches!(st, CardState::PrReview(PrReviewSub::Idle)) && card.comment_count > 0;
    let selecting = matches!(st, CardState::PrReview(PrReviewSub::SelectingFixes { .. }));
    let can_merge = matches!(st, CardState::ReadyToMerge);
    // Comments can land after triage already carried the card to the merge
    // gate; offer another pass alongside Merge — but only while some thread
    // still awaits an answer (the poll keeps the count fresh here too).
    let can_reevaluate = can_merge && card.unanswered_count > 0;
    // A ready-to-merge card whose PR is still a draft must be marked ready before
    // GitHub will merge it — offer that instead of a merge that would fail.
    let pr_is_draft = card
        .pr
        .as_ref()
        .map(|p| p.state == "draft")
        .unwrap_or(false);
    // Surface the PR from the board itself — until now only the detail panel
    // showed it, while the review board already prefixes its cards with #N.
    let pr_link = card.pr.as_ref().map(|p| (p.number, p.url.clone()));
    // The worktree holds committed, reviewable work from just-implemented through
    // PR review until merge. "Show diff" lives in the card actions menu; the
    // preview controls sit inline on the card and need a run command configured.
    // Key off the *effective* state so a faulted run (wrapped as `Failed`) still
    // exposes the diff and the preview's stop control — its worktree is intact.
    let reviewable = matches!(
        st.effective(),
        CardState::AwaitingReview(_) | CardState::PrReview(_) | CardState::ReadyToMerge
    );
    // Previews also show while implementing: the executor brings the app up
    // alongside every write run (so the agent can test against it), and that
    // running server needs its status and stop control visible from the start —
    // not only once the card reaches review.
    let previewable = reviewable || matches!(st.effective(), CardState::Implementing(_));
    let run_configured = state
        .projects
        .read()
        .iter()
        .find(|p| p.id == card.project_id)
        .map(|p| p.config.run_script.is_some())
        .unwrap_or(false);
    let can_diff = reviewable;
    let preview = if previewable && run_configured {
        match state.preview(id) {
            Some((PreviewStatus::SettingUp, _)) => PreviewTools::Starting,
            Some((PreviewStatus::Running, urls)) => PreviewTools::Running {
                url: urls.first().map(|u| u.url.clone()),
            },
            // Stopped, failed, or never started: all offer the same next step.
            _ => PreviewTools::Idle,
        }
    } else {
        PreviewTools::Hidden
    };

    let card_class = if failed {
        "card failed"
    } else if selected {
        "card selected"
    } else {
        "card"
    };
    let card_class = if busy {
        format!("{card_class} is-busy")
    } else {
        card_class.to_string()
    };
    let status = card.state.status_label();
    let title = if card.title.trim().is_empty() {
        "Untitled".to_string()
    } else {
        card.title.clone()
    };
    let menu_title = title.clone();
    let is_done = matches!(st, CardState::Done);

    rsx! {
        div {
            class: "{card_class}",
            tabindex: "0",
            "role": "button",
            onclick: move |_| state.select_card(Some(id)),
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Enter || e.key() == Key::Character(" ".to_string()) {
                    e.prevent_default();
                    state.select_card(Some(id));
                }
            },
            div { class: "card-top",
                // Keep keyboard activation of the nested buttons working: stop their
                // keydowns from bubbling to the card handler above (which prevents the
                // default action and would otherwise cancel a button's Enter activation).
                onkeydown: move |e| e.stop_propagation(),
                div { class: "card-title", "{title}" }
                div { class: "card-menu-wrap",
                    button {
                        class: "card-menu-btn",
                        title: "Card actions",
                        "aria-label": "Card actions",
                        onclick: move |e| {
                            e.stop_propagation();
                            let c = e.client_coordinates();
                            super::cardmenu::open_card_menu(super::cardmenu::CardMenuRequest {
                                // Hide actions that don't apply to the current state.
                                kind: super::cardmenu::MenuKind::Card {
                                    can_reset: !can_start,
                                    can_done: !is_done,
                                    can_diff,
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
                // `busy` covers the gap the card's own state can't: the command
                // is working, but hasn't transitioned the card yet.
                if running || busy {
                    span { class: "spinner" }
                }
                if needs_answer {
                    span { class: "badge intervention", "needs answer" }
                } else {
                    span { class: "badge status", "{status}" }
                }
                if let Some((number, url)) = pr_link {
                    a {
                        class: "badge pr-link",
                        href: "{url}",
                        target: "_blank",
                        rel: "noreferrer",
                        title: "Open pull request on GitHub",
                        // Don't let a click on the link also select the card, and
                        // shield its Enter activation from the card's onkeydown
                        // (which prevents the default action).
                        onclick: move |e| e.stop_propagation(),
                        onkeydown: move |e: KeyboardEvent| e.stop_propagation(),
                        "#{number}"
                    }
                }
                // The PR's CI state, once it has one and any check reported —
                // a red build is worth seeing from the board, before reaching
                // for Merge.
                if card.pr.is_some() && card.checks.is_reportable() {
                    span {
                        class: "badge {card.checks.css_class()}",
                        title: "{card.checks.label()}",
                        "{card.checks.glyph()} CI"
                    }
                }
            }
            div { class: "card-actions",
                // Shield the action buttons' keydowns from the card handler too.
                onkeydown: move |e| e.stop_propagation(),
                if can_start {
                    button {
                        class: "btn primary",
                        onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::Start { card_id: id }); },
                        "Start"
                    }
                }
                if needs_answer {
                    button {
                        class: "btn primary",
                        onclick: move |e| { e.stop_propagation(); state.select_card(Some(id)); },
                        "Answer"
                    }
                }
                if awaiting_approval {
                    if can_approve {
                        button {
                            class: "btn primary",
                            onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::ApprovePlan { card_id: id }); },
                            "Approve"
                        }
                    }
                    button {
                        class: "btn",
                        onclick: move |e| { e.stop_propagation(); state.select_card(Some(id)); },
                        "Review"
                    }
                }
                if awaiting_review {
                    button {
                        class: "btn primary",
                        onclick: move |e| { e.stop_propagation(); state.select_card(Some(id)); },
                        "Review"
                    }
                }
                if concluded {
                    button {
                        class: "btn primary",
                        onclick: move |e| { e.stop_propagation(); state.select_card(Some(id)); },
                        "Read conclusion"
                    }
                }
                if can_read {
                    button {
                        class: "btn primary",
                        onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::FetchComments { card_id: id }); },
                        "Review comments"
                    }
                }
                if selecting {
                    button {
                        class: "btn primary",
                        onclick: move |e| { e.stop_propagation(); state.select_card(Some(id)); },
                        "Select fixes"
                    }
                }
                if can_reevaluate {
                    button {
                        class: "btn",
                        onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::FetchComments { card_id: id }); },
                        "Review comments"
                    }
                }
                if can_merge && pr_is_draft {
                    button {
                        class: "btn primary",
                        onclick: move |e| {
                            e.stop_propagation();
                            state.send(ExecutorCommand::MarkPrReady { card_id: id });
                        },
                        "Mark ready"
                    }
                }
                if can_merge && !pr_is_draft {
                    button {
                        class: "btn success",
                        onclick: move |e| {
                            e.stop_propagation();
                            super::confirm_then_send(
                                state,
                                "Merge pull request",
                                "Merge this pull request into the base branch on GitHub? This can't be undone.".to_string(),
                                "Merge",
                                ExecutorCommand::Merge { card_id: id, delete_branch: true, force: false },
                            );
                        },
                        "Merge"
                    }
                }
                if failed {
                    button {
                        class: "btn",
                        onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::Retry { card_id: id }); },
                        "{recover_label}"
                    }
                }
                // Pushed to the bottom-right corner, away from the state's primary action.
                if preview != PreviewTools::Hidden {
                    div { class: "card-tools",
                        PreviewControls { card_id: id, tools: preview }
                    }
                }
            }
        }
    }
}

/// The card's inline preview controls: launch the project's app straight from the
/// card's worktree, watch it come up, then open or stop it. Every click is
/// stopped from bubbling — the card itself is a button that selects it.
#[component]
fn PreviewControls(card_id: Uuid, tools: PreviewTools) -> Element {
    let state = use_context::<AppState>();

    match tools {
        PreviewTools::Hidden => rsx! {},
        PreviewTools::Idle => rsx! {
            button {
                class: "card-icon-btn",
                title: "Run app",
                "aria-label": "Run app",
                onclick: move |e| {
                    e.stop_propagation();
                    state.send(ExecutorCommand::StartPreview { card_id });
                },
                IconPlay {}
            }
        },
        // The setup script is running. Show a spinner, but keep a stop control so a
        // hung setup (e.g. a slow `docker compose pull`) can still be interrupted —
        // `StopPreview` reaps the setup process too.
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
                    state.send(ExecutorCommand::StopPreview { card_id });
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
                    state.send(ExecutorCommand::StopPreview { card_id });
                },
                IconStop {}
            }
        },
    }
}
