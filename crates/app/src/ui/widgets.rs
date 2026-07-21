//! Small reusable form widgets shared by the card config form and the global
//! settings panel.

use dioxus::prelude::*;
use usine_core::{supported_efforts, Effort, ModelSpec, Provider};

/// Selectable model ids per provider.
pub(crate) fn models_for(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &["opus", "sonnet", "haiku", "fable"],
        // The pre-5.3 Codex lineup shuts down 2026-07-23; this is the surviving
        // set (minus the Pro-only `gpt-5.3-codex-spark`). Working-on-ChatGPT-auth
        // models first (verified live Jul 2026): gpt-5.5 and gpt-5.4-mini are
        // accepted; gpt-5.3-codex and gpt-5.4 are rejected on ChatGPT accounts
        // but kept selectable in case that gate changes per plan.
        Provider::Codex => &["gpt-5.5", "gpt-5.4-mini", "gpt-5.3-codex", "gpt-5.4"],
    }
}

pub(crate) fn provider_value(p: Provider) -> &'static str {
    match p {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

pub(crate) fn parse_provider(s: &str) -> Provider {
    match s {
        "codex" => Provider::Codex,
        _ => Provider::Claude,
    }
}

pub(crate) fn parse_effort(s: &str) -> Effort {
    match s {
        "low" => Effort::Low,
        "high" => Effort::High,
        "xhigh" => Effort::XHigh,
        "max" => Effort::Max,
        _ => Effort::Medium,
    }
}

/// A model dropdown + effort dropdown. Emits the updated [`ModelSpec`] on change.
#[component]
pub(crate) fn ModelEffortPicker(
    provider: Provider,
    spec: ModelSpec,
    on_change: EventHandler<ModelSpec>,
) -> Element {
    let models = models_for(provider);
    let model = spec.model.clone();
    let effort = spec.effort;

    rsx! {
        div { class: "row",
            select {
                value: "{model}",
                onchange: {
                    let spec = spec.clone();
                    // Switching models can strip the current effort (e.g. leaving a
                    // non-max Codex model on `xhigh`), so clamp it to the new model.
                    move |e: Event<FormData>| {
                        let model = e.value();
                        let effort = spec.effort.clamp_to(supported_efforts(provider, &model));
                        on_change.call(ModelSpec { model, effort })
                    }
                },
                for m in models.iter() {
                    option { value: "{m}", selected: model.as_str() == *m, "{m}" }
                }
            }
            select {
                value: effort.label(),
                onchange: {
                    let spec = spec.clone();
                    move |e: Event<FormData>| on_change.call(ModelSpec { model: spec.model.clone(), effort: parse_effort(&e.value()) })
                },
                for ef in supported_efforts(provider, &model).iter().copied() {
                    option { value: ef.label(), selected: ef == effort, "{ef.label()}" }
                }
            }
        }
    }
}

/// A [`ModelEffortPicker`] with a leading "inherit" option that clears the
/// override, emitting `None`. Used for the review phase, which falls back to the
/// implement spec when unset. The effort dropdown is hidden while inheriting —
/// there's no independent effort to show, it comes from whatever is inherited.
#[component]
pub(crate) fn OptionalModelEffortPicker(
    provider: Provider,
    spec: Option<ModelSpec>,
    /// Label for the inherit option, e.g. "Same as implement".
    inherit_label: String,
    on_change: EventHandler<Option<ModelSpec>>,
) -> Element {
    let models = models_for(provider);
    let model = spec.as_ref().map(|s| s.model.clone()).unwrap_or_default();
    let effort = spec.as_ref().map(|s| s.effort);

    rsx! {
        div { class: "row",
            select {
                value: "{model}",
                onchange: {
                    let spec = spec.clone();
                    move |e: Event<FormData>| {
                        let model = e.value();
                        if model.is_empty() {
                            on_change.call(None);
                            return;
                        }
                        // Coming from inherit there's no effort to carry over, so
                        // seed Medium — the one tier every model offers. Clamp
                        // regardless, for the same reason the picker above does.
                        let effort = spec
                            .as_ref()
                            .map(|s| s.effort)
                            .unwrap_or(Effort::Medium)
                            .clamp_to(supported_efforts(provider, &model));
                        on_change.call(Some(ModelSpec { model, effort }))
                    }
                },
                option { value: "", selected: model.is_empty(), "{inherit_label}" }
                for m in models.iter() {
                    option { value: "{m}", selected: model.as_str() == *m, "{m}" }
                }
            }
            if let Some(effort) = effort {
                select {
                    value: effort.label(),
                    onchange: {
                        let model = model.clone();
                        move |e: Event<FormData>| on_change.call(Some(ModelSpec { model: model.clone(), effort: parse_effort(&e.value()) }))
                    },
                    for ef in supported_efforts(provider, &model).iter().copied() {
                        option { value: ef.label(), selected: ef == effort, "{ef.label()}" }
                    }
                }
            }
        }
    }
}
