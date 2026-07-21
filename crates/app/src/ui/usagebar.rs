//! The bottom usage bar: each provider's session + weekly rate-limit windows
//! as small colored gauges. Fully conditional — a zero or absent window renders
//! nothing, a provider with no visible window renders nothing, and with neither
//! provider visible the bar itself doesn't mount, so it costs no space (and in
//! demo mode, where usage is never polled, it never appears).

use std::time::{SystemTime, UNIX_EPOCH};

use dioxus::prelude::*;
use usine_core::{Provider, ProviderUsage, RateLimitWindow};

use crate::state::AppState;
use crate::ui::widgets::provider_value;

#[component]
pub fn UsageBar() -> Element {
    let state = use_context::<AppState>();
    let snapshot = state.usage.read().clone();
    let claude = snapshot.claude.filter(has_visible_window);
    let codex = snapshot.codex.filter(has_visible_window);
    if claude.is_none() && codex.is_none() {
        return rsx! {};
    }
    let claude = claude.map(|usage| {
        rsx! {
            ProviderGauges { provider: Provider::Claude, usage }
        }
    });
    let codex = codex.map(|usage| {
        rsx! {
            ProviderGauges { provider: Provider::Codex, usage }
        }
    });
    let tooltip = match snapshot
        .refreshed_at
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(t) => format!(
            "Last refreshed at {} — click to refresh now",
            t.with_timezone(&chrono::Local).format("%H:%M")
        ),
        None => "Refresh usage".to_string(),
    };

    rsx! {
        div { class: "usage-bar",
            {claude}
            {codex}
            button {
                class: "usage-refresh",
                title: "{tooltip}",
                onclick: move |_| state.refresh_usage(),
                "↻"
            }
        }
    }
}

fn has_visible_window(usage: &ProviderUsage) -> bool {
    shown(usage.session.as_ref()) || shown(usage.weekly.as_ref())
}

/// "Conditionally if non-zero": a window at 0% says nothing worth a gauge.
fn shown(window: Option<&RateLimitWindow>) -> bool {
    window.is_some_and(|w| w.used_percent > 0.0)
}

#[component]
fn ProviderGauges(provider: Provider, usage: ProviderUsage) -> Element {
    let name = provider.label();
    let class = provider_value(provider);
    let session = usage
        .session
        .filter(|w| w.used_percent > 0.0)
        .map(|w| rsx! { WindowGauge { label: "5h", window: w } });
    let weekly = usage
        .weekly
        .filter(|w| w.used_percent > 0.0)
        .map(|w| rsx! { WindowGauge { label: "7d", window: w } });

    rsx! {
        div { class: "usage-group",
            span { class: "badge provider {class}", "{name}" }
            {session}
            {weekly}
        }
    }
}

#[component]
fn WindowGauge(label: String, window: RateLimitWindow) -> Element {
    let pct = window.used_percent.clamp(0.0, 100.0);
    let level = if pct >= 80.0 {
        "high"
    } else if pct >= 50.0 {
        "warn"
    } else {
        "ok"
    };
    // Claude reports a pre-formatted local time; Codex a unix timestamp that
    // reads better as a countdown.
    let reset = window
        .resets_text
        .clone()
        .or_else(|| window.resets_at.map(countdown));

    rsx! {
        div { class: "usage-gauge",
            span { class: "usage-label", "{label}" }
            div { class: "usage-meter",
                div { class: "usage-fill {level}", style: "width: {pct}%;" }
            }
            span { class: "usage-pct {level}", "{pct as u32}%" }
            if let Some(reset) = reset {
                span { class: "usage-reset", "resets {reset}" }
            }
        }
    }
}

/// `resets_at` (unix seconds) as a countdown: "in 2d 4h", "in 3h 12m", "in 45m".
fn countdown(resets_at: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut left = (resets_at - now).max(0);
    let days = left / 86_400;
    left %= 86_400;
    let hours = left / 3_600;
    let minutes = (left % 3_600) / 60;
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {minutes:02}m")
    } else {
        format!("in {minutes}m")
    }
}
