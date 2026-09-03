//! The fix-selection panel, shared by PR-comment triage and self-review: lets
//! the user pick which findings to apply — editing the AI's proposed text
//! first, since that text is exactly what the fix run (or the GitHub reply)
//! receives — say per finding *how* it should be fixed, and add a free-form
//! comment of their own, which the fix run addresses alongside them. The
//! composed task is shown at the bottom and is itself editable: touch it and
//! that text is what the agent receives, verbatim.

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

    // The task exactly as the agent will get it. `None` = untouched, so the box
    // mirrors the composed task live; `Some` = the user's own wording, sent
    // verbatim (even blank, which means "run nothing"). Origin-keyed on the
    // verdicts like `fixes.verdicts`, so a re-run's picker reseeds.
    let mut task = drafts::use_draft_of(card_id, "fixes.task", &verdicts, || None::<String>);
    let selected: Vec<FixVerdict> = edits
        .read()
        .iter()
        .filter(|v| v.selected)
        .cloned()
        .collect();
    let generated = usine_core::fix_prompt(&selected, note.read().trim());
    let edited = task.read().clone();
    let shown = edited.clone().unwrap_or_else(|| generated.clone());
    let will_run = !shown.trim().is_empty();
    let task_rows = fallback_rows(&shown);

    let apply_label = if !will_run {
        if self_review {
            "Continue without fixes"
        } else {
            "Ignore comments and continue"
        }
    } else if edited.is_some() {
        "Apply your edited task"
    } else {
        match (any_selected, has_note) {
            (true, true) => "Apply selected fixes & note",
            (true, false) => "Apply selected fixes",
            _ => "Apply your note",
        }
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
                    // A synthetic item built from a review's *body* has no
                    // path/line — label it the way the prompts do.
                    let is_review_body = v.comment.review_body_of.is_some();
                    let path = if is_review_body {
                        "PR review summary".to_string()
                    } else {
                        match v.comment.line {
                            Some(l) => format!("{}:{}", v.comment.path, l),
                            None => v.comment.path.clone(),
                        }
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
                    // the agent drafted none, so the user can add their own. Not
                    // for a review-body item: GitHub has no reply endpoint for a
                    // review body, so a typed reply would be silently discarded.
                    let reply = v.reply.clone();
                    let show_reply = !self_review && !checked && !is_review_body;
                    // Checked means "fix it"; this is where the user says how —
                    // the counterpart of the reply box, which shows when the row
                    // is unchecked. An instruction typed then unchecked stays in
                    // the draft and is simply ignored, as replies already are.
                    let instruction = v.instruction.clone();
                    let show_steer = checked;
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
                                    label { class: "reply-label", "finding — edit to reword it" }
                                    textarea {
                                        class: "review-comment-edit autogrow",
                                        rows: "{fallback_rows(&body)}",
                                        value: "{body}",
                                        oninput: move |e| edits.write()[i].comment.body = e.value(),
                                    }
                                } else {
                                    // A reviewer's comment isn't ours to edit — it's
                                    // shown verbatim; the steering box below says how
                                    // this one gets fixed.
                                    details { class: "orig",
                                        summary { "original comment" }
                                        div { class: "orig-body", "{body}" }
                                    }
                                }
                                if show_steer {
                                    div { class: "steer-edit",
                                        label { class: "reply-label", "↳ how to fix it (optional)" }
                                        textarea {
                                            class: "review-comment-edit autogrow",
                                            rows: "{fallback_rows(&instruction)}",
                                            placeholder: "Steer this fix — e.g. \"do it in the extractor, not the caller\".",
                                            value: "{instruction}",
                                            oninput: move |e| edits.write()[i].instruction = e.value(),
                                        }
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
            details { class: "fix-task",
                summary {
                    title: "The run also receives the card description and its worktree context ahead of this text",
                    "Task sent to the agent — edit to send your own wording"
                }
                textarea {
                    class: "review-comment-edit task-edit",
                    rows: "{task_rows}",
                    placeholder: "Nothing to do — check a finding, add a note, or write the task yourself.",
                    value: "{shown}",
                    oninput: move |e| task.set(Some(e.value())),
                }
                if edited.is_some() {
                    div { class: "hint", "edited — the rows and note no longer update this text" }
                    button {
                        class: "btn subtle",
                        title: "The checkboxes still control the replies posted and the threads resolved, whatever this text says",
                        onclick: move |_| task.set(None),
                        "Reset to generated"
                    }
                }
            }
            div { class: "row",
                button {
                    class: "btn primary",
                    onclick: move |_| {
                        let verdicts = edits.read().clone();
                        let text = note.read().trim().to_string();
                        let prompt = task.read().clone();
                        if self_review {
                            state.send(ExecutorCommand::ApplySelfFixes { card_id, verdicts, note: text, prompt });
                        } else {
                            state.send(ExecutorCommand::ApplyFixes { card_id, verdicts, note: text, prompt });
                        }
                        // The drafts were consumed by the send; a later
                        // re-analysis can legitimately produce identical
                        // verdicts, so the origin rule alone wouldn't clear.
                        drafts::forget(card_id, "fixes.verdicts");
                        drafts::forget(card_id, "fixes.task");
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
                        onclick: move |_| {
                            state.send(ExecutorCommand::SkipToPr { card_id });
                            // The button promises the note is discarded; forget
                            // the store entry too in case the panel unmounts
                            // before the mirror effect sees the reset.
                            note.set(String::new());
                            drafts::forget(card_id, "fixes.note");
                            // The composed task mirrored that note; nothing was
                            // sent, so don't keep an edit of it either.
                            drafts::forget(card_id, "fixes.task");
                        },
                        "Skip to PR"
                    }
                }
            }
        }
    }
}
