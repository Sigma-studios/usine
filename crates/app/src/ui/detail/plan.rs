//! The plan-approval panel: renders the proposed plan, collects answers to
//! any outstanding questions, and approves or sends the plan back.

use dioxus::prelude::*;
use usine_core::{ExecutorCommand, PlanQuestion};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::drafts;
use crate::ui::textfield::use_push_back;
use crate::ui::widgets::{ArtifactTabs, ArtifactText};

#[component]
pub(super) fn PlanApproval(card_id: Uuid, plan: String) -> Element {
    let state = use_context::<AppState>();
    let (clean_plan, questions) = usine_core::parse_plan(&plan);
    let block_malformed = usine_core::plan_block_malformed(&plan);
    let outline = usine_core::parse_plan_outline(&plan);
    let outline_malformed = usine_core::plan_outline_malformed(&plan);
    let has_questions = !questions.is_empty();
    let n = questions.len();
    // Draft answers, keyed on the plan they answer: asking a chat question
    // unmounts this panel while the read-only turn runs, so a component-local
    // signal would lose partially typed answers — while a replan landing
    // reseeds instead of restoring answers to questions it no longer asks.
    let answers = drafts::use_draft_of(card_id, "plan.answers", &plan, || vec![String::new(); n]);
    let questions_for_submit = questions.clone();
    // A question is "answered" once it has a picked option or typed text. With
    // every question answered, the plan can be sent back even with no free-form
    // text — the answers alone are the feedback.
    let all_answered = answers.read().iter().all(|a| !a.trim().is_empty());
    // The submit button says what the send will actually do: answers alone,
    // answers + feedback (when the chat box holds text), or a plain change
    // request — the chat section applies the nonblank label itself.
    let (request_label, request_label_nonblank) = if has_questions && all_answered {
        ("Send answers", Some("Send answers & feedback"))
    } else {
        ("Request changes", None)
    };
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
            // The prose plan is the payload — it is what the implement run
            // receives — so it is the default tab and never replaced. The rest
            // is the agent's own outline of it, shown only when it emitted one.
            PlanOutlineTabs { plan: clean_plan.clone(), outline: outline.clone() }
            if block_malformed {
                div { class: "hint",
                    "The agent attached a malformed questions block; it was ignored."
                }
            }
            if outline_malformed {
                div { class: "hint",
                    "The agent attached a malformed plan outline; the plan above is unaffected."
                }
            }
        }

        if has_questions {
            div { class: "section",
                h3 { "Questions" }
                QuestionList { questions: questions.clone(), answers }
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
            request_label: request_label.to_string(),
            request_label_nonblank: request_label_nonblank.map(str::to_string),
            on_request: move |text: String| {
                // No origin re-filter needed: the hook guarantees these answers
                // belong to the plan being sent back.
                let cur: Vec<String> = answers.read().clone();
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
                    drafts::forget(card_id, "plan.answers");
                }
            },
        }
    }
}

/// The agent's questions as pickable options plus a free-form box each, writing
/// into `answers` by index. Shared with the investigation panel: the block is
/// the same `usine-questions` payload wherever it comes from, and so is the
/// interaction — pick or type, then send it back.
#[component]
pub(super) fn QuestionList(questions: Vec<PlanQuestion>, answers: Signal<Vec<String>>) -> Element {
    let mut answers = answers;
    rsx! {
        for (idx, q) in questions.iter().enumerate() {
            {
                let cur = answers.read().get(idx).cloned().unwrap_or_default();
                let qcls = if cur.trim().is_empty() { "question" } else { "question answered" };
                rsx! {
                    div { key: "{idx}", class: "{qcls}",
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
                                            onclick: move |_| answers.write()[idx] = opt.clone(),
                                            "{opt}"
                                        }
                                    }
                                }
                            }
                        }
                        QuestionAnswer { idx, value: cur.clone(), answers }
                    }
                }
            }
        }
    }
}

/// The free-form answer box for one question. It is a component of its own
/// only because it needs a hook, and hooks cannot be called inside the `for`
/// over the questions.
#[component]
fn QuestionAnswer(idx: usize, value: String, answers: Signal<Vec<String>>) -> Element {
    let mut answers = answers;
    // Uncontrolled (see `ui/textfield.rs`): clicking an option writes the
    // answer from outside the field, which only reaches the DOM by remounting.
    let mut pushback = use_push_back(value.clone());
    rsx! {
        for g in [pushback.key()] {
            input {
                key: "{g}",
                // Stable hook for the push-back regression check in `stress.rs`.
                id: "plan-answer-{idx}",
                placeholder: "Or type your own answer…",
                initial_value: "{value}",
                oninput: move |e| {
                    pushback.typed(&e.value());
                    answers.write()[idx] = e.value();
                },
            }
        }
    }
}

/// Format the user's answers as feedback to re-plan (or re-investigate) with.
pub(super) fn build_answers_feedback(questions: &[PlanQuestion], answers: &[String]) -> String {
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

/// The plan itself, plus the agent's structured view of it — its steps, the
/// files it expects to touch, and how it means to check the result.
///
/// Deliberately a separate component from `PlanApproval`: the tab selection is
/// its own state, and the approval buttons, the questions and the
/// `#plan-approval` anchor must stay outside the strip so scroll-to-section and
/// the primary action are never a click away behind a tab.
#[component]
fn PlanOutlineTabs(plan: String, outline: Option<usine_core::PlanOutline>) -> Element {
    let sections: Vec<PlanTab> = PlanTab::ALL
        .into_iter()
        .filter(|tab| tab.has_content(outline.as_ref()))
        .collect();
    let mut active = use_signal(|| PlanTab::Plan);
    let index = sections.iter().position(|t| *t == active()).unwrap_or(0);
    let shown = sections.get(index).copied().unwrap_or(PlanTab::Plan);
    let empty = usine_core::PlanOutline::default();
    let outline = outline.unwrap_or(empty);

    rsx! {
        if sections.len() > 1 {
            ArtifactTabs {
                labels: sections.iter().map(|t| t.label().to_string()).collect::<Vec<_>>(),
                active: index,
                onselect: {
                    let sections = sections.clone();
                    move |i: usize| {
                        if let Some(tab) = sections.get(i) {
                            active.set(*tab);
                        }
                    }
                },
            }
        }
        match shown {
            PlanTab::Plan => rsx! {
                if !outline.tldr.is_empty() {
                    ul { class: "handoff-list",
                        for (i, t) in outline.tldr.iter().enumerate() {
                            li { key: "{i}", "{t}" }
                        }
                    }
                }
                ArtifactText { text: plan.clone() }
            },
            PlanTab::Steps => rsx! {
                ol { class: "plan-steps",
                    for (i, step) in outline.steps.iter().enumerate() {
                        li { key: "{i}",
                            div { class: "plan-step-title", "{step.title}" }
                            if !step.detail.is_empty() {
                                div { class: "change-what", "{step.detail}" }
                            }
                            if !step.files.is_empty() {
                                div { class: "plan-step-files",
                                    for (j, f) in step.files.iter().enumerate() {
                                        span { key: "{j}", class: "change-path", "{f}" }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            PlanTab::Files => rsx! {
                div { class: "plan-step-files",
                    for (i, f) in outline.files.iter().enumerate() {
                        span { key: "{i}", class: "change-path", "{f}" }
                    }
                }
            },
            PlanTab::Verify => rsx! {
                ul { class: "handoff-list",
                    for (i, v) in outline.verification.iter().enumerate() {
                        li { key: "{i}", "{v}" }
                    }
                }
                if !outline.risks.is_empty() {
                    div { class: "hint", "Risks" }
                    ul { class: "handoff-list",
                        for (i, r) in outline.risks.iter().enumerate() {
                            li { key: "{i}", "{r}" }
                        }
                    }
                }
            },
        }
    }
}

/// The plan panel's sections, in reading order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanTab {
    Plan,
    Steps,
    Files,
    Verify,
}

impl PlanTab {
    const ALL: [PlanTab; 4] = [
        PlanTab::Plan,
        PlanTab::Steps,
        PlanTab::Files,
        PlanTab::Verify,
    ];

    fn label(self) -> &'static str {
        match self {
            PlanTab::Plan => "Plan",
            PlanTab::Steps => "Steps",
            PlanTab::Files => "Files",
            PlanTab::Verify => "Verify",
        }
    }

    /// The prose plan is always there; the rest only when the outline has it.
    fn has_content(self, outline: Option<&usine_core::PlanOutline>) -> bool {
        match (self, outline) {
            (PlanTab::Plan, _) => true,
            (_, None) => false,
            (PlanTab::Steps, Some(o)) => !o.steps.is_empty(),
            (PlanTab::Files, Some(o)) => !o.files.is_empty(),
            (PlanTab::Verify, Some(o)) => !o.verification.is_empty() || !o.risks.is_empty(),
        }
    }
}
