//! Background poll for the usage bar: refresh both providers' account-level
//! rate-limit usage and emit `UsageUpdated` snapshots to the UI.

use super::*;
use crate::agent::usage::{self, UsageSnapshot};

impl Executor {
    /// Every [`USAGE_POLL_INTERVAL`]: refresh both providers' usage. The first
    /// tick fires immediately so the bar populates shortly after launch; the
    /// bar's refresh button triggers the same refresh on demand in between.
    /// Only spawned for the real provider factory (see
    /// [`ProviderFactory::polls_usage`]).
    pub(super) async fn usage_poll_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(USAGE_POLL_INTERVAL);
        loop {
            interval.tick().await;
            self.refresh_usage().await;
        }
    }

    /// Ask the Claude CLI for the account's usage (a free local command — no
    /// turn runs, no tokens spent), read the Codex rollout files for the last
    /// recorded rate-limit snapshot, and emit the result unconditionally: the
    /// snapshot carries `refreshed_at`, which the bar's tooltip shows even
    /// when the numbers themselves didn't move.
    pub(super) async fn refresh_usage(&self) {
        // The simulator promises no network: its factory never spawns the poll
        // loop, and this guard keeps a stray manual `RefreshUsage` command
        // from shelling out either.
        if !self.providers.polls_usage() {
            return;
        }
        let claude = usage::fetch_claude_usage().await;
        // File-system walk off the async workers.
        let codex = tokio::task::spawn_blocking(|| {
            usage::codex_sessions_root()
                .and_then(|root| usage::read_codex_usage(&root, usage::now_secs()))
        })
        .await
        .unwrap_or(None);
        let snapshot = UsageSnapshot {
            claude,
            codex,
            refreshed_at: Some(now_millis()),
        };
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::usage_updated(snapshot));
    }
}
