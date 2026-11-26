//! The bottom usage bar: each provider's session + weekly rate-limit windows
//! as small colored gauges, plus — where a model family is billed against its
//! own weekly cap (Claude's Fable) — a third gauge for that per-model window.
//! The gauges are conditional — a zero or absent window renders nothing, and a
//! provider with no visible window renders nothing — but the bar itself is
//! always mounted: with nothing to draw it shows a short placeholder, so its
//! refresh button stays reachable exactly when the numbers never arrived. In
//! demo mode the gauges show the simulator's mock numbers, which the button
//! re-rolls.
//!
//! Next to each provider's badge sits a global "N running" count of the agent
//! runs currently executing on it (every project's cards, PR review tasks and
//! one-off sessions), hidden at zero. A provider with running agents but no
//! visible window still gets its segment, holding just the badge and the count.
//!
//! The far right holds the update control (`ui/update.rs`).

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dioxus::prelude::*;
use usine_core::{
    now_millis, CardState, ClaudeBilling, CreditBalance, Provider, ProviderUsage, QueuedTarget,
    RateLimitWindow, ReviewSub, Session,
};
use uuid::Uuid;

use crate::state::AppState;
use crate::ui::update::UpdateControl;
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
    // Cards, review tasks and sessions change constantly across every project;
    // the memo soaks those updates up and only re-renders the bar when a count
    // moves.
    let running = use_memo(move || {
        // Review runs launch on the default provider, so that's where they count.
        let review_provider = state.settings.read().default_provider;
        let mut runs: Vec<(Uuid, Provider)> = state
            .cards
            .read()
            .iter()
            .filter(|card| runs_agent(&card.state))
            .map(|card| (card.id, card.config.provider))
            .collect();
        runs.extend(
            state
                .review_tasks
                .read()
                .values()
                .flatten()
                .filter(|task| task.status.is_running())
                .map(|task| (task.id, review_provider)),
        );
        runs.extend(session_runs(&state.sessions.read()));
        let queued = state.run_queue.read();
        (
            running_count(Provider::Claude, &runs, &queued),
            running_count(Provider::Codex, &runs, &queued),
        )
    });
    let (claude_running, codex_running) = running();
    // The API-credits badge, only while the plan is blocked and runs bill the
    // saved key (see `CreditsBadge`).
    let billing = state.billing.read().clone();
    let credits = Some(billing.clone()).filter(|b| b.on_credits_until.is_some());
    // The "API $x left" counter, whenever a key and a balance are saved.
    let counter = Some(billing).filter(shows_counter);
    let group = |usage: Option<ProviderUsage>,
                 provider: Provider,
                 running: usize,
                 credits: Option<ClaudeBilling>,
                 counter: Option<ClaudeBilling>| {
        let usage = usage.filter(has_visible_window);
        (usage.is_some() || running > 0 || credits.is_some() || counter.is_some()).then(|| {
            let usage = usage.unwrap_or_default();
            rsx! {
                ProviderGauges { provider, usage, running, credits, counter }
            }
        })
    };
    let claude = group(
        snapshot.claude.clone(),
        Provider::Claude,
        claude_running,
        credits,
        counter,
    );
    let codex = group(
        snapshot.codex.clone(),
        Provider::Codex,
        codex_running,
        None,
        None,
    );
    let empty = claude.is_none() && codex.is_none();
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
            UpdateControl {}
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

/// Whether a card in this state has an agent process running. The validate
/// step runs the project's script, not an agent, so it doesn't count.
fn runs_agent(state: &CardState) -> bool {
    state.is_running()
        && !matches!(
            state,
            CardState::AwaitingReview(ReviewSub::Validating { .. })
        )
}

/// The `(id, provider)` pairs of the sessions with a turn in flight. Sessions
/// run on their own provider, outside the run queue, so none is ever a queued
/// launch; one waiting on an answer has already torn its process down.
fn session_runs(sessions: &HashMap<Uuid, Session>) -> impl Iterator<Item = (Uuid, Provider)> + '_ {
    sessions
        .values()
        .filter(|s| s.is_running())
        .map(|s| (s.id, s.provider))
}

/// How many of the running `(id, provider)` pairs are on `provider`. A queued
/// launch already sits in its running state but has no process yet, so it's
/// skipped until its slot frees up.
fn running_count(provider: Provider, runs: &[(Uuid, Provider)], queued: &[QueuedTarget]) -> usize {
    runs.iter()
        .filter(|(id, p)| *p == provider && !queued.iter().any(|t| t.id() == *id))
        .count()
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
fn ProviderGauges(
    provider: Provider,
    usage: ProviderUsage,
    running: usize,
    credits: Option<ClaudeBilling>,
    counter: Option<ClaudeBilling>,
) -> Element {
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
            if running > 0 {
                span { class: "usage-running", "{running} running" }
            }
            {session}
            {weekly}
            {weekly_model}
            if let Some(billing) = credits {
                CreditsBadge { billing }
            }
            if let Some(billing) = counter {
                CreditCounter { billing }
            }
        }
    }
}

/// Whether the bar shows the "API $x left" counter: a key and an entered
/// balance are both saved.
fn shows_counter(billing: &ClaudeBilling) -> bool {
    billing.fallback_configured && billing.credit_balance.is_some()
}

/// "API $12.34 left": the balance the user entered (Settings) minus what
/// usine's runs on the key spent since. Red once nothing is left — counted
/// down to zero, or the API itself said the credits ran out.
#[component]
fn CreditCounter(billing: ClaudeBilling) -> Element {
    let Some(balance) = billing.credit_balance.clone() else {
        return rsx! {};
    };
    let class = if counter_empty(&billing) {
        "usage-credits exhausted"
    } else {
        "usage-credits"
    };
    let label = counter_label(&balance);
    let tip = counter_tip(&balance);
    rsx! {
        span { class: "{class} has-tip",
            "{label}"
            span { class: "info-tip up", "{tip}" }
        }
    }
}

/// Whether the counter reads as empty (red).
fn counter_empty(billing: &ClaudeBilling) -> bool {
    billing.credits_exhausted
        || billing
            .credit_balance
            .as_ref()
            .is_some_and(|b| b.remaining() < 0.005)
}

/// The counter's label.
fn counter_label(balance: &CreditBalance) -> String {
    format!("API ${:.2} left", balance.remaining())
}

/// The counter's tooltip: what it counts from, and what it can't see.
fn counter_tip(balance: &CreditBalance) -> String {
    format!(
        "${:.2} left of ${:.2} set on {} · counts only usine's runs (the CLI's estimate at list \
         prices); spend from other apps using this key isn't seen — re-enter the balance in \
         Settings to line it back up",
        balance.remaining(),
        balance.set_usd,
        set_on(balance.set_at),
    )
}

/// When a balance was entered (unix seconds), as a short local date.
pub(crate) fn set_on(set_at: i64) -> String {
    chrono::DateTime::from_timestamp(set_at, 0)
        .map(|t| t.with_timezone(&chrono::Local).format("%b %-d").to_string())
        .unwrap_or_else(|| "an unknown date".into())
}

/// "API credits": the plan's limit was hit (or a window passed the early
/// switch point) and Claude runs bill the saved API key until it resets. Red
/// once the key's credits ran out too and the plan has refused runs; when
/// the block is only the early switch's padding, runs go back to the plan
/// instead, so the badge says the plan is nearly used. The plan's gauges stay
/// as they are — they still show the plan's own windows.
#[component]
fn CreditsBadge(billing: ClaudeBilling) -> Element {
    let (label, class) = match (billing.credits_exhausted, billing.plan_refused) {
        (true, true) => ("Credits exhausted", "usage-credits exhausted"),
        (true, false) => ("Plan nearly used", "usage-credits"),
        (false, _) => ("API credits", "usage-credits"),
    };
    let tip = credits_tip(&billing);
    rsx! {
        span { class: "{class} has-tip",
            "{label}"
            span { class: "info-tip up", "{tip}" }
        }
    }
}

/// The credits badge's tooltip.
fn credits_tip(billing: &ClaudeBilling) -> String {
    let until = billing
        .on_credits_until
        .map(countdown)
        .unwrap_or_else(|| "soon".into());
    let spent = format!("${:.2} spent by usine", billing.credit_spend_usd);
    match (billing.credits_exhausted, billing.plan_refused) {
        (true, true) => format!(
            "Plan limit reached and API credits exhausted; Claude runs fail until the plan resets \
             {until} · {spent}"
        ),
        (true, false) => format!(
            "Plan nearly used and API credits exhausted; Claude runs continue on the plan until \
             it resets {until} · {spent}"
        ),
        (false, true) => format!(
            "Plan limit reached; Claude runs bill API credits until it resets {until} · {spent}"
        ),
        (false, false) => format!(
            "Plan nearly used; new Claude runs bill API credits until it resets {until}, runs \
             already going finish on the plan · {spent}"
        ),
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
    // Both real providers report a unix timestamp, shown as a countdown; the
    // simulator's mock numbers carry a pre-formatted local time instead.
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
    fn only_sessions_with_a_turn_in_flight_count_as_running() {
        use std::path::PathBuf;
        use usine_core::{Effort, Intervention, ModelSpec, SessionAccess, SessionStatus};

        let session = |provider| {
            Session::new(
                Uuid::new_v4(),
                PathBuf::from("/tmp"),
                provider,
                ModelSpec::new("opus", Effort::High),
                SessionAccess::FullAccess,
                "x",
                vec![],
            )
        };
        let running = session(Provider::Claude);
        let mut finished = session(Provider::Codex);
        finished.status = SessionStatus::Finished;
        let mut asking = session(Provider::Codex);
        asking.status = SessionStatus::NeedsAnswer(Intervention {
            request_id: "q".into(),
            question: "?".into(),
            options: vec![],
        });
        let sessions: HashMap<Uuid, Session> = [running.clone(), finished, asking]
            .into_iter()
            .map(|s| (s.id, s))
            .collect();
        let runs: Vec<_> = session_runs(&sessions).collect();
        assert_eq!(runs, vec![(running.id, Provider::Claude)]);
    }

    #[test]
    fn the_credits_tip_says_until_when_and_what_was_spent() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut billing = ClaudeBilling {
            fallback_configured: true,
            on_credits_until: Some(now + 2 * 3_600 + 120),
            credits_exhausted: false,
            plan_refused: true,
            credit_spend_usd: 1.234,
            credit_balance: None,
        };
        let tip = credits_tip(&billing);
        assert!(tip.starts_with("Plan limit reached"), "{tip}");
        assert!(
            tip.contains("bill API credits until it resets in 2h"),
            "{tip}"
        );
        assert!(tip.ends_with("$1.23 spent by usine"), "{tip}");
        billing.credits_exhausted = true;
        let tip = credits_tip(&billing);
        assert!(tip.contains("runs fail until the plan resets"), "{tip}");
        // Padding only (the early switch, no refusal): the plan has room.
        billing.plan_refused = false;
        let tip = credits_tip(&billing);
        assert!(tip.starts_with("Plan nearly used"), "{tip}");
        assert!(tip.contains("continue on the plan"), "{tip}");
        billing.credits_exhausted = false;
        let tip = credits_tip(&billing);
        assert!(tip.starts_with("Plan nearly used"), "{tip}");
        assert!(tip.contains("finish on the plan"), "{tip}");
    }

    #[test]
    fn the_credit_counter_shows_whats_left_and_goes_red_at_zero() {
        let balance = CreditBalance {
            set_usd: 20.0,
            set_at: 1_790_000_000,
            spent_usd: 7.656,
        };
        assert_eq!(counter_label(&balance), "API $12.34 left");
        let tip = counter_tip(&balance);
        assert!(tip.starts_with("$12.34 left of $20.00 set on "), "{tip}");
        assert!(tip.contains("isn't seen"), "{tip}");

        let mut billing = ClaudeBilling {
            fallback_configured: true,
            credit_balance: Some(balance.clone()),
            ..Default::default()
        };
        assert!(shows_counter(&billing));
        assert!(!counter_empty(&billing));
        // Spent down to zero: red, never negative.
        billing.credit_balance.as_mut().unwrap().spent_usd = 25.0;
        assert!(counter_empty(&billing));
        assert_eq!(
            counter_label(billing.credit_balance.as_ref().unwrap()),
            "API $0.00 left"
        );
        // The API said the credits ran out: red whatever the estimate.
        billing.credit_balance = Some(balance);
        billing.credits_exhausted = true;
        assert!(counter_empty(&billing));
        // Hidden without a key or without a balance.
        billing.fallback_configured = false;
        assert!(!shows_counter(&billing));
        billing.fallback_configured = true;
        billing.credit_balance = None;
        assert!(!shows_counter(&billing));
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

    #[test]
    fn agent_runs_count_but_the_validate_script_does_not() {
        use usine_core::RunSub;
        assert!(runs_agent(&CardState::Implementing(RunSub::Running)));
        assert!(runs_agent(&CardState::AwaitingReview(
            ReviewSub::FixingValidation {
                attempt: 1,
                output: String::new(),
            }
        )));
        assert!(!runs_agent(&CardState::AwaitingReview(
            ReviewSub::Validating { attempt: 1 }
        )));
        assert!(runs_agent(&CardState::Answering {
            previous: Box::new(CardState::ReadyToMerge),
            question: "why?".into(),
        }));
        assert!(!runs_agent(&CardState::StartingBlock));
    }

    #[test]
    fn running_count_tallies_one_provider_and_skips_queued_launches() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let runs = [
            (a, Provider::Claude),
            (b, Provider::Claude),
            (c, Provider::Codex),
        ];
        assert_eq!(running_count(Provider::Claude, &runs, &[]), 2);
        assert_eq!(running_count(Provider::Codex, &runs, &[]), 1);
        let queued = [QueuedTarget::Card(b), QueuedTarget::Review(c)];
        assert_eq!(running_count(Provider::Claude, &runs, &queued), 1);
        assert_eq!(running_count(Provider::Codex, &runs, &queued), 0);
        assert_eq!(running_count(Provider::Claude, &[], &[]), 0);
    }
}
