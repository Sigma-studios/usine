//! The bottom usage bar: each provider's session + weekly rate-limit windows
//! as small colored gauges, plus — where a model family is billed against its
//! own weekly cap (Claude's Fable) — a third gauge for that per-model window.
//! The gauges are conditional — a zero or absent window renders nothing, and a
//! provider with no visible window renders nothing — but the bar itself is
//! always mounted: with nothing to draw it shows a short placeholder, so its
//! refresh button stays reachable exactly when the numbers never arrived. In
//! demo mode the gauges show the simulator's mock numbers, which the button
//! re-rolls.

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
    // The click currently in flight, if any: without disabling the button,
    // clicks fan out parallel `claude -p /usage` runs, since `RefreshUsage` is
    // neither exclusive nor persistence and the executor's dispatch loop spawns
    // every copy concurrently. Each click carries its own generation so a
    // *later* click owns the button outright — the earlier one's safety timer
    // and any snapshot older than it can no longer clear the spinner.
    let mut clicks = use_signal(|| 0u32);
    let mut pending = use_signal(|| None::<Pending>);
    // Every refresh emits `UsageUpdated`, which is what retires the spinner.
    // The 15-min background poll emits the same event, and a snapshot it lands
    // mid-click counts too: the numbers on screen really are fresher than the
    // click, so the wait is over either way.
    use_effect(move || {
        let refreshed_at = state.usage.read().refreshed_at;
        if refresh_landed(*pending.peek(), refreshed_at) {
            pending.set(None);
        }
    });
    let snapshot = state.usage.read().clone();
    let claude = snapshot.claude.filter(has_visible_window);
    let codex = snapshot.codex.filter(has_visible_window);
    let empty = claude.is_none() && codex.is_none();
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
    let empty_msg = empty.then(|| empty_message(snapshot.refreshed_at, pending().is_some()));

    rsx! {
        div { class: "usage-bar has-tip",
            if let Some(msg) = empty_msg {
                span { class: "usage-empty", "{msg}" }
            }
            {claude}
            {codex}
            button {
                class: "usage-refresh",
                aria_label: "Refresh usage now",
                disabled: pending().is_some(),
                onclick: move |_| {
                    clicks += 1;
                    let generation = clicks();
                    pending.set(Some(Pending { generation, clicked_at: now_millis() }));
                    state.refresh_usage();
                    // Safety net for the real path: if no event ever comes back
                    // (executor gone, channel dropped) the button un-sticks on
                    // its own, well past the CLI fetch's own timeout — but only
                    // for the click that armed it, never a newer one's spinner.
                    spawn(async move {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        if pending.peek().is_some_and(|p| p.generation == generation) {
                            pending.set(None);
                        }
                    });
                },
                if pending().is_some() {
                    span { class: "spinner" }
                } else {
                    "↻"
                }
            }
            span { class: "info-tip up", "{label}" }
        }
    }
}

/// A refresh click waiting for its `UsageUpdated`, identified so that neither a
/// stale safety timer nor a snapshot from before the click can retire it.
#[derive(Clone, Copy, PartialEq)]
struct Pending {
    generation: u32,
    clicked_at: i64,
}

/// Whether an incoming snapshot ends the wait: only a snapshot refreshed at or
/// after the click was made — one that predates it says nothing about it.
fn refresh_landed(pending: Option<Pending>, refreshed_at: Option<i64>) -> bool {
    match (pending, refreshed_at) {
        (Some(pending), Some(at)) => at >= pending.clicked_at,
        _ => false,
    }
}

/// What the bar says when there are no gauges to draw: the first poll fires
/// immediately at launch, so "never refreshed" still means "in flight".
fn empty_message(refreshed_at: Option<i64>, pending: bool) -> &'static str {
    if pending || refreshed_at.is_none() {
        "Checking usage…"
    } else {
        "No usage data"
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
    shown(usage.session.as_ref())
        || shown(usage.weekly.as_ref())
        || shown(usage.weekly_model.as_ref().map(|(_, w)| w))
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
    // A model family billed against its own weekly cap (Claude's Fable), which
    // the all-models gauge above says nothing about.
    let weekly_model = usage
        .weekly_model
        .filter(|(_, w)| w.used_percent > 0.0)
        .map(|(name, w)| rsx! { WindowGauge { label: "7d {name}", window: w } });

    rsx! {
        div { class: "usage-group",
            span { class: "badge provider {class}", "{name}" }
            {session}
            {weekly}
            {weekly_model}
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
    fn an_empty_bar_reads_as_in_flight_until_a_refresh_lands() {
        assert_eq!(empty_message(None, false), "Checking usage…");
        assert_eq!(empty_message(None, true), "Checking usage…");
        assert_eq!(empty_message(Some(NOW), true), "Checking usage…");
        assert_eq!(empty_message(Some(NOW), false), "No usage data");
    }

    #[test]
    fn only_a_snapshot_from_after_the_click_retires_the_spinner() {
        let pending = Pending {
            generation: 1,
            clicked_at: NOW,
        };
        assert!(!refresh_landed(Some(pending), None));
        assert!(!refresh_landed(Some(pending), Some(NOW - 1)));
        assert!(refresh_landed(Some(pending), Some(NOW)));
        assert!(refresh_landed(Some(pending), Some(NOW + 5_000)));
        // Nothing in flight: an incoming snapshot has no spinner to clear.
        assert!(!refresh_landed(None, Some(NOW)));
    }

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

    #[test]
    fn a_per_model_weekly_cap_alone_still_shows_the_provider_segment() {
        // An account that has only touched Fable this week reports 0% on both
        // shared windows; the segment must not vanish.
        let usage = ProviderUsage {
            session: None,
            weekly: None,
            weekly_model: Some((
                "Fable".into(),
                RateLimitWindow {
                    used_percent: 6.0,
                    ..Default::default()
                },
            )),
        };
        assert!(has_visible_window(&usage));

        // A 0% per-model window says nothing worth a gauge, like the others.
        let idle = ProviderUsage {
            weekly_model: Some(("Fable".into(), RateLimitWindow::default())),
            ..Default::default()
        };
        assert!(!has_visible_window(&idle));
    }
}
