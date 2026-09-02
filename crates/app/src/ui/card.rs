use dioxus::prelude::*;
use usine_core::{
    Card, CardKind, CardState, CheckStatus, DesignSub, ExecutorCommand, PrReviewSub, PreviewStatus,
    ReviewSub,
};
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

    // The card's launch is parked in the run queue waiting for a concurrency
    // slot: the card sits in a running state, but nothing is executing — show
    // its place in line instead of a spinner.
    let queued_pos = state.queue_position(id);

    let running = card.state.is_running();
    let failed = card.state.is_failed();
    // The buttons dispatch on the state seen THROUGH a question run: asking
    // something used to strip the card of every action (and reflow the column)
    // for the duration. They stay put and read disabled instead — `frozen`
    // adds `.is-busy`, whose `.btn` rule is exactly the "a command wouldn't
    // land right now" treatment. A fault keeps its Retry-only board treatment:
    // a faulted card is acted on in the panel.
    let frozen = matches!(card.state, CardState::Answering { .. });
    let st = match &card.state {
        CardState::Answering { previous, .. } => previous.effective(),
        s => s,
    };
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
    // Only the parked review sub-states get a board button, labelled for what
    // opening the panel actually offers there; the running ones just spin.
    // Each carries the panel anchor it promises to land on: these buttons only
    // open the card, which clicking it already does, so they must at least take
    // you to the section they name.
    let review_action = match st {
        CardState::AwaitingReview(ReviewSub::ReadyForReview) => Some(("Review", "pr-create")),
        CardState::AwaitingReview(ReviewSub::SelectingFixes { .. }) => {
            Some(("Select fixes", "fix-selection"))
        }
        // The panel offers three ways forward (fix, skip, re-run), not just a
        // fix — say "Validation failed" and let the panel present them.
        CardState::AwaitingReview(ReviewSub::ValidationFailed { .. }) => {
            Some(("Validation failed", "pr-create"))
        }
        CardState::AwaitingReview(ReviewSub::ReadyForPr) => Some(("Create PR", "pr-create")),
        _ => None,
    };
    // Validation gave up — urgent tier (`needs_urgent_attention`), so the badge
    // must read broken, not routine.
    let validation_failed = matches!(
        st,
        CardState::AwaitingReview(ReviewSub::ValidationFailed { .. })
    );
    // A PR closed without merging is closer to "something went wrong" than a
    // routine hand-off, so its badge borrows the intervention styling; the
    // merged-without-review variant keeps the neutral status badge.
    let externally_closed = matches!(st, CardState::MergedWithoutReview { merged: false });
    let is_investigation = card.config.kind == CardKind::Investigation;
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
    // ...and an unread review *body* (a body-only review's summary text) counts
    // like a comment: it's feedback the triage pass can read.
    let can_read =
        matches!(st, CardState::PrReview(PrReviewSub::Idle)) && card.has_triageable_feedback();
    let selecting = matches!(st, CardState::PrReview(PrReviewSub::SelectingFixes { .. }));
    let can_merge = matches!(st, CardState::ReadyToMerge);
    // The merge button is a promise the executor must keep, so don't show it
    // when the merge would be refused: a conflicting PR gets a resolve run, a
    // red build gets a fix run (both self-guarding — they re-read the forge and
    // no-op with a toast if the problem healed since), and a still-running
    // build gets nothing at all — the "• CI" badge explains the wait and the
    // poll brings the button back on its own. The detail panel keeps the
    // explicit "Merge anyway" override for the states that can use one.
    // Conflicts win over failing checks: the resolve pushes a merge commit
    // that re-runs CI anyway.
    let conflicting = can_merge && card.mergeable.is_conflicting();
    let ci_failing = can_merge && !conflicting && card.checks == CheckStatus::Failing;
    let ci_pending = can_merge && !conflicting && card.checks == CheckStatus::Pending;
    // Comments can land after triage already carried the card to the merge
    // gate; offer another pass alongside Merge — but only while some thread
    // still awaits an answer (the poll keeps the count fresh here too), or a
    // review body landed unread.
    let can_reevaluate =
        can_merge && (card.unanswered_count > 0 || !card.pending_review_bodies().is_empty());
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
    // Who approved the PR, for the badge tooltip. Empty = no approval landed,
    // no badge.
    let approved_by = card.approved_by().join(", ");
    // Reviewers whose review *body* awaits reading — a body-only review (e.g.
    // a bot's report) leaves no comment count, so this is its board-level
    // visibility. Informational: no state change rides on it. Only shown while
    // the card is at the PR or merge gate — the states whose panels offer a way
    // to read and dismiss it; past the merge (Done) it would nag forever with
    // no dismissal path.
    let body_actionable = matches!(st, CardState::PrReview(_) | CardState::ReadyToMerge);
    let commented_by = if body_actionable {
        card.pending_review_bodies()
            .iter()
            .map(|r| r.author.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        String::new()
    };
    // The worktree holds committed, reviewable work from just-implemented through
    // PR review until merge. "Show diff" lives in the card actions menu; the
    // preview controls sit inline on the card and need a run command configured.
    // Key off the *effective* state so a faulted run (wrapped as `Failed`) still
    // exposes the diff and the preview's stop control — its worktree is intact.
    let reviewable = matches!(
        st.effective(),
        CardState::AwaitingReview(_) | CardState::PrReview(_) | CardState::ReadyToMerge
    );
    // The executor brings the app up alongside every write run, but that
    // preview is the *agent's* tool for testing its own work — surfacing Stop
    // there would let the user yank the app out from under a mid-run agent.
    // Controls only appear once the pipeline parks and it's the user's turn to
    // test (previews are light-stopped at every automated park, so they'll read
    // Idle and offer a warm restart).
    let previewable = reviewable && !running;
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
    // Appended last so the blocked tint wins the border on a failed card, while
    // `.card.selected`'s ring still shows.
    let card_class = if card.blocked {
        format!("{card_class} blocked")
    } else {
        card_class
    };
    // Kept in step with the marker by `Card::set_blocked`: a note only
    // exists while the card is blocked.
    let blocked_note = card.blocked_note.clone();
    // From the card, not the state alone — "ready to merge" must not sit on a
    // card whose button says "Resolve conflicts". While a lifecycle command is
    // in flight the card is doing something its state can't show yet, so say so
    // and keep the real state on the tooltip.
    let status = if busy && !running {
        "working…"
    } else {
        card.status_label()
    };
    let status_title = card.status_label();
    let title = if card.title.trim().is_empty() {
        "Untitled".to_string()
    } else {
        card.title.clone()
    };
    let menu_title = title.clone();
    let menu_note = blocked_note.clone();
    let is_done = matches!(st, CardState::Done);
    // Non-done cards keep the open-project fallback (useful even with no
    // worktree yet); a done card whose worktree was reaped has nothing to open.
    let can_open = !is_done
        || card
            .worktree_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false);

    rsx! {
        div {
            class: "{card_class}",
            tabindex: "0",
            "role": "button",
            // Clicking the open card closes its panel — the same toggle every
            // other "selected" affordance in the app has, and the board's only
            // way back to full width besides the panel's ×.
            onclick: move |_| state.select_card((!selected).then_some(id)),
            onkeydown: move |e: KeyboardEvent| {
                if e.key() == Key::Enter || e.key() == Key::Character(" ".to_string()) {
                    e.prevent_default();
                    state.select_card((!selected).then_some(id));
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
                                    can_open,
                                    blocked: card.blocked,
                                    blocked_note: menu_note.clone(),
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
                if (running || busy) && queued_pos.is_none() {
                    span { class: "spinner" }
                }
                if is_investigation {
                    span { class: "badge kind", "Investigation" }
                }
                // Sits beside the status badge rather than replacing it: the
                // marker is an annotation, and the real state still matters.
                if card.blocked {
                    span {
                        class: "badge blocked",
                        title: "Marked blocked — this card doesn't count toward the attention badge",
                        "blocked"
                    }
                }
                // Beside the status badge, not instead of it: a queued card
                // still has a state, and losing it left the card saying only
                // where it stood in line.
                if let Some(n) = queued_pos {
                    span {
                        class: "badge queued",
                        title: "Waiting for a free run slot (see max concurrent runs in Settings)",
                        "queued #{n}"
                    }
                }
                if needs_answer {
                    span { class: "badge intervention", "needs answer" }
                } else if externally_closed || validation_failed {
                    span { class: "badge intervention", title: "{status_title}", "{status}" }
                } else if concluded {
                    span { class: "badge concluded", title: "{status_title}", "{status}" }
                } else {
                    span { class: "badge status", title: "{status_title}", "{status}" }
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
                // A standing approval on the PR — the fact that carries a card
                // to the merge gate, so it's worth seeing from the board.
                if card.pr.is_some() && !approved_by.is_empty() {
                    span {
                        class: "badge approved",
                        title: "Approved by {approved_by}",
                        "✓ approved"
                    }
                }
                // An unread review body — feedback with no inline comments to
                // count, worth seeing from the board until read or triaged.
                if card.pr.is_some() && !commented_by.is_empty() {
                    span {
                        class: "badge commented",
                        title: "Review comment from {commented_by} — open the card to read it",
                        "💬 commented"
                    }
                }
            }
            // The reason the user left when marking the card blocked. Clamped to
            // three lines so a long note can't stretch the column; the title
            // attribute keeps the whole thing readable on hover.
            if let Some(note) = blocked_note {
                div { class: "blocked-note", title: "{note}", "{note}" }
            }
            div { class: if frozen { "card-actions is-busy" } else { "card-actions" },
                // Shield the action buttons' keydowns from the card handler too.
                onkeydown: move |e| e.stop_propagation(),
                // A blocked card is waiting on something outside Usine, so hide
                // the actions that would advance it. The preview controls below
                // and the chevron menu (the only way to unmark) stay.
                if !card.blocked {
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
                            onclick: move |e| {
                                e.stop_propagation();
                                super::open_card_at(state, id, "intervention-answer");
                            },
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
                            // "Review" is the self-review pass; reading a plan is
                            // a different thing and gets a different verb.
                            onclick: move |e| {
                                e.stop_propagation();
                                super::open_card_at(state, id, "plan-approval");
                            },
                            "Read plan"
                        }
                    }
                    if let Some((label, anchor)) = review_action {
                        button {
                            class: "btn primary",
                            onclick: move |e| { e.stop_propagation(); super::open_card_at(state, id, anchor); },
                            "{label}"
                        }
                    }
                    if concluded {
                        button {
                            class: "btn primary",
                            onclick: move |e| { e.stop_propagation(); super::open_card_at(state, id, "conclusion"); },
                            "Read conclusion"
                        }
                    }
                    if can_read {
                        button {
                            class: "btn primary",
                            title: "The agent reads the review, triages each comment and proposes which to fix",
                            onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::FetchComments { card_id: id }); },
                            "Read the review"
                        }
                    }
                    if selecting {
                        button {
                            class: "btn primary",
                            onclick: move |e| { e.stop_propagation(); super::open_card_at(state, id, "fix-selection"); },
                            "Select fixes"
                        }
                    }
                    // Sits beside the green Merge, so it is explicitly the
                    // secondary of the two — nothing else said which one the
                    // merge gate wants you to press.
                    if can_reevaluate {
                        button {
                            class: "btn subtle",
                            title: "Feedback landed after the last pass — have the agent read and triage it before merging",
                            onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::FetchComments { card_id: id }); },
                            "Re-read the review"
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
                        if conflicting {
                            button {
                                class: "btn primary",
                                onclick: move |e| {
                                    e.stop_propagation();
                                    state.send(ExecutorCommand::ResolveConflicts { card_id: id });
                                },
                                "Resolve conflicts"
                            }
                        } else if ci_failing {
                            button {
                                class: "btn primary",
                                onclick: move |e| {
                                    e.stop_propagation();
                                    state.send(ExecutorCommand::FixChecks { card_id: id });
                                },
                                "Fix checks"
                            }
                        } else if ci_pending {
                            // Was nothing at all, which left an actionable-looking
                            // card with no button and only the "• CI" badge's
                            // tooltip to explain the wait. The panel keeps the
                            // "Merge anyway" override.
                            button {
                                class: "btn success",
                                disabled: true,
                                title: "Waiting on CI — the poll re-enables this within ~20s. Open the card to merge without waiting.",
                                "Merge"
                            }
                        } else {
                            button {
                                class: "btn success",
                                onclick: move |e| {
                                    e.stop_propagation();
                                    super::confirm_then_send(
                                        state,
                                        "Merge pull request",
                                        // The board button has always deleted the
                                        // branch (the panel offers a checkbox);
                                        // say so, since the action can't be undone.
                                        "Merge this pull request into the base branch on GitHub and delete its branch? This can't be undone.".to_string(),
                                        "Merge",
                                        ExecutorCommand::Merge { card_id: id, delete_branch: true, force: false },
                                    );
                                },
                                "Merge"
                            }
                        }
                    }
                    // A run in flight had no board-level way out at all — the
                    // only Stop lived in the panel. A question run is excluded:
                    // its Cancel is on the panel's banner, and the row is frozen.
                    if running && !frozen && queued_pos.is_none() {
                        button {
                            class: "btn subtle",
                            title: "Stop the agent's current run",
                            onclick: move |e| {
                                e.stop_propagation();
                                super::request_confirm(super::ConfirmRequest {
                                    title: "Stop the run?".into(),
                                    message: "Stop the agent's current run? Its progress is discarded.".into(),
                                    confirm_label: "Stop".into(),
                                    danger: true,
                                    action: super::ConfirmAction::Send(ExecutorCommand::Cancel { card_id: id }),
                                });
                            },
                            "Stop"
                        }
                    }
                    if failed {
                        button {
                            class: "btn",
                            onclick: move |e| { e.stop_propagation(); state.send(ExecutorCommand::Retry { card_id: id }); },
                            "{recover_label}"
                        }
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
