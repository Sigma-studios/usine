//! The bottom usage bar: each provider's session + weekly rate-limit windows
//! as small colored gauges. Fully conditional — a zero or absent window renders
//! nothing, a provider with no visible window renders nothing, and with neither
//! provider visible the bar itself doesn't mount, so it costs no space (and in
//! demo mode, where usage is never polled, it never appears).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dioxus::prelude::*;
use usine_core::{now_millis, Provider, ProviderUsage, RateLimitWindow};

use crate::state::AppState;
use crate::ui::widgets::provider_value;

#[component]
pub fn UsageBar() -> Element {
    let state = use_context::<AppState>();
    // The poll only runs every 15 min, so the relative half of the label would
    // sit frozen at "just now"; re-render every 30s to let it advance.
    let mut tick = use_signal(|| 0u32);
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            tick += 1;
        }
    });
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
    // Read the ticker so the label re-renders with it.
    let _ = tick();
    let label = refreshed_label(snapshot.refreshed_at, now_millis());

    rsx! {
        div { class: "usage-bar has-tip",
            {claude}
            {codex}
            button {
                class: "usage-refresh",
                aria_label: "Refresh usage now",
                onclick: move |_| state.refresh_usage(),
                "↻"
            }
            span { class: "info-tip up", "{label}" }
        }
    }
}

/// The bar's hover tooltip: how long ago the rate-limit numbers were refreshed,
/// plus the wall-clock time ("Updated 3 min ago (14:32)").
fn refreshed_label(refreshed_at: Option<i64>, now_ms: i64) -> String {
    let Some(at) = refreshed_at.and_then(chrono::DateTime::from_timestamp_millis) else {
        return "Usage not refreshed yet".to_string();
    };
    // Clock skew (or a snapshot from the future) reads as fresh, not negative.
    let elapsed = (now_ms - at.timestamp_millis()).max(0) / 1_000;
    let relative = if elapsed < 60 {
        "just now".to_string()
    } else if elapsed < 3_600 {
        format!("{} min ago", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h ago", elapsed / 3_600)
    } else {
        format!("{}d ago", elapsed / 86_400)
    };
    let fmt = if elapsed < 86_400 {
        "%H:%M"
    } else {
        "%b %d, %H:%M"
    };
    let absolute = at.with_timezone(&chrono::Local).format(fmt);
    format!("Updated {relative} ({absolute})")
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn never_refreshed_says_so() {
        assert_eq!(refreshed_label(None, NOW), "Usage not refreshed yet");
    }

    #[test]
    fn recent_refreshes_read_as_just_now() {
        let label = refreshed_label(Some(NOW - 10_000), NOW);
        assert!(label.starts_with("Updated just now ("), "{label}");
        assert!(label.ends_with(')'), "{label}");
    }

    #[test]
    fn elapsed_time_is_bucketed() {
        for (ago_ms, expected) in [
            (3 * 60_000, "Updated 3 min ago ("),
            (2 * 3_600_000, "Updated 2h ago ("),
            (2 * 86_400_000, "Updated 2d ago ("),
        ] {
            let label = refreshed_label(Some(NOW - ago_ms), NOW);
            assert!(label.starts_with(expected), "{label}");
            assert!(label.ends_with(')'), "{label}");
        }
    }

    #[test]
    fn a_future_timestamp_is_clamped_to_fresh() {
        let label = refreshed_label(Some(NOW + 60_000), NOW);
        assert!(label.starts_with("Updated just now ("), "{label}");
    }
}
