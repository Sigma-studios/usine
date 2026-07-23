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
    // Draft answers live in `AppState` keyed by card + the plan they answer:
    // asking a chat question unmounts this panel while the read-only turn runs,
    // so a component-local signal would lose partially typed answers. A draft
    // stored for a different plan (a replan landed) is ignored here and
    // replaced on the next edit.
    let mut drafts = state.plan_drafts;
    let answers: Vec<String> = match drafts.read().get(&card_id) {
        Some((p, a)) if *p == plan && a.len() == n => a.clone(),
        _ => vec![String::new(); n],
    };
    let plan_for_edit = plan.clone();
    let set_draft = move |idx: usize, value: String| {
        let mut map = drafts.write();
        let entry = map
            .entry(card_id)
            .or_insert_with(|| (plan_for_edit.clone(), vec![String::new(); n]));
        if entry.0 != plan_for_edit || entry.1.len() != n {
            *entry = (plan_for_edit.clone(), vec![String::new(); n]);
        }
        entry.1[idx] = value;
    };
    let plan_for_submit = plan.clone();
    let questions_for_submit = questions.clone();
    // A question is "answered" once it has a picked option or typed text. With
    // every question answered, the plan can be sent back even with no free-form
    // text — the answers alone are the feedback.
    let all_answered = answers.iter().all(|a| !a.trim().is_empty());
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
                        let cur = answers.get(idx).cloned().unwrap_or_default();
                        let mut set_for_input = set_draft.clone();
                        rsx! {
                            div { key: "{idx}", class: "question",
                                div { class: "qtext", "{q.question}" }
                                div { class: "option-row",
                                    for opt in q.options.iter() {
                                        {
                                            let opt = opt.clone();
                                            let cls = if cur == opt { "btn primary" } else { "btn" };
                                            let mut set_for_option = set_draft.clone();
                                            rsx! {
                                                button {
                                                    key: "{opt}",
                                                    class: "{cls}",
                                                    onclick: move |_| set_for_option(idx, opt.clone()),
                                                    "{opt}"
                                                }
                                            }
                                        }
                                    }
                                }
                                input {
                                    placeholder: "Or type your own answer…",
                                    value: "{cur}",
                                    oninput: move |e| set_for_input(idx, e.value()),
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
                let cur: Vec<String> = drafts
                    .read()
                    .get(&card_id)
                    .filter(|(p, _)| *p == plan_for_submit)
                    .map(|(_, a)| a.clone())
                    .unwrap_or_default();
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
                    // The answers were consumed by this send-back; the replan's
                    // questions will be different.
                    drafts.write().remove(&card_id);
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
