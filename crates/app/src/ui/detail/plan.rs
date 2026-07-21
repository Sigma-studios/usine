//! The plan-approval panel: renders the proposed plan, collects answers to
//! any outstanding questions, and approves or sends the plan back.

use dioxus::prelude::*;
use usine_core::ExecutorCommand;
use uuid::Uuid;

use crate::state::AppState;

#[component]
pub(super) fn PlanApproval(card_id: Uuid, plan: String) -> Element {
    let state = use_context::<AppState>();
    let (clean_plan, questions) = usine_core::parse_plan(&plan);
    let has_questions = !questions.is_empty();
    let n = questions.len();
    let mut answers = use_signal(|| vec![String::new(); n]);
    let mut feedback = use_signal(String::new);
    let questions_for_submit = questions.clone();
    // A question is "answered" once it has a picked option or typed text. The
    // plan can be sent back to refine either when every question is answered or
    // when there's free-form text — both inputs share one submit button, so
    // neither is silently dropped.
    let all_answered = answers.read().iter().all(|a| !a.trim().is_empty());
    let has_feedback = !feedback.read().trim().is_empty();
    let can_submit = (has_questions && all_answered) || has_feedback;
    let submit_class = if has_questions { "btn primary" } else { "btn" };
    let submit_hint = if has_questions {
        "Answer every question, or request changes below, to send the plan back."
    } else {
        "Type a change request to send the plan back."
    };

    rsx! {
        div { class: "section",
            h3 { "Proposed plan" }
            div { class: "plan-box", "{clean_plan}" }
        }

        if has_questions {
            div { class: "section",
                h3 { "Questions" }
                for (idx, q) in questions.iter().enumerate() {
                    {
                        let cur = answers.read().get(idx).cloned().unwrap_or_default();
                        rsx! {
                            div { key: "{idx}", class: "question",
                                div { class: "qtext", "{q.question}" }
                                div { class: "option-row",
                                    for opt in q.options.iter() {
                                        {
                                            let opt = opt.clone();
                                            let cls = if cur == opt { "btn primary" } else { "btn" };
                                            rsx! {
                                                button {
                                                    key: "{opt}",
                                                    class: "{cls}",
                                                    onclick: move |_| {
                                                        if let Some(a) = answers.write().get_mut(idx) {
                                                            *a = opt.clone();
                                                        }
                                                    },
                                                    "{opt}"
                                                }
                                            }
                                        }
                                    }
                                }
                                input {
                                    placeholder: "Or type your own answer…",
                                    value: "{cur}",
                                    oninput: move |e| {
                                        if let Some(a) = answers.write().get_mut(idx) {
                                            *a = e.value();
                                        }
                                    },
                                }
                            }
                        }
                    }
                }
            }
        }

        div { class: "section",
            // Approval is only offered once the agent has no outstanding
            // questions — until then the only way forward is to answer them
            // (which refines the plan). The server-side guard in `approve_plan`
            // enforces the same invariant defensively.
            if has_questions {
                div { class: "hint",
                    "Answer the questions above (or request changes below) and send the plan back to refine it before approving."
                }
            } else {
                div { class: "card-actions",
                    button {
                        class: "btn primary",
                        onclick: move |_| state.send(ExecutorCommand::ApprovePlan { card_id }),
                        "Approve & implement"
                    }
                }
            }
            div { class: "field",
                label { r#for: "plan-feedback", "Request changes (free-form)" }
                textarea {
                    id: "plan-feedback",
                    placeholder: "Anything else to change?",
                    value: "{feedback}",
                    oninput: move |e| feedback.set(e.value()),
                }
                button {
                    class: "{submit_class}",
                    disabled: !can_submit,
                    onclick: move |_| {
                        let cur = answers.read();
                        let any_answered = cur.iter().any(|a| !a.trim().is_empty());
                        // Fold answered questions and free-form notes into one
                        // feedback blob so neither input is lost on send-back.
                        let mut parts: Vec<String> = Vec::new();
                        if has_questions && any_answered {
                            parts.push(build_answers_feedback(&questions_for_submit, &cur));
                        }
                        let fb = feedback.read().trim().to_string();
                        if !fb.is_empty() {
                            parts.push(fb);
                        }
                        let combined = parts.join("\n\n");
                        if !combined.is_empty() {
                            state.send(ExecutorCommand::RejectPlan { card_id, feedback: combined });
                        }
                    },
                    "Send back to design"
                }
                if !can_submit {
                    div { class: "hint", "{submit_hint}" }
                }
            }
        }
    }
}

/// Format the user's answers as feedback to re-plan with.
fn build_answers_feedback(questions: &[usine_core::PlanQuestion], answers: &[String]) -> String {
    let mut s = String::from("Here are my answers to your questions:\n");
    for (i, q) in questions.iter().enumerate() {
        let a = answers.get(i).map(|x| x.trim()).unwrap_or("");
        let a = if a.is_empty() {
            "(no preference — you decide)"
        } else {
            a
        };
        s.push_str(&format!("{}. {} → {}\n", i + 1, q.question, a));
    }
    s.push_str("\nPlease update the plan to reflect these decisions.");
    s
}
