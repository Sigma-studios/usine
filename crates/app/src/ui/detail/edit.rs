//! Starting-block editing panels: task description, image attachments, and
//! the run configuration form. The title is edited in the panel header
//! (`EditableTitle` in the parent module), which works in every card state.

use std::path::PathBuf;

use base64::Engine as _;
use dioxus::prelude::*;
use usine_core::{Card, CardConfig, CardKind, ExecutorCommand};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::widgets::{
    parse_provider, provider_value, ModelEffortPicker, OptionalModelEffortPicker,
};

#[component]
pub(super) fn EditableTask(card: Card) -> Element {
    let state = use_context::<AppState>();
    let id = card.id;
    // Local copy of the description, saved on blur; it also drives the
    // auto-grow wrapper (`data-replicated-value`).
    let mut desc = use_signal(|| card.description.clone());
    // Deselecting (or a state change) unmounts this panel before blur can fire,
    // which would silently drop whatever was typed — commit on unmount too.
    // Comparing against the mount-time value keeps an untouched field from
    // overwriting an edit that arrived from elsewhere; a blur-then-unmount
    // double save is idempotent.
    let original_desc = card.description.clone();
    use_drop(move || {
        let d = desc.peek().clone();
        if d != original_desc {
            state.update_card(id, |c| c.description = d);
        }
    });

    rsx! {
        div { class: "section",
            h3 { "Task" }
            div { class: "field",
                label { r#for: "task-desc", "Description" }
                div { class: "grow-wrap", "data-replicated-value": "{desc}",
                    textarea {
                        id: "task-desc",
                        value: "{desc}",
                        placeholder: "What should the agent do? (paste a screenshot to attach it)",
                        oninput: move |e| desc.set(e.value()),
                        onchange: move |e| state.update_card(id, |c| c.description = e.value()),
                        // Paste an image from the clipboard to attach it (text
                        // paste is unaffected — we only act when the clipboard
                        // actually holds an image).
                        onpaste: move |_| {
                            if let Some(png) = clipboard_image_png() {
                                state.attach_image_bytes(id, png);
                            }
                        },
                    }
                }
            }
        }
    }
}

/// Read an image off the OS clipboard and PNG-encode it, if one is present.
/// Returns `None` for a text paste (or when the clipboard has no image), so the
/// caller can safely try this on every paste. Uses the native clipboard rather
/// than the webview paste payload, which doesn't reliably expose image bytes.
fn clipboard_image_png() -> Option<Vec<u8>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let img = clipboard.get_image().ok()?;
    let buf =
        image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

// ---------------------------------------------------------------------------
// Image attachments
// ---------------------------------------------------------------------------

/// Both providers can see attached images: Claude reads them with its
/// vision-capable Read tool, Codex receives them as `codex exec -i` flags. Non-
/// image files ride as prompt-listed paths either way (Codex's `-i` filter only
/// covers png/jpg/jpeg/gif/webp), which both CLIs read with their shell tools.
#[component]
pub(super) fn Attachments(card_id: Uuid) -> Element {
    let state = use_context::<AppState>();
    let atts = state.card_attachments(card_id);
    // Image chip clicked → preview it in a modal; None when the dialog is closed.
    let mut preview = use_signal(|| None::<PathBuf>);

    rsx! {
        div { class: "section",
            h3 { "Attachments" }
            if atts.is_empty() {
                div { class: "hint", "No files attached." }
            } else {
                div { class: "chips",
                    for path in atts {
                        {
                            let key = path.to_string_lossy().into_owned();
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let label = usine_core::infra::paths::attachment_label(&path);
                            let to_remove = path.clone();
                            let is_image = image_mime(&path).is_some();
                            let to_preview = path.clone();
                            rsx! {
                                span { key: "{key}", class: "chip",
                                    if is_image {
                                        span {
                                            class: "chip-label clickable",
                                            title: "{name}",
                                            onclick: move |_| preview.set(Some(to_preview.clone())),
                                            "{label}"
                                        }
                                    } else {
                                        span { class: "chip-label", title: "{name}", "{label}" }
                                    }
                                    button {
                                        class: "chip-remove",
                                        title: "Remove",
                                        onclick: move |_| state.detach_image(card_id, to_remove.clone()),
                                        "×"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(path) = preview() {
                AttachmentPreview { path, on_close: move |_| preview.set(None) }
            }
            button {
                class: "btn",
                onclick: move |_| {
                    spawn(async move {
                        if let Some(handles) = rfd::AsyncFileDialog::new().pick_files().await {
                            let paths = handles.iter().map(|h| h.path().to_path_buf()).collect();
                            state.attach_images(card_id, paths);
                        }
                    });
                },
                "Attach file"
            }
        }
    }
}

/// The mime type for an attachment path, if its extension is a web-renderable
/// image — only those chips open the preview dialog.
fn image_mime(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

/// Simple modal previewing an image attachment (named to avoid colliding with
/// the card "preview" concept). Same dismiss/focus behaviour as `ConfirmHost`.
#[component]
fn AttachmentPreview(path: PathBuf, on_close: EventHandler<()>) -> Element {
    let label = usine_core::infra::paths::attachment_label(&path);
    // Attachments live outside any served root, so inline the bytes as a data
    // URL (the same trick as `LOGO_URI`). None = the file could not be read.
    let src = use_memo(use_reactive!(|path| {
        let mime = image_mime(&path)?;
        let bytes = std::fs::read(&path).ok()?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        Some(format!("data:{mime};base64,{b64}"))
    }));

    rsx! {
        div { class: "modal-overlay", onclick: move |_| on_close.call(()),
            div {
                class: "modal image-preview",
                "role": "dialog",
                "aria-modal": "true",
                tabindex: "-1",
                // Don't dismiss when clicking inside the dialog.
                onclick: move |e| e.stop_propagation(),
                // Escape closes the dialog (matches the overlay-click behaviour).
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        on_close.call(());
                    }
                },
                // Focus the dialog on open so Escape works and focus is trapped here.
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
                h3 { class: "modal-title", "{label}" }
                div { class: "modal-body",
                    if let Some(src) = src() {
                        img { src: "{src}", alt: "{label}" }
                    } else {
                        div { class: "hint", "Could not read {label}." }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Config form
// ---------------------------------------------------------------------------

/// The three ways a card can run, folded from its kind + skip-plan flag. The
/// selector below writes both: `kind` on the card's config, `skip_plan` in its
/// persisted side flag.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Plan first, then implement (the default).
    Design,
    /// Implement straight from the description, no plan phase.
    Direct,
    /// Read-only investigation ending in a conclusion — no code changes.
    Investigate,
}

#[component]
pub(super) fn ConfigForm(card: Card) -> Element {
    let state = use_context::<AppState>();
    let id = card.id;
    let provider = card.config.provider;
    // "Skip plan" mark — persisted, settable only here (before the card starts).
    let mut skip = use_signal(|| state.card_skip_plan(id));
    // Auto self-review toggle — persisted like skip-plan, on by default.
    let mut auto_review = use_signal(|| state.card_auto_review(id));
    let mode = if card.config.kind == CardKind::Investigation {
        Mode::Investigate
    } else if skip() {
        Mode::Direct
    } else {
        Mode::Design
    };
    let start_label = match mode {
        Mode::Design => "Start designing",
        Mode::Direct => "Start implementing",
        Mode::Investigate => "Start investigation",
    };
    let mode_btn = |m: Mode| if mode == m { "btn primary" } else { "btn" };

    rsx! {
        div { class: "section",
            h3 { "Configuration" }
            div { class: "field",
                label { r#for: "config-provider", "Provider" }
                select {
                    id: "config-provider",
                    value: provider_value(provider),
                    onchange: move |e: Event<FormData>| {
                        // Switching provider resets the models to that provider's
                        // defaults so the selectors always show valid options —
                        // but keeps the chosen mode (kind) intact.
                        state.update_card(id, |c| {
                            let kind = c.config.kind;
                            c.config = CardConfig::default_for(parse_provider(&e.value()));
                            c.config.kind = kind;
                        });
                    },
                    option { value: "claude", selected: provider_value(provider) == "claude", "Claude" }
                    option { value: "codex", selected: provider_value(provider) == "codex", "Codex" }
                }
            }
            div { class: "field",
                label { "Mode" }
                div { class: "option-row",
                    button {
                        class: mode_btn(Mode::Design),
                        onclick: move |_| {
                            state.update_card(id, |c| c.config.kind = CardKind::Task);
                            skip.set(false);
                            state.set_card_skip_plan(id, false);
                        },
                        "Design + implement"
                    }
                    button {
                        class: mode_btn(Mode::Direct),
                        onclick: move |_| {
                            state.update_card(id, |c| c.config.kind = CardKind::Task);
                            skip.set(true);
                            state.set_card_skip_plan(id, true);
                        },
                        "Implement directly"
                    }
                    button {
                        class: mode_btn(Mode::Investigate),
                        onclick: move |_| {
                            state.update_card(id, |c| c.config.kind = CardKind::Investigation);
                        },
                        "Investigate only"
                    }
                }
                if mode == Mode::Investigate {
                    div { class: "hint",
                        "Read-only: the agent audits the code and returns a conclusion — no changes, no branch, no PR."
                    }
                }
            }
            if mode != Mode::Investigate {
                // Investigations are read-only: nothing to self-review.
                label { class: "checkbox-row",
                    input {
                        r#type: "checkbox",
                        checked: auto_review(),
                        onchange: move |_| {
                            let v = !auto_review();
                            auto_review.set(v);
                            state.set_card_auto_review(id, v);
                        },
                    }
                    span { "Self-review automatically when the implementation finishes" }
                }
            }
            if mode == Mode::Investigate {
                div { class: "field",
                    label { "Investigate phase" }
                    ModelEffortPicker {
                        provider,
                        spec: card.config.plan.clone(),
                        on_change: move |spec| state.update_card(id, |c| c.config.plan = spec),
                    }
                }
            } else {
                if mode == Mode::Design {
                    div { class: "field",
                        label { "Plan phase" }
                        ModelEffortPicker {
                            provider,
                            spec: card.config.plan.clone(),
                            on_change: move |spec| state.update_card(id, |c| c.config.plan = spec),
                        }
                    }
                }
                div { class: "field",
                    label { "Implement phase" }
                    ModelEffortPicker {
                        provider,
                        spec: card.config.implement.clone(),
                        on_change: move |spec| state.update_card(id, |c| c.config.implement = spec),
                    }
                }
                div { class: "field",
                    label { "Review phase" }
                    OptionalModelEffortPicker {
                        provider,
                        spec: card.config.review.clone(),
                        inherit_label: "Same as implement",
                        on_change: move |spec| state.update_card(id, |c| c.config.review = spec),
                    }
                    div { class: "hint", "Self-review and PR-comment triage." }
                }
            }
            button {
                class: "btn primary",
                onclick: move |_| state.send(ExecutorCommand::Start { card_id: id }),
                "{start_label}"
            }
        }
    }
}
