//! The concluded-investigation panel: renders the conclusion, offers a
//! follow-up round ("dig deeper"), and can convert the card — in place — into
//! an implementation whose prompt carries the findings.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::drafts;
use crate::ui::textfield::use_push_back;
use crate::ui::widgets::{ArtifactTabs, ArtifactText};

#[component]
pub(super) fn ConclusionPanel(card_id: Uuid, conclusion: String) -> Element {
    let state = use_context::<AppState>();
    // The prose conclusion is the payload — it is what a follow-up quotes back
    // and what "turn into implementation" folds into the description — so the
    // blocks are views over it, stripped for display exactly as for the prompt.
    let prose = usine_core::conclusion_prose(&conclusion);
    let findings = usine_core::parse_findings(&conclusion);
    let findings_malformed = usine_core::findings_malformed(&conclusion);
    let (_, questions) = usine_core::parse_questions(&conclusion);
    let has_questions = !questions.is_empty();
    let n = questions.len();
    // Keyed on the conclusion they answer, like the plan panel's: a follow-up
    // round lands a new conclusion whose questions are different.
    let answers = drafts::use_draft_of(card_id, "investigate.answers", &conclusion, || {
        vec![String::new(); n]
    });
    let questions_for_submit = questions.clone();
    let mut follow_up = drafts::use_draft(card_id, "investigate.followup", String::new);
    // Nothing to send is nothing to do — same rule the chat section's two
    // buttons already follow, rather than a live button that no-ops.
    let blank =
        follow_up.read().trim().is_empty() && answers.read().iter().all(|a| a.trim().is_empty());
    // Uncontrolled (see `ui/textfield.rs`), so clearing the box after a send
    // only reaches the DOM through a remount.
    let mut pushback = use_push_back(follow_up.read().clone());

    rsx! {
        div { class: "section",
            h3 { "Conclusion" }
            ConclusionTabs { card_id, prose: prose.clone(), findings: findings.clone() }
            if findings_malformed {
                div { class: "hint",
                    "The agent attached a malformed findings block; the conclusion above is unaffected."
                }
            }
        }

        if has_questions {
            div { class: "section",
                h3 { "Questions" }
                div { class: "hint",
                    "Answering these sends a follow-up round with your answers as context."
                }
                super::plan::QuestionList { questions: questions.clone(), answers }
            }
        }

        div { class: "section",
            h3 { "Dig deeper" }
            div { class: "field",
                for g in [pushback.key()] {
                    textarea {
                        key: "{g}",
                        placeholder: "What should the agent look into next?",
                        initial_value: "{follow_up.peek()}",
                        oninput: move |e| {
                            pushback.typed(&e.value());
                            follow_up.set(e.value());
                        },
                        onpaste: move |_| super::edit::attach_from_clipboard(state, card_id),
                    }
                }
            }
            button {
                class: "btn",
                disabled: blank,
                title: "The agent re-investigates with this conclusion and every earlier round as context",
                onclick: move |_| {
                    // Answers and free-form text are one blob, same as the plan
                    // panel's send-back: neither input is lost.
                    let cur: Vec<String> = answers.read().clone();
                    let mut parts: Vec<String> = Vec::new();
                    if cur.iter().any(|a| !a.trim().is_empty()) {
                        parts.push(super::plan::build_answers_feedback(&questions_for_submit, &cur));
                    }
                    let text = follow_up.read().trim().to_string();
                    if !text.is_empty() {
                        parts.push(text);
                    }
                    let combined = parts.join("\n\n");
                    if !combined.is_empty() {
                        state.send(ExecutorCommand::FollowUpInvestigation { card_id, feedback: combined });
                        follow_up.set(String::new());
                        drafts::forget(card_id, "investigate.answers");
                    }
                },
                "Investigate further"
            }
        }

        div { class: "section",
            h3 { "Turn into implementation" }
            button {
                class: "btn primary",
                title: "Continues this card as an implementation: the findings are folded into the task description and the card returns to the starting block, where you shape the prompt before starting",
                onclick: move |_| state.send(ExecutorCommand::ConvertToImplementation { card_id }),
                "Turn into implementation"
            }
        }
    }
}

/// The conclusion, plus the agent's structured findings when it emitted them:
/// each claim as its own row with the `file:line` it rests on, instead of
/// references buried in prose.
#[component]
fn ConclusionTabs(card_id: Uuid, prose: String, findings: Option<usine_core::Findings>) -> Element {
    let state = use_context::<AppState>();
    // An investigation has no branch and no diff, so a file reference opens the
    // file in the configured editor rather than a diff view.
    let open_path = move |path: String| {
        state.send(ExecutorCommand::OpenPath { card_id, path });
    };
    let has_findings = findings.as_ref().is_some_and(|f| !f.findings.is_empty());
    let mut on_findings = use_signal(|| false);
    let showing_findings = has_findings && on_findings();
    rsx! {
        if has_findings {
            ArtifactTabs {
                labels: vec!["Conclusion".to_string(), "Findings".to_string()],
                active: if showing_findings { 1 } else { 0 },
                onselect: move |i: usize| on_findings.set(i == 1),
            }
        }
        if showing_findings {
            {
                let f = findings.clone().unwrap_or_default();
                rsx! {
                    if !f.verdict.is_empty() {
                        div { class: "conclusion-verdict", "{f.verdict}" }
                    }
                    div { class: "change-list",
                        for (i, finding) in f.findings.iter().enumerate() {
                            div { key: "{i}", class: "change-row finding-row",
                                div { class: "change-head",
                                    if !finding.confidence.is_empty() {
                                        span { class: "badge kind", "{finding.confidence}" }
                                    }
                                    for (j, e) in finding.evidence.iter().enumerate() {
                                        button {
                                            key: "{j}",
                                            class: "change-path link",
                                            onclick: {
                                                let label = evidence_label(e);
                                                move |_| open_path(label.clone())
                                            },
                                            "{evidence_label(e)}"
                                        }
                                    }
                                }
                                div { class: "change-what", "{finding.claim}" }
                            }
                        }
                    }
                    if !f.open_questions.is_empty() {
                        div { class: "hint", "Still open" }
                        ul { class: "handoff-list",
                            for (i, q) in f.open_questions.iter().enumerate() {
                                li { key: "{i}", "{q}" }
                            }
                        }
                    }
                }
            }
        } else {
            ArtifactText { text: prose.clone(), on_path: open_path }
        }
    }
}

/// A piece of evidence as `path:line` (or just the path when unnumbered).
fn evidence_label(e: &usine_core::Evidence) -> String {
    match e.line {
        Some(line) => format!("{}:{}", e.path, line),
        None => e.path.clone(),
    }
}
