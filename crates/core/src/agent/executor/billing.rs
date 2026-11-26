//! The Claude API-credits fallback.
//!
//! Every Claude run starts on the user's plan (the CLI's own login). The plan
//! counts as blocked until a window resets once that window is 75% full —
//! [`SESSION_SWITCH_AT`] / [`WEEKLY_SWITCH_AT`], read from the runs' own
//! `rate_limit_event`s ([`AgentEvent::PlanUsage`]) and from the usage poll —
//! or once a run is cut off by the limit ([`AgentEvent::LimitReached`]).
//! While it is, and while an Anthropic API key is saved (Settings), new
//! Claude runs get that key instead: they bill the plan's monthly API
//! credits. Switching early is the padding: the runs already going on the
//! plan keep its last quarter to finish in, so few of them ever hit the
//! wall.
//!
//! The ones that still do are continued, not failed: a run the plan cuts off
//! resumes on the key, and a run on the key whose credits run out after the
//! plan has reset resumes on the plan. Every consumer (cards, sessions, PR
//! reviews) resumes the conversation in place. No key saved = the fallback is
//! off and a limit fails the run as before.
//!
//! The key lives in the OS keychain (`infra/secrets.rs`) for the real CLIs.
//! The simulator keeps it in memory, so a demo/preview never writes the real
//! app's keychain entry; test factories never have one.

use super::*;
use crate::agent::events::{PlanWindow, PlanWindowUsage};
use crate::agent::provider::stream::is_credit_exhausted_message;
use crate::agent::provider::ClaudeAuth;
use crate::agent::usage::{ClaudeBilling, ProviderUsage};
use crate::domain::model::Provider;
use crate::infra::secrets::{self, SecretString};

/// How long the plan counts as blocked when neither the CLI nor the usage
/// poll said when the limit resets. The next usage poll at 100% extends it;
/// one showing every window under 100% ends it.
const UNKNOWN_RESET_BLOCK_SECS: i64 = 60 * 60;

/// What a run cut off by a usage limit is resumed with, on the other source.
pub(super) const LIMIT_CONTINUE_PROMPT: &str = "You were interrupted by a usage limit; continue \
    from where you stopped.";

/// How full the plan's 5-hour session window may get before new runs move
/// to credits. The last quarter is the padding the runs already on the plan
/// finish in: several long runs going at once can burn through a lot of it.
/// What they leave unused isn't lost: a run on credits whose credits run out
/// while the plan was only padded (never refused) continues on the plan.
pub(super) const SESSION_SWITCH_AT: f64 = 0.75;

/// The same for the 7-day window.
pub(super) const WEEKLY_SWITCH_AT: f64 = 0.75;

/// The fill at which `window` stops taking new runs.
fn switch_at(window: PlanWindow) -> f64 {
    match window {
        PlanWindow::Session => SESSION_SWITCH_AT,
        PlanWindow::Weekly => WEEKLY_SWITCH_AT,
    }
}

/// A guessed block is only ended by a poll that read the usage at least this
/// long after the block began: a poll already in flight when the limit hit
/// may still show the window just under 100%.
const GUESS_END_MIN_AGE_SECS: i64 = 30;

/// Where the saved API key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyStore {
    /// The OS keychain (real CLIs).
    Keychain,
    /// Process memory only (simulator, tests).
    Memory,
}

#[derive(Default)]
struct BillingState {
    key: Option<SecretString>,
    /// Unix seconds when the plan's usage limit resets; set while blocked.
    blocked_until: Option<i64>,
    /// Whether `blocked_until` is a guess (the limit came with no reset time
    /// of its own) rather than a reset the CLI or the poll reported, and
    /// when that guessed block began. A fresh poll with no full window ends
    /// a guessed block early (see [`Billing::observe_usage`]).
    guessed_since: Option<i64>,
    /// Whether the plan actually refused a run (or a poll read a window at
    /// 100%) during the current block, rather than the block being only the
    /// early switch's padding. A padding-only block means the plan still has
    /// room: a run whose credits run out continues there.
    rejected: bool,
    /// Whether the saved key has been read from the keychain yet (or set or
    /// removed since, which makes the keychain read moot).
    key_loaded: bool,
    credits_exhausted: bool,
    credit_spend_usd: f64,
    /// The latest Claude usage the poll read, for a reset time when the CLI
    /// gave none.
    last_usage: Option<ProviderUsage>,
}

/// The fallback's shared state (see the module docs).
pub(super) struct Billing {
    state: Mutex<BillingState>,
    store: KeyStore,
    evt_tx: UnboundedSender<ExecutorEvent>,
}

/// What a run cut off by the plan's limit should do next.
#[derive(Debug, PartialEq)]
pub(super) enum LimitOutcome {
    /// The fallback is active: continue the run on API credits.
    ResumeOnCredits,
    /// No fallback: fail the run with this message.
    Fail(String),
}

impl Billing {
    /// The keychain is NOT read here: on macOS a re-signed binary (every dev
    /// rebuild) gets an access prompt, and the executor must not wait on it.
    /// Call [`Billing::load_key`] once the executor is up.
    pub(super) fn new(store: KeyStore, evt_tx: UnboundedSender<ExecutorEvent>) -> Self {
        Billing {
            state: Mutex::new(BillingState {
                key_loaded: store == KeyStore::Memory,
                ..Default::default()
            }),
            store,
            evt_tx,
        }
    }

    /// Read the saved key from the keychain on a blocking thread, then
    /// announce the fallback's state. Until it returns, runs bill the plan
    /// and a limit fails as if no key were saved. A key saved or removed in
    /// Settings meanwhile wins over what the keychain held.
    pub(super) async fn load_key(&self) {
        if self.store != KeyStore::Keychain {
            return self.announce();
        }
        let key = tokio::task::spawn_blocking(secrets::get_anthropic_key)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("reading the Anthropic API key failed: {e}");
                None
            });
        let mut st = lock(&self.state);
        if !st.key_loaded {
            st.key_loaded = true;
            st.key = key;
        }
        self.emit(&st);
    }

    /// How a run on `provider` should authenticate right now: the API key only
    /// while the plan is blocked, a key is saved and its credits haven't run
    /// out; the plan otherwise (and always for non-Claude providers).
    pub(super) fn auth_for(&self, provider: Provider) -> ClaudeAuth {
        if provider != Provider::Claude {
            return ClaudeAuth::Subscription;
        }
        let mut st = lock(&self.state);
        if self.expire(&mut st) {
            self.emit(&st);
        }
        match (&st.key, st.blocked_until) {
            (Some(key), Some(_)) if !st.credits_exhausted => ClaudeAuth::ApiKey(key.clone()),
            _ => ClaudeAuth::Subscription,
        }
    }

    /// Clear a block whose reset time has passed; whether anything changed.
    fn expire(&self, st: &mut BillingState) -> bool {
        match st.blocked_until {
            Some(until) if until <= crate::agent::usage::now_secs() => {
                st.blocked_until = None;
                st.guessed_since = None;
                st.rejected = false;
                st.credits_exhausted = false;
                st.credit_spend_usd = 0.0;
                true
            }
            _ => false,
        }
    }

    /// The plan's usage limit was hit (`rejected`) or nearly so; it resets at
    /// `resets_at` (falling back to the usage poll's numbers, then to an hour
    /// from now). Whether this began a new block.
    pub(super) fn note_subscription_limit(&self, resets_at: Option<i64>, rejected: bool) -> bool {
        let now = crate::agent::usage::now_secs();
        let mut st = lock(&self.state);
        self.expire(&mut st);
        let reported = resets_at.filter(|t| *t > now);
        let until = reported
            .or_else(|| {
                st.last_usage
                    .as_ref()
                    .and_then(|u| padded_window(u, now))
                    .map(|p| p.resets_at)
            })
            .filter(|t| *t > now)
            .unwrap_or(now + UNKNOWN_RESET_BLOCK_SECS);
        let fresh = st.blocked_until.is_none();
        if fresh {
            // A fresh block: its spend and credit state start over.
            st.credits_exhausted = false;
            st.credit_spend_usd = 0.0;
            st.rejected = false;
        }
        st.rejected |= rejected;
        // A reported reset makes the block certain; a guess only stays a
        // guess while nothing certain has been reported for this block.
        st.guessed_since = match (reported, fresh) {
            (Some(_), _) => None,
            (None, true) => Some(now),
            (None, false) => st.guessed_since,
        };
        st.blocked_until = Some(st.blocked_until.map_or(until, |b| b.max(until)));
        self.emit(&st);
        fresh
    }

    /// A plan window crossed its switch point (`used` is its fill, 0–1):
    /// block the plan until it resets so new runs start on credits, and say
    /// so once unless `quiet`. A full window counts as a refusal.
    fn switch_early(&self, window: PlanWindow, reset: i64, used: f64, quiet: bool) {
        let full = used >= 1.0;
        if !self.note_subscription_limit(Some(reset), full) || quiet {
            return;
        }
        let st = lock(&self.state);
        if st.key.is_none() || st.credits_exhausted {
            return;
        }
        let name = match window {
            PlanWindow::Session => "session",
            PlanWindow::Weekly => "weekly",
        };
        let message = if full {
            format!(
                "Your plan's {name} limit is used up: new Claude runs use API credits until it \
                 resets ({}).",
                countdown(reset)
            )
        } else {
            format!(
                "Your plan's {name} limit is {:.0}% used: new Claude runs use API credits until \
                 it resets ({}); runs already going finish on the plan.",
                (used * 100.0).floor(),
                countdown(reset)
            )
        };
        let _ =
            self.evt_tx
                .unbounded_send(ExecutorEvent::toast(Uuid::nil(), Severity::Info, message));
    }

    /// A plan-billed run's window readings ([`AgentEvent::PlanUsage`]): a
    /// window past its switch point blocks the plan until that window resets.
    pub(super) fn observe_plan_windows(&self, windows: &[PlanWindowUsage]) {
        let now = crate::agent::usage::now_secs();
        let Some((window, reset, used)) = windows
            .iter()
            .filter(|w| w.utilization >= switch_at(w.window))
            .filter_map(|w| Some((w.window, w.resets_at.filter(|t| *t > now)?, w.utilization)))
            .max_by_key(|(_, reset, _)| *reset)
        else {
            return;
        };
        // Already blocked at least that long: every plan-billed run re-reports
        // its windows as they move, so stay quiet.
        if lock(&self.state).blocked_until.is_some_and(|b| b >= reset) {
            return;
        }
        // A full window here is the refusal right before a cut-off, whose own
        // toast says the run continues on credits: block, but stay quiet.
        self.switch_early(window, reset, used, used >= 1.0);
    }

    /// The usage poll's Claude numbers: a window past its switch point blocks
    /// the plan up front, so new runs go straight to credits instead of
    /// failing once first. A guessed block (the limit gave no reset time) ends
    /// as soon as a poll read after it began shows every window back under
    /// its switch point: the
    /// guess was too late and the plan has room again. Also re-announces the
    /// fallback's state on every poll.
    pub(super) fn observe_usage(&self, usage: Option<&ProviderUsage>) {
        let now = crate::agent::usage::now_secs();
        let full = usage.and_then(|u| padded_window(u, now));
        lock(&self.state).last_usage = usage.cloned();
        match full {
            Some(p) => self.switch_early(p.window, p.resets_at, p.used, false),
            None => {
                let mut st = lock(&self.state);
                self.expire(&mut st);
                let stale_guess = st
                    .guessed_since
                    .is_some_and(|since| now - since >= GUESS_END_MIN_AGE_SECS);
                let has_windows = usage.is_some_and(|u| u.session.is_some() || u.weekly.is_some());
                if has_windows && stale_guess {
                    st.blocked_until = None;
                    st.guessed_since = None;
                    st.rejected = false;
                    st.credits_exhausted = false;
                    st.credit_spend_usd = 0.0;
                }
                self.emit(&st);
            }
        }
    }

    /// A Claude run cut off by the plan's limit: block the plan, then say
    /// whether to resume it on credits.
    pub(super) fn on_limit_reached(&self, resets_at: Option<i64>, message: &str) -> LimitOutcome {
        let _ = self.note_subscription_limit(resets_at, true);
        if self.auth_for(Provider::Claude).is_api_key() {
            return LimitOutcome::ResumeOnCredits;
        }
        let st = lock(&self.state);
        let hint = if st.key.is_none() {
            "Save an Anthropic API key in Settings to continue on your plan's API credits."
        } else {
            "Your API credits are exhausted too."
        };
        LimitOutcome::Fail(format!("{} {hint}", message.trim()))
    }

    /// Wrap a run's event stream for billing. A plan-billed run's window
    /// readings feed the early switch and are dropped. A run on the API key
    /// adds its cost to the block's spend and notes credits running out —
    /// passed on as `LimitReached` (resume on the plan) when the plan has
    /// reset meanwhile. A plan-billed run's `LimitReached` blocks the plan and
    /// passes through when the fallback is active. Either passes only when the
    /// consumer can resume (`can_resume`), its message saying where the run
    /// continues; otherwise it becomes an `Error`, so consumers never see a
    /// `LimitReached` they must not act on.
    pub(super) fn meter(
        self: &Arc<Self>,
        events: BoxStream<'static, AgentEvent>,
        auth: &ClaudeAuth,
        can_resume: bool,
    ) -> BoxStream<'static, AgentEvent> {
        let billing = Arc::clone(self);
        let on_credits = auth.is_api_key();
        events
            .filter_map(move |evt| {
                let out = match evt {
                    AgentEvent::PlanUsage { windows } => {
                        billing.observe_plan_windows(&windows);
                        None
                    }
                    evt => Some(billing.meter_event(evt, on_credits, can_resume)),
                };
                futures::future::ready(out)
            })
            .boxed()
    }

    fn meter_event(&self, evt: AgentEvent, on_credits: bool, can_resume: bool) -> AgentEvent {
        match evt {
            AgentEvent::Done { cost_usd, .. } if on_credits => {
                let mut st = lock(&self.state);
                st.credit_spend_usd += cost_usd.max(0.0);
                self.emit(&st);
                evt
            }
            AgentEvent::Error { message }
                if on_credits && is_credit_exhausted_message(&message) =>
            {
                let mut st = lock(&self.state);
                // The plan reset while this run billed credits, or the block
                // was only the early switch's padding: the plan has room, so
                // the run continues there. A padding-only block stays (new
                // runs keep off credits via `credits_exhausted`).
                let reset = self.expire(&mut st) || st.blocked_until.is_none();
                if reset || !st.rejected {
                    if !reset {
                        st.credits_exhausted = true;
                    }
                    self.emit(&st);
                    let why = if reset {
                        "whose limit has reset"
                    } else {
                        "which still has room"
                    };
                    return if can_resume {
                        AgentEvent::LimitReached {
                            resets_at: None,
                            message: format!(
                                "API credits exhausted — continuing on your plan, {why}."
                            ),
                        }
                    } else {
                        AgentEvent::Error {
                            message: format!(
                                "API credits exhausted. Your plan, {why}, can take over: retry \
                                 to continue on it."
                            ),
                        }
                    };
                }
                st.credits_exhausted = true;
                let until = st.blocked_until.map(countdown).unwrap_or_default();
                self.emit(&st);
                AgentEvent::Error {
                    message: format!(
                        "API credits exhausted. Top up at platform.claude.com/settings/billing; \
                         runs go back to your plan when its limit resets{}.",
                        if until.is_empty() {
                            String::new()
                        } else {
                            format!(" ({until})")
                        }
                    ),
                }
            }
            // A limit while already on credits isn't the plan's: never loop.
            AgentEvent::LimitReached { message, .. } if on_credits => AgentEvent::Error { message },
            AgentEvent::LimitReached { resets_at, message } => {
                match self.on_limit_reached(resets_at, &message) {
                    LimitOutcome::ResumeOnCredits if can_resume => {
                        let until = self
                            .credits_until_text()
                            .map(|t| format!(" until the limit resets ({t})"))
                            .unwrap_or_default();
                        AgentEvent::LimitReached {
                            resets_at,
                            message: format!(
                                "{} — continuing on API credits{until}.",
                                message.trim()
                            ),
                        }
                    }
                    LimitOutcome::ResumeOnCredits => AgentEvent::Error {
                        message: format!("{} Retry to continue on API credits.", message.trim()),
                    },
                    LimitOutcome::Fail(message) => AgentEvent::Error { message },
                }
            }
            other => other,
        }
    }

    /// When the current block ends, as a countdown ("in 2h 05m"), if blocked.
    pub(super) fn credits_until_text(&self) -> Option<String> {
        lock(&self.state).blocked_until.map(countdown)
    }

    /// Save (`Some`) or remove (`None`) the API key. Keychain I/O runs on a
    /// blocking thread: the OS may show an access prompt.
    pub(super) async fn set_key(&self, key: Option<String>) -> Result<()> {
        let key = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
        if self.store == KeyStore::Keychain {
            let k = key.clone();
            tokio::task::spawn_blocking(move || match k {
                Some(k) => secrets::set_anthropic_key(&k),
                None => secrets::delete_anthropic_key(),
            })
            .await
            .map_err(|e| CoreError::other(e.to_string()))?
            .map_err(CoreError::other)?;
        }
        let mut st = lock(&self.state);
        st.key = key.map(SecretString::new);
        st.key_loaded = true;
        // A new key may well have credits the old one lacked.
        st.credits_exhausted = false;
        self.emit(&st);
        Ok(())
    }

    #[cfg(test)]
    fn snapshot(&self) -> ClaudeBilling {
        to_snapshot(&lock(&self.state))
    }

    pub(super) fn announce(&self) {
        self.emit(&lock(&self.state));
    }

    fn emit(&self, st: &BillingState) {
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::billing_updated(to_snapshot(st)));
    }
}

fn to_snapshot(st: &BillingState) -> ClaudeBilling {
    ClaudeBilling {
        fallback_configured: st.key.is_some(),
        on_credits_until: st.blocked_until.filter(|_| st.key.is_some()),
        credits_exhausted: st.credits_exhausted,
        plan_refused: st.rejected,
        credit_spend_usd: st.credit_spend_usd,
    }
}

/// The plan window past its switch point that resets last, with that reset —
/// the time the plan takes new runs again. `None` when no window is past it.
///
/// The per-model weekly window (`weekly_model`, e.g. Fable) is left out: when
/// only it is full, the plan still covers the other models, and blocking
/// would bill every Claude run to credits for up to a week.
fn padded_window(usage: &ProviderUsage, now: i64) -> Option<PaddedWindow> {
    [
        (PlanWindow::Session, usage.session.as_ref()),
        (PlanWindow::Weekly, usage.weekly.as_ref()),
    ]
    .into_iter()
    .filter_map(|(kind, w)| Some((kind, w?)))
    .filter(|(kind, w)| w.used_percent >= switch_at(*kind) * 100.0)
    // A reading whose window has already reset says nothing about now.
    .filter_map(|(kind, w)| {
        Some(PaddedWindow {
            window: kind,
            resets_at: w.resets_at.filter(|t| *t > now)?,
            used: w.used_percent / 100.0,
        })
    })
    .max_by_key(|p| p.resets_at)
}

/// A plan window past its switch point (see [`padded_window`]).
struct PaddedWindow {
    window: PlanWindow,
    resets_at: i64,
    /// Its fill, 0–1.
    used: f64,
}

/// `at` (unix seconds) as a countdown: "in 2d 4h", "in 3h 12m", "in 45m".
fn countdown(at: i64) -> String {
    let mut left = (at - crate::agent::usage::now_secs()).max(0);
    let days = left / 86_400;
    left %= 86_400;
    let (hours, minutes) = (left / 3_600, (left % 3_600) / 60);
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {minutes:02}m")
    } else {
        format!("in {minutes}m")
    }
}

impl Executor {
    /// `SetAnthropicKey` / `ClearAnthropicKey`: save or remove the fallback's
    /// key and confirm with a toast.
    pub(super) async fn set_anthropic_key(&self, key: Option<SecretString>) -> Result<()> {
        let saving = key.is_some();
        self.billing
            .set_key(key.map(|k| k.expose().to_string()))
            .await?;
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::toast(
            Uuid::nil(),
            Severity::Success,
            if saving {
                "API key saved. Claude runs use it only once your plan is nearly out, until it resets."
            } else {
                "API key removed. Claude runs stay on your plan."
            },
        ));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::usage::{now_secs, RateLimitWindow};
    use crate::domain::model::Usage;
    use futures::executor::block_on;

    fn billing() -> (Arc<Billing>, UnboundedReceiver<ExecutorEvent>) {
        let (tx, rx) = mpsc::unbounded();
        (Arc::new(Billing::new(KeyStore::Memory, tx)), rx)
    }

    fn with_key() -> (Arc<Billing>, UnboundedReceiver<ExecutorEvent>) {
        let (b, rx) = billing();
        block_on(b.set_key(Some("sk-test".into()))).unwrap();
        (b, rx)
    }

    fn done(cost: f64) -> AgentEvent {
        AgentEvent::Done {
            result: String::new(),
            cost_usd: cost,
            usage: Usage::default(),
        }
    }

    fn limit(resets_at: Option<i64>) -> AgentEvent {
        AgentEvent::LimitReached {
            resets_at,
            message: "You've hit your session limit".into(),
        }
    }

    #[test]
    fn auth_is_the_plan_unless_blocked_with_a_key_and_credits() {
        let soon = now_secs() + 600;
        // No key: blocked or not, the plan.
        let (b, _rx) = billing();
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
        b.note_subscription_limit(Some(soon), true);
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);

        // A key, not blocked: still the plan.
        let (b, _rx) = with_key();
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
        // Blocked: the key — for Claude only.
        b.note_subscription_limit(Some(soon), true);
        assert!(b.auth_for(Provider::Claude).is_api_key());
        assert_eq!(b.auth_for(Provider::Codex), ClaudeAuth::Subscription);
        // Credits exhausted: back to the plan (which fails until the reset).
        lock(&b.state).credits_exhausted = true;
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
    }

    #[test]
    fn an_expired_block_clears_itself() {
        let (b, _rx) = with_key();
        b.note_subscription_limit(Some(now_secs() + 600), true);
        {
            let mut st = lock(&b.state);
            st.blocked_until = Some(now_secs() - 1);
            st.credits_exhausted = true;
            st.credit_spend_usd = 3.0;
        }
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
        let snap = b.snapshot();
        assert_eq!(snap.on_credits_until, None);
        assert!(!snap.credits_exhausted);
        assert_eq!(snap.credit_spend_usd, 0.0);
    }

    #[test]
    fn a_limit_resumes_on_credits_only_with_a_key() {
        let (b, _rx) = billing();
        let soon = Some(now_secs() + 600);
        match b.meter_event(limit(soon), false, true) {
            AgentEvent::Error { message } => {
                assert!(message.contains("Save an Anthropic API key"), "{message}")
            }
            other => panic!("expected an error, got {other:?}"),
        }
        let (b, _rx) = with_key();
        assert!(matches!(
            b.meter_event(limit(soon), false, true),
            AgentEvent::LimitReached { .. }
        ));
        // A consumer that can't resume gets a retry hint instead.
        match b.meter_event(limit(soon), false, false) {
            AgentEvent::Error { message } => assert!(message.contains("Retry"), "{message}"),
            other => panic!("expected an error, got {other:?}"),
        }
        // A limit on a run already on credits never loops.
        assert!(matches!(
            b.meter_event(limit(soon), true, true),
            AgentEvent::Error { .. }
        ));
    }

    #[test]
    fn credit_runs_are_metered_and_exhaustion_is_noted() {
        let (b, _rx) = with_key();
        b.note_subscription_limit(Some(now_secs() + 600), true);
        b.meter_event(done(1.25), true, true);
        b.meter_event(done(0.5), true, true);
        // Plan-billed runs cost the credits nothing.
        b.meter_event(done(9.0), false, true);
        assert!((b.snapshot().credit_spend_usd - 1.75).abs() < 1e-9);

        let evt = b.meter_event(
            AgentEvent::Error {
                message: "Your credit balance is too low to access the Anthropic API.".into(),
            },
            true,
            true,
        );
        assert!(matches!(evt, AgentEvent::Error { message } if message.contains("exhausted")));
        assert!(b.snapshot().credits_exhausted);
        // Saving a new key clears it.
        block_on(b.set_key(Some("sk-other".into()))).unwrap();
        assert!(!b.snapshot().credits_exhausted);
    }

    #[test]
    fn a_full_window_in_the_usage_poll_blocks_the_plan() {
        let (b, _rx) = with_key();
        let reset = now_secs() + 3_000;
        let usage = ProviderUsage {
            session: Some(RateLimitWindow {
                used_percent: 100.0,
                resets_at: Some(reset),
                resets_text: None,
            }),
            weekly: Some(RateLimitWindow {
                used_percent: 40.0,
                resets_at: Some(reset + 99_999),
                resets_text: None,
            }),
            weekly_model: None,
        };
        b.observe_usage(Some(&usage));
        assert_eq!(b.snapshot().on_credits_until, Some(reset));
        // A limit with no reset of its own falls back to the poll's.
        let (b, _rx) = with_key();
        lock(&b.state).last_usage = Some(usage);
        b.note_subscription_limit(None, true);
        assert_eq!(b.snapshot().on_credits_until, Some(reset));
    }

    fn window(used_percent: f64, resets_at: i64) -> Option<RateLimitWindow> {
        Some(RateLimitWindow {
            used_percent,
            resets_at: Some(resets_at),
            resets_text: None,
        })
    }

    #[test]
    fn a_full_per_model_window_alone_never_blocks_the_plan() {
        let (b, _rx) = with_key();
        let later = now_secs() + 5 * 86_400;
        let usage = ProviderUsage {
            session: window(30.0, now_secs() + 3_000),
            weekly: window(60.0, later),
            weekly_model: Some((
                "Fable".into(),
                RateLimitWindow {
                    used_percent: 100.0,
                    resets_at: Some(later),
                    resets_text: None,
                },
            )),
        };
        b.observe_usage(Some(&usage));
        assert_eq!(b.snapshot().on_credits_until, None);
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
        // Nor does it stretch a limit with no reset of its own to the week.
        b.note_subscription_limit(None, true);
        let until = b.snapshot().on_credits_until.unwrap();
        assert!(until < later, "blocked until the per-model reset");
    }

    #[test]
    fn a_fresh_poll_with_room_ends_a_guessed_block() {
        let (b, _rx) = with_key();
        let room = ProviderUsage {
            session: window(5.0, now_secs() + 18_000),
            weekly: window(60.0, now_secs() + 99_999),
            weekly_model: None,
        };
        b.note_subscription_limit(None, true);
        // A poll already in flight when the limit hit doesn't end it.
        b.observe_usage(Some(&room));
        assert!(b.snapshot().on_credits_until.is_some());
        // One read well after the block began does.
        lock(&b.state).guessed_since = Some(now_secs() - GUESS_END_MIN_AGE_SECS);
        b.observe_usage(Some(&room));
        assert_eq!(b.snapshot().on_credits_until, None);
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);

        // A reset the CLI reported is never second-guessed by the poll.
        b.note_subscription_limit(Some(now_secs() + 600), true);
        lock(&b.state).guessed_since = None;
        b.observe_usage(Some(&room));
        assert!(b.snapshot().on_credits_until.is_some());
        // Nor is a guess once a later limit reported its reset.
        let (b, _rx) = with_key();
        b.note_subscription_limit(None, true);
        b.note_subscription_limit(Some(now_secs() + 600), true);
        assert_eq!(lock(&b.state).guessed_since, None);
    }

    fn reading(window: PlanWindow, utilization: f64, resets_at: i64) -> AgentEvent {
        AgentEvent::PlanUsage {
            windows: vec![PlanWindowUsage {
                window,
                utilization,
                resets_at: Some(resets_at),
            }],
        }
    }

    /// Run `events` through the meter as one plan-billed run's stream.
    fn meter_all(b: &Arc<Billing>, events: Vec<AgentEvent>) -> Vec<AgentEvent> {
        let stream = futures::stream::iter(events).boxed();
        block_on(
            b.meter(stream, &ClaudeAuth::Subscription, true)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn a_window_past_its_switch_point_moves_new_runs_to_credits() {
        let (b, _rx) = with_key();
        let reset = now_secs() + 3_000;
        // Under the switch points: nothing changes, and the readings never
        // reach the run's consumer.
        let out = meter_all(
            &b,
            vec![
                reading(PlanWindow::Session, SESSION_SWITCH_AT - 0.01, reset),
                reading(PlanWindow::Weekly, WEEKLY_SWITCH_AT - 0.01, reset + 99_999),
                done(0.0),
            ],
        );
        assert!(
            matches!(out.as_slice(), [AgentEvent::Done { .. }]),
            "{out:?}"
        );
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
        // The session window at its switch point: blocked until it resets.
        meter_all(
            &b,
            vec![reading(PlanWindow::Session, SESSION_SWITCH_AT, reset)],
        );
        assert_eq!(b.snapshot().on_credits_until, Some(reset));
        assert!(b.auth_for(Provider::Claude).is_api_key());
        // The weekly window at its switch point stretches it to the weekly reset.
        meter_all(
            &b,
            vec![reading(
                PlanWindow::Weekly,
                WEEKLY_SWITCH_AT,
                reset + 99_999,
            )],
        );
        assert_eq!(b.snapshot().on_credits_until, Some(reset + 99_999));

        // The usage poll applies the same switch points.
        let (b, _rx) = with_key();
        b.observe_usage(Some(&ProviderUsage {
            session: window(80.0, reset),
            weekly: window(50.0, reset + 99_999),
            weekly_model: None,
        }));
        assert_eq!(b.snapshot().on_credits_until, Some(reset));

        // A poll reading whose window already reset is ignored, not turned
        // into a guessed block.
        let (b, _rx) = with_key();
        b.observe_usage(Some(&ProviderUsage {
            session: window(100.0, now_secs() - 5),
            weekly: window(50.0, reset + 99_999),
            weekly_model: None,
        }));
        assert_eq!(b.snapshot().on_credits_until, None);
    }

    #[test]
    fn the_early_switch_is_announced_once() {
        let (b, mut rx) = with_key();
        while rx.try_recv().is_ok() {}
        let reset = now_secs() + 3_000;
        let toasts = |rx: &mut UnboundedReceiver<ExecutorEvent>| {
            let mut n = 0;
            while let Ok(e) = rx.try_recv() {
                if matches!(
                    e.kind,
                    crate::agent::events::ExecutorEventKind::Toast { .. }
                ) {
                    n += 1;
                }
            }
            n
        };
        b.observe_plan_windows(&[PlanWindowUsage {
            window: PlanWindow::Session,
            utilization: 0.96,
            resets_at: Some(reset),
        }]);
        assert_eq!(toasts(&mut rx), 1);
        b.observe_plan_windows(&[PlanWindowUsage {
            window: PlanWindow::Session,
            utilization: 0.97,
            resets_at: Some(reset),
        }]);
        assert_eq!(toasts(&mut rx), 0);
    }

    #[test]
    fn the_early_switch_toast_shows_the_actual_fill_and_skips_refusals() {
        let (b, mut rx) = with_key();
        while rx.try_recv().is_ok() {}
        let toast_texts = |rx: &mut UnboundedReceiver<ExecutorEvent>| {
            let mut out = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let crate::agent::events::ExecutorEventKind::Toast { message, .. } = e.kind {
                    out.push(message);
                }
            }
            out
        };
        b.observe_plan_windows(&[PlanWindowUsage {
            window: PlanWindow::Session,
            utilization: 0.83,
            resets_at: Some(now_secs() + 3_000),
        }]);
        let texts = toast_texts(&mut rx);
        assert!(
            texts.len() == 1 && texts[0].contains("83% used"),
            "{texts:?}"
        );
        // The refused reading right before a cut-off blocks quietly (the
        // cut-off says the run continues on credits), and counts as a refusal.
        let (b, mut rx) = with_key();
        while rx.try_recv().is_ok() {}
        b.observe_plan_windows(&[PlanWindowUsage {
            window: PlanWindow::Session,
            utilization: 1.0,
            resets_at: Some(now_secs() + 3_000),
        }]);
        assert!(toast_texts(&mut rx).is_empty());
        assert!(b.auth_for(Provider::Claude).is_api_key());
        assert!(lock(&b.state).rejected);
    }

    #[test]
    fn credits_running_out_during_padding_only_continue_on_the_plan() {
        let exhausted = || AgentEvent::Error {
            message: "Your credit balance is too low to access the Anthropic API.".into(),
        };
        let reset = now_secs() + 3_000;
        let (b, _rx) = with_key();
        b.observe_plan_windows(&[PlanWindowUsage {
            window: PlanWindow::Session,
            utilization: 0.8,
            resets_at: Some(reset),
        }]);
        assert!(b.auth_for(Provider::Claude).is_api_key());
        // The badge can tell padding from a refusal.
        assert!(!b.snapshot().plan_refused);
        match b.meter_event(exhausted(), true, true) {
            AgentEvent::LimitReached { message, .. } => {
                assert!(message.contains("still has room"), "{message}")
            }
            other => panic!("expected LimitReached, got {other:?}"),
        }
        // New runs go back to the plan; the block itself stays.
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
        assert_eq!(b.snapshot().on_credits_until, Some(reset));
        // Once the plan really refuses a run, exhausted credits fail it.
        let (b, _rx) = with_key();
        b.observe_plan_windows(&[PlanWindowUsage {
            window: PlanWindow::Session,
            utilization: 0.8,
            resets_at: Some(reset),
        }]);
        let _ = b.meter_event(limit(Some(reset)), false, true);
        assert!(b.snapshot().plan_refused);
        assert!(matches!(
            b.meter_event(exhausted(), true, true),
            AgentEvent::Error { .. }
        ));
    }

    #[test]
    fn credits_running_out_after_the_reset_resume_on_the_plan() {
        let exhausted = || AgentEvent::Error {
            message: "Your credit balance is too low to access the Anthropic API.".into(),
        };
        let (b, _rx) = with_key();
        b.note_subscription_limit(Some(now_secs() + 600), true);
        // Still blocked: nowhere to go, the run fails.
        assert!(matches!(
            b.meter_event(exhausted(), true, true),
            AgentEvent::Error { .. }
        ));
        // The plan reset meanwhile: the run continues there.
        let (b, _rx) = with_key();
        b.note_subscription_limit(Some(now_secs() + 600), true);
        lock(&b.state).blocked_until = Some(now_secs() - 1);
        match b.meter_event(exhausted(), true, true) {
            AgentEvent::LimitReached { message, .. } => {
                assert!(message.contains("continuing on your plan"), "{message}")
            }
            other => panic!("expected LimitReached, got {other:?}"),
        }
        assert_eq!(b.auth_for(Provider::Claude), ClaudeAuth::Subscription);
        // A consumer that can't resume gets a retry hint.
        assert!(matches!(
            b.meter_event(exhausted(), true, false),
            AgentEvent::Error { message } if message.contains("retry")
        ));
    }

    #[test]
    fn an_unknown_reset_blocks_for_an_hour() {
        let (b, _rx) = with_key();
        b.note_subscription_limit(None, true);
        let until = b.snapshot().on_credits_until.unwrap();
        assert!((until - now_secs() - UNKNOWN_RESET_BLOCK_SECS).abs() <= 2);
    }
}
