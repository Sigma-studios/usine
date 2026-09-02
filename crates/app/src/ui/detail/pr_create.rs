//! The "awaiting review" panel: read the implement run's hand-off, run a
//! self-review pass over the finished work in the card's worktree (or send it
//! back for revisions), then open the pull request. Testing the app and
//! inspecting the diff live in the card actions menu. Nothing here checks the
//! branch out into the main working copy.

use dioxus::prelude::*;
use usine_core::{Card, CardState, ExecutorCommand, ReviewSub, MAX_VALIDATION_ATTEMPTS};

use crate::state::AppState;
use crate::ui::drafts;

#[component]
pub(super) fn PrCreateForm(card: Card) -> Element {
    let state = use_context::<AppState>();
    let id = card.id;
    let project_id = card.project_id;
    // The form fields are drafts: typed-but-unsent text survives deselects and
    // the card moving between the parked states. The seeded ones (title,
    // reviewer) stay live-fresh until actually touched, per the seed rule.
    let mut title = drafts::use_draft(id, "pr.title", || card.title.clone());
    // Start the PR description blank rather than echoing the original task prompt.
    let mut body = drafts::use_draft(id, "pr.body", String::new);
    let project = state
        .projects
        .read()
        .iter()
        .find(|p| p.id == card.project_id)
        .cloned();
    let default_reviewer = project
        .as_ref()
        .and_then(|p| p.config.reviewer.clone())
        .unwrap_or_default();
    let mut reviewer = drafts::use_draft(id, "pr.reviewer", || default_reviewer);
    let branch = card.branch.clone().unwrap_or_default();
    // The PR branch name is required and starts blank: the user must deliberately
    // choose one rather than shipping the auto-generated `usine/…` name.
    let mut branch_name = drafts::use_draft(id, "pr.branch", String::new);
    // Sanitise for display and for the command, but keep the raw text in the
    // signal — rewriting the field on every keystroke would fight the user (a
    // trailing `.` gets trimmed, so `feat.x` becomes untypeable).
    let typed = branch_name.read().trim().to_string();
    let clean_branch = usine_core::sanitize_branch_name(&typed);
    let branch_rewritten = !clean_branch.is_empty() && clean_branch != typed;
    let branch_unusable = !typed.is_empty() && clean_branch.is_empty();
    let branch_ready = !clean_branch.is_empty();
    let has_branch = !branch.is_empty();
    let worktree_display = card
        .worktree_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let has_worktree = card.worktree_path.is_some();

    // Populate the reviewer dropdown once on first view; the refresh button
    // re-runs the lookup on demand.
    use_hook(move || {
        if state.reviewers.peek().get(&project_id).is_none() {
            state.fetch_reviewers(project_id);
        }
    });
    let people = state
        .reviewers
        .read()
        .get(&project_id)
        .cloned()
        .unwrap_or_default();
    let has_people = !people.is_empty();

    let is_ready_for_review = matches!(
        card.state,
        CardState::AwaitingReview(ReviewSub::ReadyForReview)
    );
    let is_self_reviewing = matches!(
        card.state,
        CardState::AwaitingReview(ReviewSub::Reviewing | ReviewSub::ApplyingFixes)
    );
    let is_selecting_fixes = matches!(
        card.state,
        CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
    );
    let is_ready_for_pr = matches!(card.state, CardState::AwaitingReview(ReviewSub::ReadyForPr));
    // The validation gate's three faces: the check running, the agent fixing a
    // failure, and the parked exhausted-budget failure.
    let validating_attempt = match &card.state {
        CardState::AwaitingReview(ReviewSub::Validating { attempt }) => Some(*attempt),
        _ => None,
    };
    let fixing_attempt = match &card.state {
        CardState::AwaitingReview(ReviewSub::FixingValidation { attempt, .. }) => Some(*attempt),
        _ => None,
    };
    let validation_failure = match &card.state {
        CardState::AwaitingReview(ReviewSub::ValidationFailed { attempt, output }) => {
            Some((*attempt, output.clone()))
        }
        _ => None,
    };
    let handoff = state.handoffs.read().get(&id).cloned();

    rsx! {
        if let Some(handoff) = handoff {
            super::HandoffPanel { handoff }
        }

        if has_branch {
            div { class: "section",
                h3 { "Branch" }
                div { class: "wt-path", "{branch}" }
                if has_worktree {
                    div { class: "wt-path", "worktree: {worktree_display}" }
                }
            }
        }

        // 1) The recovery gate: the self-review normally auto-starts when the
        //    implementation finishes, so a card only rests here after a cancelled
        //    review or a failed auto-start. Re-run it, skip it, or send the work
        //    back to the agent to revise.
        if is_ready_for_review {
            div { class: "section",
                h3 { "Self-review" }
                div { class: "hint", "The self-review didn't run — it was cancelled or failed to start." }
                div { class: "row",
                    button {
                        class: "btn primary",
                        title: "Runs the review pass over the committed diff, guided by the project's review.md (or a default prompt)",
                        onclick: move |_| state.send(ExecutorCommand::SelfReview { card_id: id }),
                        "Self-review"
                    }
                    button {
                        class: "btn",
                        title: "Goes straight to the create-PR form with no review pass",
                        onclick: move |_| state.send(ExecutorCommand::SkipReview { card_id: id }),
                        "Skip review"
                    }
                }
            }
            super::AgentChatSection {
                card_id: id,
                request_title: "The agent picks the work back up in the card's worktree and re-runs the self-review afterwards",
                on_request: move |fb: String| {
                    state.send(ExecutorCommand::ReviseImplementation { card_id: id, feedback: fb });
                },
            }
        }

        // Self-review agent running. Cancelling parks the card back at the
        // manual gate with all its options.
        if is_self_reviewing {
            div { class: "section",
                div { class: "hint", "Self-review in progress…" }
                button {
                    class: "btn",
                    onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                    "Cancel"
                }
            }
        }

        // The fix picker (rendered separately) is where an auto-reviewed card
        // first parks, so the send-back affordance must be reachable from it too.
        if is_selecting_fixes {
            super::AgentChatSection {
                card_id: id,
                request_title: "The agent picks the work back up in the card's worktree and re-runs the self-review afterwards",
                on_request: move |fb: String| {
                    state.send(ExecutorCommand::ReviseImplementation { card_id: id, feedback: fb });
                },
            }
        }

        // Validation gate: the check running…
        if let Some(attempt) = validating_attempt {
            div { class: "section",
                h3 { "Validation" }
                div { class: "hint",
                    "Running the project's validate command (attempt {attempt.min(MAX_VALIDATION_ATTEMPTS)}/{MAX_VALIDATION_ATTEMPTS})…"
                }
                button {
                    class: "btn",
                    // Sends `Cancel`: it stops the check that is running, it does
                    // not mark validation skipped (that is "Create PR anyway",
                    // below, once the check has parked).
                    title: "Stops the validate command; the card returns to the create-PR gate",
                    onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                    "Stop validation"
                }
            }
        }

        // …the agent fixing a failure…
        if let Some(attempt) = fixing_attempt {
            div { class: "section",
                h3 { "Validation" }
                div { class: "hint",
                    "Validation failed — the agent is fixing it (attempt {attempt.min(MAX_VALIDATION_ATTEMPTS)}/{MAX_VALIDATION_ATTEMPTS})…"
                }
                button {
                    class: "btn",
                    onclick: move |_| state.send(ExecutorCommand::Cancel { card_id: id }),
                    "Cancel fix"
                }
            }
        }

        // …and the parked exhausted-budget failure.
        if let Some((attempt, output)) = validation_failure {
            div { class: "section",
                h3 { "Validation failed" }
                div { class: "hint",
                    "The validate command still fails after {attempt} attempt(s). The output ends with:"
                }
                pre { class: "validation-output", "{output}" }
                div { class: "row",
                    button {
                        class: "btn primary",
                        onclick: move |_| state.send(ExecutorCommand::RunValidation { card_id: id }),
                        "Run validation again"
                    }
                    button {
                        class: "btn",
                        onclick: move |_| state.send(ExecutorCommand::FixValidation { card_id: id }),
                        "Send to agent again"
                    }
                    button {
                        class: "btn",
                        onclick: move |_| state.send(ExecutorCommand::SkipValidation { card_id: id }),
                        "Create PR anyway"
                    }
                }
            }
            // The parked failure can also bounce the work back wholesale.
            super::AgentChatSection {
                card_id: id,
                request_title: "The agent picks the work back up in the card's worktree and re-runs the self-review afterwards",
                on_request: move |fb: String| {
                    state.send(ExecutorCommand::ReviseImplementation { card_id: id, feedback: fb });
                },
            }
        }

        // 2) Ready to open the pull request.
        if is_ready_for_pr {
            div { class: "section",
                h3 { "Create pull request" }
                div { class: "field",
                    label { r#for: "pr-branch", "Branch (required)" }
                    input {
                        id: "pr-branch",
                        placeholder: "e.g. feat/licensee-export",
                        initial_value: "{branch_name.peek()}",
                        oninput: move |e| branch_name.set(e.value()),
                    }
                    if branch_rewritten {
                        div { class: "hint", "Will be created as {clean_branch}" }
                    } else if branch_unusable {
                        div { class: "hint warn", "Not a usable branch name." }
                    } else {
                        div { class: "hint", "Choose the branch name the PR opens from." }
                    }
                }
                div { class: "field",
                    label { r#for: "pr-title", "Title" }
                    input {
                        id: "pr-title",
                        initial_value: "{title.peek()}",
                        oninput: move |e| title.set(e.value()),
                    }
                }
                div { class: "field",
                    label { r#for: "pr-body", "Description" }
                    textarea {
                        id: "pr-body",
                        initial_value: "{body.peek()}",
                        oninput: move |e| body.set(e.value()),
                    }
                }
                div { class: "field",
                    label { r#for: "pr-reviewer", "Reviewer" }
                    div { class: "row",
                        if has_people {
                            select {
                                id: "pr-reviewer",
                                value: "{reviewer}",
                                onchange: move |e| reviewer.set(e.value()),
                                option {
                                    value: "",
                                    selected: reviewer.read().is_empty(),
                                    "— no reviewer —"
                                }
                                for login in people.iter() {
                                    option {
                                        key: "{login}",
                                        value: "{login}",
                                        selected: *reviewer.read() == *login,
                                        "{login}"
                                    }
                                }
                            }
                        } else {
                            input {
                                id: "pr-reviewer",
                                placeholder: "GitHub username",
                                initial_value: "{reviewer.peek()}",
                                oninput: move |e| reviewer.set(e.value()),
                            }
                        }
                        button {
                            class: "btn icon",
                            title: "Refresh collaborators",
                            "aria-label": "Refresh collaborators",
                            onclick: move |_| state.fetch_reviewers(project_id),
                            "↻"
                        }
                    }
                }
                div { class: "row",
                    button {
                        class: "btn primary",
                        disabled: !branch_ready,
                        onclick: {
                            let branch = clean_branch.clone();
                            move |_| {
                                let r = reviewer.read().trim().to_string();
                                crate::ui::confirm_then_send(
                                    state,
                                    "Create pull request",
                                    "Rename the branch (if changed), push it, and open a pull request \
                                     ready for review on GitHub?".to_string(),
                                    "Create PR",
                                    ExecutorCommand::CreatePr {
                                        card_id: id,
                                        branch: branch.clone(),
                                        title: title.read().clone(),
                                        body: body.read().clone(),
                                        reviewer: if r.is_empty() { None } else { Some(r) },
                                        draft: false,
                                    },
                                );
                            }
                        },
                        "Create PR"
                    }
                    button {
                        class: "btn",
                        disabled: !branch_ready,
                        onclick: {
                            let branch = clean_branch.clone();
                            move |_| {
                                let r = reviewer.read().trim().to_string();
                                crate::ui::confirm_then_send(
                                    state,
                                    "Create draft pull request",
                                    "Rename the branch (if changed), push it, and open a draft pull request \
                                     on GitHub? You can add screenshots and mark it ready afterwards.".to_string(),
                                    "Create draft PR",
                                    ExecutorCommand::CreatePr {
                                        card_id: id,
                                        branch: branch.clone(),
                                        title: title.read().clone(),
                                        body: body.read().clone(),
                                        reviewer: if r.is_empty() { None } else { Some(r) },
                                        draft: true,
                                    },
                                );
                            }
                        },
                        "Create draft PR"
                    }
                }
            }

            // Still bounce the work back to the agent before opening the PR.
            super::AgentChatSection {
                card_id: id,
                request_title: "The agent picks the work back up in the card's worktree, before anything is pushed to GitHub",
                on_request: move |fb: String| {
                    state.send(ExecutorCommand::ReviseImplementation { card_id: id, feedback: fb });
                },
            }
        }
    }
}
