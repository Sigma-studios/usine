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
    let questions_for_submit = questions.clone();
    // A question is "answered" once it has a picked option or typed text. With
    // every question answered, the plan can be sent back even with no free-form
    // text — the answers alone are the feedback.
    let all_answered = answers.read().iter().all(|a| !a.trim().is_empty());
    let chat_hint = if has_questions {
        "Answer the questions above and/or type below, then request changes to send the plan \
         back — or ask the agent a question about its plan without re-planning."
    } else {
        "Request changes to send the plan back to design, or ask the agent a question about \
         its plan without re-planning."
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
        }

        super::AgentChatSection {
            card_id,
            hint: chat_hint,
            request_enabled_when_blank: has_questions && all_answered,
            on_request: move |text: String| {
                let cur = answers.read();
                let any_answered = cur.iter().any(|a| !a.trim().is_empty());
                // Fold answered questions and free-form notes into one
                // feedback blob so neither input is lost on send-back.
                let mut parts: Vec<String> = Vec::new();
                if has_questions && any_answered {
                    parts.push(build_answers_feedback(&questions_for_submit, &cur));
                }
                if !text.is_empty() {
                    parts.push(text);
                }
                let combined = parts.join("\n\n");
                if !combined.is_empty() {
                    state.send(ExecutorCommand::RejectPlan { card_id, feedback: combined });
                }
            },
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
