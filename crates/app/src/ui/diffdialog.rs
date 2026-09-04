//! The diff dialog: a large modal that shows a unified, syntax-highlighted diff
//! — a card's committed contribution (`merge_base(base, branch)..branch`) before
//! the PR, or a contributor's pull request while reviewing it — without the code
//! ever leaving the worktree.
//!
//! For a PR under review it is also where the review is *validated*: the drafted
//! comments are anchored inline against the lines they're about, GitHub-style,
//! and checked, edited, and published from here. Judging a comment next to the
//! code it refers to is the whole point; the detail panel's flat list stays as
//! the scannable index into it.
//!
//! Like the confirm/menu hosts, it renders once at the app root from a global
//! signal holding what to show. It recomputes the diff on every open — the
//! cached entry is keyed by entity id alone, and the same id can mean two
//! different diffs (a PR, then the fix committed on top of it) — and offers a
//! manual recompute for after a revision.

use std::collections::HashSet;

use dioxus::prelude::*;
use usine_core::{
    anchor_drafts, DiffFile, DiffLineKind, DiffState, DraftComment, ExecutorCommand, FileStatus,
    ReviewEvent,
};
use uuid::Uuid;

use super::reviewdraft;
use crate::state::AppState;
use crate::ui::textfield::use_push_back;
use crate::ui::widgets::SeverityPicker;

/// Which entity's diff the dialog is showing. Both are keyed by a plain id in
/// the executor's per-entity diff map, so only the command used to (re)compute
/// them and the title differ.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffTarget {
    Card(Uuid),
    Review(Uuid),
}

impl DiffTarget {
    fn id(self) -> Uuid {
        match self {
            DiffTarget::Card(id) | DiffTarget::Review(id) => id,
        }
    }

    /// The command that (re)computes this diff.
    fn compute(self) -> ExecutorCommand {
        match self {
            DiffTarget::Card(card_id) => ExecutorCommand::ComputeDiff { card_id },
            DiffTarget::Review(review_id) => ExecutorCommand::ComputeReviewDiff { review_id },
        }
    }
}

/// What the dialog is showing, plus an optional anchor to scroll to on open.
#[derive(Clone)]
struct DiffRequest {
    target: DiffTarget,
    /// DOM id to scroll into view once the diff has rendered — set when the user
    /// jumped here from a specific drafted comment.
    scroll_to: Option<String>,
    /// A file path to scroll to instead. Resolved to a file's anchor once the
    /// diff is rendered, because the file's index — which the anchor is keyed by
    /// — isn't known until then.
    scroll_to_path: Option<String>,
    /// Fresh per open. The host computes once per token, which is what makes an
    /// open recompute rather than re-show whatever the cache holds.
    token: Uuid,
}

static DIFF_DIALOG: GlobalSignal<Option<DiffRequest>> = Signal::global(|| None);

/// Open the diff dialog for a card (called from the card actions menu).
pub(crate) fn open_diff_dialog(card_id: Uuid) {
    *DIFF_DIALOG.write() = Some(DiffRequest {
        target: DiffTarget::Card(card_id),
        scroll_to: None,
        scroll_to_path: None,
        token: Uuid::new_v4(),
    });
}

/// Open a card's diff scrolled to one file — the card-side twin of
/// [`open_review_diff_at`], for the hand-off's Changes tab and the `path:line`
/// chips in agent prose.
pub(crate) fn open_diff_dialog_at(card_id: Uuid, path: String) {
    *DIFF_DIALOG.write() = Some(DiffRequest {
        target: DiffTarget::Card(card_id),
        scroll_to: None,
        scroll_to_path: Some(path),
        token: Uuid::new_v4(),
    });
}

/// Open the diff dialog for a PR under review.
pub(crate) fn open_review_diff(review_id: Uuid) {
    *DIFF_DIALOG.write() = Some(DiffRequest {
        target: DiffTarget::Review(review_id),
        scroll_to: None,
        scroll_to_path: None,
        token: Uuid::new_v4(),
    });
}

/// Open a PR's diff scrolled to one drafted comment — the panel list's
/// "jump to diff" action.
pub(crate) fn open_review_diff_at(review_id: Uuid, comment_index: usize) {
    *DIFF_DIALOG.write() = Some(DiffRequest {
        target: DiffTarget::Review(review_id),
        scroll_to: Some(reviewdraft::anchor_id(comment_index)),
        scroll_to_path: None,
        token: Uuid::new_v4(),
    });
}

fn dismiss() {
    *DIFF_DIALOG.write() = None;
}

/// Close the dialog from outside (e.g. once a review it was validating has been
/// published). A no-op when nothing is open.
pub(crate) fn dismiss_dialog() {
    dismiss();
}

#[component]
pub fn DiffDialogHost() -> Element {
    let state = use_context::<AppState>();
    // Compute the diff once per open. Not "when nothing is cached": the cache is
    // keyed by entity id, so a review that has since moved to the fix gate would
    // otherwise re-show the PR diff computed while validating it — under a hint
    // announcing the fix. The token makes the effect fire exactly once per open,
    // so a `Computing` landing in the map can't re-trigger it.
    let mut computed = use_signal(|| None::<Uuid>);
    use_effect(move || {
        if let Some(req) = DIFF_DIALOG.read().as_ref() {
            if *computed.peek() != Some(req.token) {
                computed.set(Some(req.token));
                state.send(req.target.compute());
            }
        }
    });

    let Some(req) = DIFF_DIALOG.read().clone() else {
        return rsx! {};
    };
    let target = req.target;
    let id = target.id();
    let diff = state.diff(id);

    // The drafted comments to overlay, when this is a review awaiting validation.
    let review = match target {
        DiffTarget::Review(review_id) => {
            state.review_task(review_id).and_then(|t| match &t.status {
                usine_core::ReviewStatus::AwaitingValidation {
                    drafts,
                    summary,
                    event,
                } => {
                    reviewdraft::ensure_seeded(review_id, drafts, summary, *event);
                    reviewdraft::edits_for(review_id).map(|e| (t, e))
                }
                _ => None,
            })
        }
        DiffTarget::Card(_) => None,
    };

    // A PR whose review we published and are now fixing ourselves: the diff on
    // screen is the agent's fix, not the contributor's PR (see
    // `compute_review_diff`), and the dialog has to say so. `gate` additionally
    // means the fix is committed and waiting to be pushed — the bar's actions.
    let (fixing, gate) = match target {
        DiffTarget::Review(review_id) => state
            .review_task(review_id)
            .map(|t| {
                (
                    t.status.fix_gate().is_some(),
                    t.status.fix_gate_ready().then(|| t.head_ref.clone()),
                )
            })
            .unwrap_or((false, None)),
        DiffTarget::Card(_) => (false, None),
    };

    let title = match target {
        DiffTarget::Card(card_id) => state
            .cards
            .read()
            .iter()
            .find(|c| c.id == card_id)
            .map(|c| format!("Diff — {}", c.title))
            .unwrap_or_else(|| "Diff".to_string()),
        DiffTarget::Review(review_id) => state
            .review_task(review_id)
            .map(|t| {
                if fixing {
                    format!("Fix for PR #{} — {}", t.pr_number, t.pr_title)
                } else {
                    format!("PR #{} — {}", t.pr_number, t.pr_title)
                }
            })
            .unwrap_or_else(|| "Pull request".to_string()),
    };

    // Summed over every file, mirroring the per-file counts in the list.
    let totals = match &diff {
        Some(DiffState::Ready(data)) if !data.files.is_empty() => Some((
            data.files.iter().map(|f| f.added).sum::<u32>(),
            data.files.iter().map(|f| f.removed).sum::<u32>(),
        )),
        _ => None,
    };

    let drafts = review.as_ref().map(|(_, e)| e.comments.clone());
    let review_id = review.as_ref().map(|(t, _)| t.id);

    // While the review worktree holds the PR branch checked out, the head can't
    // be refetched (git refuses), so the diff shows the branch as checked out.
    let checked_out = match target {
        DiffTarget::Review(review_id) => state
            .review_task(review_id)
            .is_some_and(|t| t.worktree_path.is_some()),
        DiffTarget::Card(_) => false,
    };

    rsx! {
        div { class: "modal-overlay", onclick: move |_| dismiss(),
            div {
                class: "modal modal-large",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        dismiss();
                    }
                },
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                div { class: "modal-header",
                    h3 { class: "modal-title", "{title}" }
                    if let Some((added, removed)) = totals {
                        span { class: "diff-total-stat",
                            span { class: "add", "+{added}" }
                            span { class: "del", "−{removed}" }
                        }
                    }
                    div { class: "modal-header-actions",
                        button {
                            class: "btn icon",
                            title: "Recompute the diff",
                            "aria-label": "Recompute the diff",
                            onclick: move |_| state.send(target.compute()),
                            "↻"
                        }
                        button {
                            class: "btn icon",
                            title: "Close",
                            "aria-label": "Close",
                            onclick: move |_| dismiss(),
                            "✕"
                        }
                    }
                }
                div { class: "modal-body-fill",
                    match diff {
                        None | Some(DiffState::Computing) => rsx! {
                            div { class: "hint center", "Computing the diff…" }
                        },
                        Some(DiffState::Empty) => rsx! {
                            div { class: "hint center", "No committed changes over the base branch yet." }
                        },
                        Some(DiffState::Failed(msg)) => rsx! {
                            div { class: "hint error", "Couldn't compute the diff: {msg}" }
                        },
                        Some(DiffState::Ready(data)) => rsx! {
                            if checked_out {
                                div { class: "hint",
                                    if fixing {
                                        "The fix as committed in the review checkout — not pushed yet."
                                    } else {
                                        "Showing the PR as checked out for the review — pushes made after the review started aren't included."
                                    }
                                }
                            }
                            DiffView {
                                data,
                                drafts: drafts.clone(),
                                scroll_to: req.scroll_to.clone(),
                                scroll_to_path: req.scroll_to_path.clone(),
                            }
                        },
                    }
                }
                if let Some(review_id) = review_id {
                    ValidateBar { review_id }
                }
                if let Some(head_ref) = gate {
                    FixBar { review_id: id, head_ref }
                }
            }
        }
    }
}

/// The rendered unified diff: a navigable file list on the left, the diff on the
/// right. Split out so its per-file collapse state lives in its own component
/// scope (only mounted once a diff is ready).
///
/// When `drafts` is set, each comment is rendered as an inline thread under the
/// line it annotates. Comments whose anchor isn't in the diff are surfaced at the
/// top rather than dropped — see [`anchor_drafts`].
#[component]
fn DiffView(
    data: ReadSignal<usine_core::DiffData>,
    drafts: Option<Vec<DraftComment>>,
    scroll_to: Option<String>,
    scroll_to_path: Option<String>,
) -> Element {
    // Indices of files the user has collapsed (expanded by default).
    let mut collapsed = use_signal(HashSet::<usize>::new);
    // Case-insensitive substring filter over the left file list.
    let mut query = use_signal(String::new);
    let mut pushback = use_push_back(query.read().clone());
    let files = data.read();

    let drafts_ref = drafts.as_deref().unwrap_or(&[]);
    let anchors = anchor_drafts(&files, drafts_ref);

    // Scroll to the requested comment once the diff is on screen. Runs after
    // mount; the anchor exists only if the comment could be placed.
    use_effect(move || {
        // A path target resolves here, where the file list exists: the anchor is
        // keyed by index, and a path the diff doesn't cover simply doesn't scroll.
        let anchor = scroll_to.clone().or_else(|| {
            let want = scroll_to_path.clone()?;
            let files = data.read();
            files
                .files
                .iter()
                .position(|f| crate::ui::widgets::same_path(&want, &file_path(f)))
                .map(|fi| format!("difffile-{fi}"))
        });
        if let Some(anchor) = anchor {
            dioxus::document::eval(&format!(
                "requestAnimationFrame(function(){{document.getElementById('{anchor}')\
                 ?.scrollIntoView({{behavior:'smooth',block:'center'}});}});"
            ));
        }
    });

    if files.files.is_empty() {
        return rsx! { div { class: "hint", "No changes." } };
    }

    let needle = query.read().to_lowercase();
    // Original indices are kept: they anchor the scroll target and collapse state.
    let matches: Vec<(usize, &DiffFile)> = files
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| needle.is_empty() || file_path(f).to_lowercase().contains(&needle))
        .collect();

    rsx! {
        div { class: "diff-layout",
            // Left: the file list. Clicking a row expands that file (if collapsed)
            // and scrolls the diff to it.
            div { class: "diff-filelist",
                div { class: "diff-filelist-search",
                    // Uncontrolled (see `ui/textfield.rs`): Escape clears the
                    // filter from outside the field, so that clear only reaches
                    // the DOM by remounting — which also costs the focus the
                    // user still has, hence the re-focus below.
                    for g in [pushback.key()] {
                        input {
                            key: "{g}",
                            r#type: "text",
                            placeholder: "Filter files…",
                            "aria-label": "Filter files",
                            initial_value: "{query.peek()}",
                            oninput: move |e| {
                                pushback.typed(&e.value());
                                query.set(e.value());
                            },
                            onkeydown: move |e: KeyboardEvent| {
                                if e.key() == Key::Escape && !query.read().is_empty() {
                                    // Clear the filter first; a second Escape closes the dialog.
                                    e.stop_propagation();
                                    query.set(String::new());
                                }
                            },
                            onmounted: move |e: MountedEvent| {
                                if g > 0 {
                                    spawn(async move {
                                        let _ = e.data().set_focus(true).await;
                                    });
                                }
                            },
                        }
                    }
                }
                div { class: "diff-filelist-items",
                    if matches.is_empty() {
                        div { class: "diff-filelist-empty", "No file matches “{query}”." }
                    }
                    for (fi, file) in matches {
                        {
                            // How many drafted comments landed in this file, so the
                            // list doubles as a map of where the review has notes.
                            let n = anchors.per_file.get(&fi).copied().unwrap_or(0);
                            rsx! {
                                button {
                                    key: "nav{fi}",
                                    class: "diff-fileitem",
                                    title: "{file_path(file)}",
                                    onclick: move |_| {
                                        collapsed.write().remove(&fi);
                                        dioxus::document::eval(&format!(
                                            "document.getElementById('difffile-{fi}')?.scrollIntoView({{behavior:'smooth',block:'start'}});"
                                        ));
                                    },
                                    span { class: "diff-status {status_class(file.status)}", "{status_glyph(file.status)}" }
                                    span { class: "diff-fileitem-path", "{file_name(file)}" }
                                    if n > 0 {
                                        span { class: "diff-fileitem-comments", title: "{n} drafted comment(s)", "{n}" }
                                    }
                                    span { class: "diff-fileitem-stat",
                                        span { class: "add", "+{file.added}" }
                                        span { class: "del", "−{file.removed}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Right: the diff, scrollable.
            div { class: "diff-main",
                // Comments the agent anchored somewhere this diff doesn't cover
                // (a line outside any hunk, or a path that isn't in the change).
                // GitHub rejects inline comments off the diff, so these would fail
                // to post — surface them for editing instead of hiding them.
                if !anchors.unplaced.is_empty() {
                    div { class: "diff-unplaced",
                        div { class: "diff-unplaced-head",
                            "{anchors.unplaced.len()} comment(s) couldn't be placed on a line in this diff — they'll be posted on the review summary instead."
                        }
                        for ci in anchors.unplaced.iter().copied() {
                            InlineComment { key: "u{ci}", index: ci, comment: drafts_ref[ci].clone() }
                        }
                    }
                }
                div { class: "diff",
                    for (fi, file) in files.files.iter().enumerate() {
                        {
                            let is_collapsed = collapsed.read().contains(&fi);
                            rsx! {
                                div { key: "{fi}", id: "difffile-{fi}", class: "diff-file",
                                    div {
                                        class: "diff-file-head",
                                        onclick: move |_| {
                                            let mut c = collapsed.write();
                                            if !c.insert(fi) { c.remove(&fi); }
                                        },
                                        span { class: "diff-status {status_class(file.status)}", "{status_glyph(file.status)}" }
                                        span { class: "diff-file-path", "{file_path(file)}" }
                                        span { class: "diff-file-stat add", "+{file.added}" }
                                        span { class: "diff-file-stat del", "−{file.removed}" }
                                        if !file.highlighted && !file.binary {
                                            span { class: "diff-file-note", "no highlight" }
                                        }
                                        span { class: "diff-collapse", if is_collapsed { "▸" } else { "▾" } }
                                    }
                                    if !is_collapsed {
                                        // File-level comments (no line): they belong to
                                        // the file as a whole, so they head its block.
                                        for ci in anchors.file_level.get(&fi).into_iter().flatten().copied() {
                                            InlineComment { key: "f{ci}", index: ci, comment: drafts_ref[ci].clone() }
                                        }
                                        if file.binary {
                                            div { class: "diff-binary", "Binary file — not shown." }
                                        } else {
                                            for (hi, hunk) in file.hunks.iter().enumerate() {
                                                {
                                                    // Reserve ~one line-height per line so off-screen
                                                    // hunks (content-visibility) keep the scrollbar stable
                                                    // before they've been rendered once.
                                                    let est_px = hunk.lines.len() * 18 + 24;
                                                    rsx! {
                                                        div { key: "h{hi}", class: "diff-hunk",
                                                            style: "contain-intrinsic-height: auto {est_px}px;",
                                                            div { class: "diff-hunk-head", "{hunk.header}" }
                                                            for (li, line) in hunk.lines.iter().enumerate() {
                                                                {
                                                                    let at_line = anchors.by_line.get(&(fi, hi, li));
                                                                    rsx! {
                                                                        div { key: "l{li}",
                                                                            div { class: "diff-line {line_class(line.kind)}",
                                                                                span { class: "diff-gutter", "{gutter(line.old_no)}" }
                                                                                span { class: "diff-gutter", "{gutter(line.new_no)}" }
                                                                                span { class: "diff-sign", "{sign(line.kind)}" }
                                                                                span { class: "diff-code",
                                                                                    for (ti, tok) in line.tokens.iter().enumerate() {
                                                                                        span {
                                                                                            key: "{ti}",
                                                                                            style: tok.color.as_ref().map(|c| format!("color:{c}")).unwrap_or_default(),
                                                                                            "{tok.text}"
                                                                                        }
                                                                                    }
                                                                                }
                                                                            }
                                                                            // The review's own comments, threaded under
                                                                            // the line they're about.
                                                                            for ci in at_line.into_iter().flatten().copied() {
                                                                                InlineComment { key: "c{ci}", index: ci, comment: drafts_ref[ci].clone() }
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One drafted comment, rendered as a thread under the line it annotates.
/// Checking it includes it in the published review; the body is editable in
/// place. Edits go to the shared buffer, so the detail panel's list sees them.
#[component]
fn InlineComment(index: usize, comment: DraftComment) -> Element {
    let sev = comment.severity.clone();
    let checked = comment.selected;
    let row_class = if checked {
        "inline-comment"
    } else {
        "inline-comment is-skipped"
    };
    let rows = fallback_rows(&comment.body);
    let mut pushback = use_push_back(comment.body.clone());

    rsx! {
        div {
            class: "{row_class}",
            id: reviewdraft::anchor_id(index),
            // The diff line above is not interactive, but the file header is —
            // don't let a click inside the comment collapse the file.
            onclick: move |e| e.stop_propagation(),
            div { class: "inline-comment-head",
                input {
                    r#type: "checkbox",
                    checked,
                    "aria-label": "Include this comment in the review",
                    onchange: move |_| reviewdraft::toggle(index),
                }
                SeverityPicker {
                    severity: sev.clone(),
                    on_change: move |s| reviewdraft::set_severity(index, s),
                }
                span { class: "inline-comment-loc",
                    match comment.line {
                        Some(l) => format!("{}:{}", comment.path, l),
                        None => comment.path.clone(),
                    }
                }
                if !checked {
                    span { class: "inline-comment-skip", "skipped" }
                }
            }
            // Uncontrolled (see `ui/textfield.rs`): the review panel renders the
            // same `reviewdraft` body at the same time as this dialog, so an edit
            // made on the other surface has to be pushed back in.
            for g in [pushback.key()] {
                textarea {
                    key: "{g}",
                    class: "review-comment-edit autogrow",
                    rows: "{rows}",
                    initial_value: "{comment.body}",
                    oninput: move |e| {
                        pushback.typed(&e.value());
                        reviewdraft::set_body(index, e.value());
                    },
                }
            }
        }
    }
}

/// The publish bar pinned under the diff: what will be sent, the overall summary,
/// the verdict, and the button that posts it. Publishing from here means the last
/// thing seen before submitting is the code, not a form.
#[component]
fn ValidateBar(review_id: Uuid) -> Element {
    let state = use_context::<AppState>();
    let edits = reviewdraft::edits_for(review_id);
    // Before the early return below: hooks must run on every render.
    let mut pushback = use_push_back(
        edits
            .as_ref()
            .map(|e| e.summary.clone())
            .unwrap_or_default(),
    );
    let Some(edits) = edits else {
        return rsx! {};
    };
    let checked = edits.comments.iter().filter(|c| c.selected).count();
    let publish_label = match checked {
        0 => "Publish summary only".to_string(),
        1 => "Publish 1 comment".to_string(),
        n => format!("Publish {n} comments"),
    };
    let verdict = edits.verdict;
    let comments = edits.comments.clone();
    let summary = edits.summary.clone();

    rsx! {
        div { class: "review-validate-bar",
            div { class: "review-validate-summary",
                label { class: "review-validate-label", "Review summary" }
                // Uncontrolled (see `ui/textfield.rs`): the review panel edits
                // the same `reviewdraft` summary while this bar is mounted.
                for g in [pushback.key()] {
                    textarea {
                        key: "{g}",
                        class: "review-summary-edit",
                        rows: "2",
                        placeholder: "The overall review comment posted with the verdict…",
                        initial_value: "{edits.summary}",
                        oninput: move |e| {
                            pushback.typed(&e.value());
                            reviewdraft::set_summary(e.value());
                        },
                    }
                }
            }
            div { class: "review-validate-actions",
                select {
                    "aria-label": "Review verdict",
                    value: "{event_value(verdict)}",
                    onchange: move |e| reviewdraft::set_verdict(parse_event(&e.value())),
                    for ev in ReviewEvent::all() {
                        option {
                            value: event_value(ev),
                            selected: event_value(verdict) == event_value(ev),
                            "{ev.label()}"
                        }
                    }
                }
                button {
                    class: "btn primary",
                    onclick: move |_| {
                        super::confirm_publish_review(
                            state,
                            review_id,
                            comments.clone(),
                            verdict,
                            summary.clone(),
                        );
                    },
                    "{publish_label}"
                }
            }
        }
    }
}

/// The fix gate's bar, pinned under the diff: the same shape as [`ValidateBar`],
/// but its subject is our own committed fix. Pushing from here means the last
/// thing seen before the commit leaves the machine is the code it changes.
#[component]
fn FixBar(review_id: Uuid, head_ref: String) -> Element {
    let state = use_context::<AppState>();
    let mut note = super::drafts::use_draft(review_id, "review.fixnote", String::new);
    let push_ref = head_ref.clone();

    rsx! {
        div { class: "review-validate-bar review-fix-bar",
            div { class: "review-validate-summary",
                label { class: "review-validate-label", "Send it back with feedback" }
                textarea {
                    class: "review-summary-edit",
                    rows: "2",
                    placeholder: "What should the agent do differently?",
                    // Uncontrolled (see `detail/chat.rs`): a controlled `value` re-emits a
                    // DOM patch a frame after each keystroke, which parks the caret at the
                    // end whenever macOS splits one keystroke into two mutations (dead
                    // keys, smart quotes). Nothing but this field writes it while mounted.
                    initial_value: "{note.peek()}",
                    oninput: move |e| note.set(e.value()),
                }
            }
            div { class: "review-validate-actions",
                button {
                    class: "btn",
                    title: "Run the agent again, keeping the commits it already made",
                    onclick: move |_| {
                        state.revise_review_fix(review_id, note.read().clone());
                        super::drafts::forget(review_id, "review.fixnote");
                        dismiss();
                    },
                    "Redo"
                }
                button {
                    class: "btn subtle",
                    title: "Abandon the fix and tell the author it isn't coming",
                    onclick: move |_| super::confirm_discard_review_fix(review_id),
                    "Discard"
                }
                button {
                    class: "btn primary",
                    onclick: move |_| {
                        super::confirm_push_review_fix(state, review_id, push_ref.clone());
                    },
                    "Push to {head_ref}"
                }
            }
        }
    }
}

/// A `rows` estimate for a comment box. Counts hard line breaks and allows one
/// wrapped line per ~72 characters.
fn fallback_rows(body: &str) -> usize {
    let rows: usize = body.lines().map(|l| 1 + l.len() / 72).sum();
    rows.clamp(2, 24)
}

/// The stable `<select>` value for a review verdict.
pub(crate) fn event_value(e: ReviewEvent) -> &'static str {
    match e {
        ReviewEvent::Comment => "comment",
        ReviewEvent::RequestChanges => "request_changes",
        ReviewEvent::Approve => "approve",
    }
}

pub(crate) fn parse_event(s: &str) -> ReviewEvent {
    match s {
        "approve" => ReviewEvent::Approve,
        "request_changes" => ReviewEvent::RequestChanges,
        _ => ReviewEvent::Comment,
    }
}

/// A file's base name (last path segment) for the compact left list.
fn file_name(f: &DiffFile) -> String {
    let full = match (&f.old_path, &f.new_path) {
        (_, Some(n)) => n,
        (Some(o), None) => o,
        (None, None) => return "(unknown)".to_string(),
    };
    full.rsplit('/').next().unwrap_or(full).to_string()
}

/// A file's displayed path: `old → new` for a rename, otherwise the side that
/// exists.
fn file_path(f: &DiffFile) -> String {
    match (&f.old_path, &f.new_path) {
        (Some(o), Some(n)) if o != n => format!("{o} → {n}"),
        (_, Some(n)) => n.clone(),
        (Some(o), None) => o.clone(),
        (None, None) => "(unknown)".to_string(),
    }
}

fn status_glyph(s: FileStatus) -> &'static str {
    match s {
        FileStatus::Added => "A",
        FileStatus::Deleted => "D",
        FileStatus::Modified => "M",
        FileStatus::Renamed => "R",
        FileStatus::Copied => "C",
    }
}

fn status_class(s: FileStatus) -> &'static str {
    match s {
        FileStatus::Added => "add",
        FileStatus::Deleted => "del",
        _ => "mod",
    }
}

fn line_class(k: DiffLineKind) -> &'static str {
    match k {
        DiffLineKind::Added => "add",
        DiffLineKind::Removed => "del",
        DiffLineKind::Context => "ctx",
    }
}

fn sign(k: DiffLineKind) -> &'static str {
    match k {
        DiffLineKind::Added => "+",
        DiffLineKind::Removed => "−",
        DiffLineKind::Context => " ",
    }
}

fn gutter(n: Option<u32>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}
