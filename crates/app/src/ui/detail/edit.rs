//! Starting-block editing panels: task title/description, image attachments,
//! and the run configuration form.

use dioxus::prelude::*;
use usine_core::{Card, CardConfig, CardKind, ExecutorCommand, Provider};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::widgets::{
    parse_provider, provider_value, ModelEffortPicker, OptionalModelEffortPicker,
};

#[component]
pub(super) fn EditableTask(card: Card) -> Element {
    let state = use_context::<AppState>();
    let id = card.id;
    // Local copy of the description so the textarea can auto-grow as you type
    // (the `data-replicated-value` mirror drives the wrapper height); saved on blur.
    let mut desc = use_signal(|| card.description.clone());

    rsx! {
        div { class: "section",
            h3 { "Task" }
            div { class: "field",
                label { r#for: "task-title", "Title" }
                input {
                    id: "task-title",
                    value: "{card.title}",
                    placeholder: "Card title…",
                    onchange: move |e| state.update_card(id, |c| c.title = e.value()),
                }
            }
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
// Image attachments (Claude-only)
// ---------------------------------------------------------------------------

#[component]
pub(super) fn Attachments(card_id: Uuid, provider: Provider) -> Element {
    let state = use_context::<AppState>();
    // Claude reads attached images via its (vision-capable) Read tool; Codex has
    // no equivalent path today, so the section only shows for Claude cards.
    if provider != Provider::Claude {
        return rsx! {};
    }
    let atts = state.card_attachments(card_id);

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
                            // Stored names are `<8 hex>-<original>`; show the original part.
                            let name = path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let label = name.split_once('-').map(|x| x.1).unwrap_or(&name).to_string();
                            let to_remove = path.clone();
                            rsx! {
                                span { key: "{key}", class: "chip",
                                    span { class: "chip-label", title: "{name}", "{label}" }
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
