//! The fix-selection panel, shared by PR-comment triage and self-review: lets
//! the user pick which findings to apply — editing the AI's proposed text
//! first, since that text is exactly what the fix run (or the GitHub reply)
//! receives — and add a free-form comment of their own, which the fix run
//! addresses alongside them.

use dioxus::prelude::*;
use usine_core::{ExecutorCommand, FixVerdict};
use uuid::Uuid;

use super::review::fallback_rows;
use crate::state::AppState;
use crate::ui::drafts;

#[component]
pub(super) fn FixSelection(card_id: Uuid, verdicts: Vec<FixVerdict>, self_review: bool) -> Element {
    let state = use_context::<AppState>();
    // The user's working copy of the verdicts: checkbox state and edited text
    // live here, and the apply command sends it wholesale. A draft keyed on the
    // verdicts themselves, so edits survive deselects but a re-run analysis
    // reseeds instead of restoring edits to findings it no longer shows.
    let mut edits = drafts::use_draft_of(card_id, "fixes.verdicts", &verdicts, || verdicts.clone());
    let mut note = drafts::use_draft(card_id, "fixes.note", String::new);
    let heading = if self_review {
        "Self-review findings"
    } else {
        "Review comments"
    };
    // The note is a fix request in its own right: with nothing checked it still
    // sends the agent to work, so the primary button stays live and says so.
    // With neither a check nor a note, the button doesn't apply anything — it
    // advances (skipping to the PR for a self-review, ignoring the comments and
    // moving to merge for PR triage), so it says that instead.
    let has_note = !note.read().trim().is_empty();
    let any_selected = edits.read().iter().any(|v| v.selected);
    let apply_label = match (any_selected, has_note) {
        (true, true) => "Apply selected fixes & note",
        (true, false) => "Apply selected fixes",
        (false, true) => "Apply your note",
        (false, false) if self_review => "Continue without fixes",
        (false, false) => "Ignore comments and continue",
    };

    // Grow each edit box to fit its text (same script as the review drafts
    // panel: idempotent, keeps sizing on every keystroke). Unlike that panel,
    // textareas here appear and disappear as rows are (un)checked — reply boxes
    // mount when a row is unchecked — so the effect subscribes to `edits` to
    // re-wire whatever the toggle just put in the DOM.
    use_effect(move || {
        edits.read();
        dioxus::document::eval(
            "(function(){document.querySelectorAll('textarea.autogrow').forEach(function(el){\
             var fit=function(){el.style.height='auto';el.style.height=el.scrollHeight+'px';};\
             if(!el.dataset.growInit){el.dataset.growInit='1';el.addEventListener('input',fit);}\
             fit();});})();",
        );
    });

    let rows_snapshot = edits.read().clone();
    rsx! {
        div { class: "section",
            h3 { "{heading}" }
            for (i, v) in rows_snapshot.iter().enumerate() {
                {
                    let cid = v.comment.id;
                    let checked = v.selected;
                    let path = match v.comment.line {
                        Some(l) => format!("{}:{}", v.comment.path, l),
                        None => v.comment.path.clone(),
                    };
                    let body = v.comment.body.clone();
                    let rationale = v.rationale.clone();
                    // Severity badge (falls back to a neutral dash when unrated).
                    let sev = v.severity.clone();
                    let sev_label = if sev.is_empty() { "—".to_string() } else { sev.clone() };
                    let sev_class = if sev.is_empty() {
                        "sev".to_string()
                    } else {
                        format!("sev sev-{sev}")
                    };
                    let verdict_label = if v.worth_fixing { "fix" } else { "skip" };
                    let vclass = if v.worth_fixing { "verdict-yes" } else { "verdict-no" };
                    // For PR comments, the reply posted if the comment is left
                    // unchecked (i.e. not fixed) — editable, and offered even when
                    // the agent drafted none, so the user can add their own.
                    let reply = v.reply.clone();
                    let show_reply = !self_review && !checked;
                    rsx! {
                        div { key: "{cid}", class: "comment",
                            input {
                                r#type: "checkbox",
                                checked,
                                onchange: move |_| {
                                    let mut list = edits.write();
                                    list[i].selected = !list[i].selected;
                                },
                            }
                            div { class: "comment-main",
                                div { class: "comment-head",
                                    span { class: "{sev_class}", "{sev_label}" }
                                    span { class: "verdict-tag {vclass}", "{verdict_label}" }
                                    span { class: "path", "{path}" }
                                }
                                div { class: "rationale", "{rationale}" }
                                if self_review {
                                    // The finding text IS the fix instruction the
                                    // agent receives — let the user reword it.
                                    textarea {
                                        class: "review-comment-edit autogrow",
                                        rows: "{fallback_rows(&body)}",
                                        value: "{body}",
                                        oninput: move |e| edits.write()[i].comment.body = e.value(),
                                    }
                                } else {
                                    // A reviewer's comment isn't ours to edit; the
                                    // free-form note below steers how it gets fixed.
                                    details { class: "orig",
                                        summary { "original comment" }
                                        div { class: "orig-body", "{body}" }
                                    }
                                }
                                if show_reply {
                                    div { class: "reply-edit",
                                        label { class: "reply-label", "↳ reply posted if left unchecked" }
                                        textarea {
                                            class: "review-comment-edit autogrow",
                                            rows: "{fallback_rows(&reply)}",
                                            placeholder: "No reply — leave empty to ignore silently.",
                                            value: "{reply}",
                                            oninput: move |e| edits.write()[i].reply = e.value(),
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "field",
                label { r#for: "fix-note", "Add a comment (free-form)" }
                textarea {
                    id: "fix-note",
                    placeholder: if self_review {
                        "Anything the review missed? It's applied with the fixes you checked."
                    } else {
                        "Anything to change beyond these comments? It's applied with the fixes you checked."
                    },
                    value: "{note}",
                    oninput: move |e| note.set(e.value()),
                }
            }
            div { class: "row",
                button {
                    class: "btn primary",
                    onclick: move |_| {
                        let verdicts = edits.read().clone();
                        let text = note.read().trim().to_string();
                        if self_review {
                            state.send(ExecutorCommand::ApplySelfFixes { card_id, verdicts, note: text });
                        } else {
                            state.send(ExecutorCommand::ApplyFixes { card_id, verdicts, note: text });
                        }
                        // The drafts were consumed by the send; a later
                        // re-analysis can legitimately produce identical
                        // verdicts, so the origin rule alone wouldn't clear.
                        drafts::forget(card_id, "fixes.verdicts");
                        note.set(String::new());
                    },
                    "{apply_label}"
                }
                button {
                    class: "btn",
                    title: if self_review { "Re-review the committed diff" } else { "Re-fetch the PR comments and re-run the analysis" },
                    onclick: move |_| {
                        if self_review {
                            state.send(ExecutorCommand::SelfReview { card_id });
                        } else {
                            state.send(ExecutorCommand::FetchComments { card_id });
                        }
                    },
                    "Re-run analysis"
                }
                if self_review {
                    button {
                        class: "btn",
                        title: if has_note { "Opens the PR without applying anything — your note is discarded" } else { "Apply nothing and open the PR" },
                        onclick: move |_| state.send(ExecutorCommand::SkipToPr { card_id }),
                        "Skip to PR"
                    }
                }
            }
        }
    }
}
