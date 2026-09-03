//! Small reusable form widgets shared by the card config form and the global
//! settings panel.

use dioxus::prelude::*;
use usine_core::{supported_efforts, Effort, ModelSpec, Provider, SEVERITY_LEVELS};

/// Selectable model ids per provider.
pub(crate) fn models_for(provider: Provider) -> &'static [&'static str] {
    match provider {
        Provider::Claude => &["opus", "sonnet", "haiku", "fable", "claude-fable-5-1"],
        // Models available through Codex with ChatGPT authentication, newest
        // first. Older entries remain selectable for plan-dependent access.
        Provider::Codex => &[
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4-mini",
            "gpt-5.3-codex",
            "gpt-5.4",
        ],
    }
}

/// Display text for a model id. Ids that are already friendly (the Claude
/// aliases, the Codex ids) print as-is.
pub(crate) fn model_label(model: &str) -> &str {
    match model {
        "claude-fable-5-1" => "fable 5.1",
        other => other,
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
        "ultra" => Effort::Ultra,
        _ => Effort::Medium,
    }
}

/// The criticality of a drafted review comment, as an editable pill. The
/// maintainer owns this rating — it is published to the contributor alongside
/// the comment, so they can correct one they disagree with, or pick `\u{2014}`
/// to clear it and post the comment untagged.
///
/// One component for both validation surfaces (the detail panel's list and the
/// diff viewer's inline thread) so the two never drift apart.
#[component]
pub(crate) fn SeverityPicker(severity: String, on_change: EventHandler<String>) -> Element {
    let class = if severity.is_empty() {
        "sev".to_string()
    } else {
        format!("sev sev-{severity}")
    };
    rsx! {
        select {
            class: "{class}",
            value: "{severity}",
            title: "Criticality published with this comment",
            "aria-label": "Criticality",
            onchange: move |e: Event<FormData>| on_change.call(e.value()),
            option { value: "", selected: severity.is_empty(), "\u{2014}" }
            for level in SEVERITY_LEVELS.iter() {
                option { value: "{level}", selected: severity.as_str() == *level, "{level}" }
            }
        }
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
                    option { value: "{m}", selected: model.as_str() == *m, "{model_label(m)}" }
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
                    option { value: "{m}", selected: model.as_str() == *m, "{model_label(m)}" }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_picker_lists_the_gpt_5_6_family() {
        let models = models_for(Provider::Codex);
        assert!(models.contains(&"gpt-5.6-sol"));
        assert!(models.contains(&"gpt-5.6-terra"));
        assert!(models.contains(&"gpt-5.6-luna"));
    }

    #[test]
    fn claude_picker_pins_fable_5_1_while_keeping_the_alias() {
        let models = models_for(Provider::Claude);
        assert!(models.contains(&"claude-fable-5-1"));
        // Dropping the bare alias would strand cards already configured on it:
        // the select would render blank and silently switch model on next edit.
        assert!(models.contains(&"fable"));
    }

    #[test]
    fn only_the_pinned_ids_get_a_friendlier_label() {
        assert_eq!(model_label("claude-fable-5-1"), "fable 5.1");
        assert_eq!(model_label("opus"), "opus");
        assert_eq!(model_label("gpt-5.6-sol"), "gpt-5.6-sol");
    }

    #[test]
    fn ultra_effort_is_parsed() {
        assert_eq!(parse_effort("ultra"), Effort::Ultra);
    }
}
