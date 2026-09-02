//! Background poll for the usage bar: refresh both providers' account-level
//! rate-limit usage and emit `UsageUpdated` snapshots to the UI.

use super::*;
use crate::agent::provider::{sim, UsageSource};
use crate::agent::usage::{self, UsageSnapshot};

impl Executor {
    /// Every [`USAGE_POLL_INTERVAL`]: refresh both providers' usage. The first
    /// tick fires immediately so the bar populates shortly after launch; the
    /// bar's refresh button triggers the same refresh on demand in between.
    /// Only spawned for the real CLIs or the simulator's mock numbers, never
    /// for test factories (see [`ProviderFactory::usage_source`]).
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
    /// when the numbers themselves didn't move — and which is what clears the
    /// button's in-flight spinner.
    ///
    /// Under the simulator this makes up numbers instead; test factories get
    /// no usage at all (see [`ProviderFactory::usage_source`]).
    pub(super) async fn refresh_usage(&self) {
        let mut snapshot = match self.providers.usage_source() {
            // Tests (and any unknown factory) must not shell out, and their
            // event sequences must stay free of `UsageUpdated`.
            UsageSource::None => return,
            UsageSource::Cli => {
                let claude = usage::fetch_claude_usage().await;
                // File-system walk off the async workers.
                let codex = tokio::task::spawn_blocking(|| {
                    usage::codex_sessions_root()
                        .and_then(|root| usage::read_codex_usage(&root, usage::now_secs()))
                })
                .await
                .unwrap_or(None);
                UsageSnapshot {
                    claude,
                    codex,
                    refreshed_at: None,
                }
            }
            UsageSource::Simulated => {
                // Matches SimProvider's fake latency, so the refresh button's
                // spinner is legible instead of a one-frame flicker.
                tokio::time::sleep(Duration::from_millis(600)).await;
                sim::simulated_usage(usage::now_secs())
            }
        };
        snapshot.refreshed_at = Some(now_millis());
        let _ = self
            .evt_tx
            .unbounded_send(ExecutorEvent::usage_updated(snapshot));
    }
}
