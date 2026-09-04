//! A simulated provider used in Phase A so the whole board — interventions,
//! plan approval, implementing, fixes — is fully navigable without spawning a
//! real agent. It exercises the exact same executor code path the real
//! providers will use.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt;

use crate::agent::events::{AgentEvent, RunControl};
use crate::agent::usage::{ProviderUsage, RateLimitWindow, UsageSnapshot};
use crate::domain::model::{Provider, Usage};
use crate::error::Result;

use super::{AgentProvider, ProviderFactory, RunConfig, RunHandle, RunMode, UsageSource};

const SAMPLE_PLAN: &str = "\
## Plan

1. Add the new module and wire it into the app.
2. Implement the core logic with unit tests.
3. Update the docs and changelog.

_Estimated: ~120 lines across 4 files._

```usine-questions
[
  {\"question\": \"Which storage backend should this use?\", \"options\": [\"SQLite\", \"In-memory\", \"Postgres\"]},
  {\"question\": \"Should the feature be on by default?\", \"options\": [\"Yes\", \"No, behind a flag\"]}
]
```

```usine-plan
{\"v\": 1,
 \"tldr\": [\"Add the module behind a flag\", \"Adapt at the boundary so no call site moves\"],
 \"steps\": [
   {\"title\": \"Add the new module\", \"detail\": \"Setup, teardown and one public entry point.\", \"files\": [\"crates/core/src/newmod.rs\"]},
   {\"title\": \"Wire it into the app\", \"detail\": \"Behind the flag, at startup.\", \"files\": [\"crates/app/src/main.rs\"]},
   {\"title\": \"Cover it with tests\", \"detail\": \"Setup runs once; old records still load.\", \"files\": [\"crates/core/tests/newmod.rs\"]}
 ],
 \"files\": [\"crates/core/src/newmod.rs\", \"crates/app/src/main.rs\", \"crates/core/tests/newmod.rs\", \"README.md\"],
 \"verification\": [\"cargo test -p usine-core\", \"Launch the app and create a card\"],
 \"risks\": [\"The flag's default changes behaviour for existing installs.\"]}
```";

pub struct SimProvider {
    provider: Provider,
}

impl SimProvider {
    pub fn new(provider: Provider) -> Self {
        SimProvider { provider }
    }
}

#[async_trait::async_trait]
impl AgentProvider for SimProvider {
    fn provider(&self) -> Provider {
        self.provider
    }

    /// The simulator keeps its task alive and answers questions over the control
    /// channel, so it's interactive (unlike the one-shot real CLIs).
    fn interactive(&self) -> bool {
        true
    }

    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        let (evt_tx, evt_rx) = mpsc::unbounded::<AgentEvent>();
        let (ctl_tx, ctl_rx) = mpsc::unbounded::<RunControl>();
        let session = cfg.session_id.to_string();
        tokio::spawn(simulate(cfg.mode, session, evt_tx, ctl_rx));
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

async fn simulate(
    mode: RunMode,
    session: String,
    evt_tx: mpsc::UnboundedSender<AgentEvent>,
    mut ctl_rx: mpsc::UnboundedReceiver<RunControl>,
) {
    macro_rules! emit {
        ($e:expr) => {
            let _ = evt_tx.unbounded_send($e);
        };
    }
    macro_rules! pause {
        ($ms:expr) => {
            tokio::time::sleep(Duration::from_millis($ms)).await;
        };
    }

    emit!(AgentEvent::Started {
        session_id: session
    });

    match mode {
        RunMode::Plan => {
            emit!(AgentEvent::Progress {
                text: "📖 Reading project files…".into()
            });
            pause!(500);
            emit!(AgentEvent::Progress {
                text: "🤔 Considering the approach…".into()
            });
            pause!(500);
            emit!(AgentEvent::NeedsInput {
                request_id: "sim-q1".into(),
                question: "Should I optimize this change for speed or for simplicity?".into(),
                options: vec!["Speed".into(), "Simplicity".into()],
            });
            // Block until the user answers (or cancels).
            match ctl_rx.next().await {
                Some(RunControl::Answer { text }) => {
                    emit!(AgentEvent::Progress {
                        text: format!("✅ Got it — going with: {text}")
                    });
                }
                Some(RunControl::Cancel) | None => {
                    emit!(AgentEvent::Error {
                        message: "run cancelled".into()
                    });
                    return;
                }
                Some(RunControl::Interrupt) => {}
            }
            pause!(700);
            emit!(AgentEvent::PlanReady {
                plan: SAMPLE_PLAN.into()
            });
        }
        RunMode::Implement => {
            emit!(AgentEvent::Progress {
                text: "🌿 Working in an isolated worktree…".into()
            });
            pause!(500);
            emit!(AgentEvent::Progress {
                text: "✍️  Editing files…".into()
            });
            pause!(600);
            emit!(AgentEvent::Progress {
                text: "🧪 Running tests… all green".into()
            });
            pause!(500);
            // The hand-off block feeds the awaiting-review panel's recap.
            emit!(AgentEvent::Done {
                result: "Implemented the change.\n\n```usine-handoff\n{\
                    \"v\":2,\
                    \"summary\":\"TL;DR: wired the new module into the app behind a flag.\\n- The \
                    adapter keeps every existing call site untouched.\\n- Docs updated; the \
                    changelog is generated, so it was left alone.\",\
                    \"changes\":[\
                    {\"path\":\"crates/core/src/newmod.rs\",\"what\":\"New module: setup, teardown \
                    and the one public entry point.\",\"kind\":\"feat\"},\
                    {\"path\":\"crates/app/src/main.rs\",\"what\":\"Wires the module in at startup \
                    behind the flag.\",\"kind\":\"feat\"},\
                    {\"path\":\"README.md\",\"what\":\"Documents the flag.\",\"kind\":\"docs\"}],\
                    \"tests\":[\
                    {\"scenario\":\"Create a card and start it\",\"expect\":\"the new module logs \
                    its setup exactly once\",\"verified\":true},\
                    {\"scenario\":\"Restart with an existing database\",\"expect\":\"old records \
                    still load, unmigrated\",\"verified\":false}],\
                    \"risks\":[\"The flag defaults to on, so an existing install changes behaviour \
                    on upgrade.\"],\
                    \"questions\":[\"The flag defaults to on — should it ship behind an opt-in instead?\"]\
                    }\n```".into(),
                cost_usd: 0.42,
                usage: Usage {
                    input_tokens: 12_000,
                    output_tokens: 3_400,
                },
            });
        }
        RunMode::ApplyFixes => {
            emit!(AgentEvent::Progress {
                text: "🩹 Applying the selected review fixes…".into()
            });
            pause!(500);
            emit!(AgentEvent::Progress {
                text: "🧪 Re-running tests…".into()
            });
            pause!(500);
            // The `usine-fixes` block reports per-finding outcomes against the
            // ids the fix prompt carried — id 0 is the first self-review
            // finding above, the one checked by default.
            emit!(AgentEvent::Done {
                result: "TL;DR: extracted the duplicated helper.\n- The shared logic now lives in                          `util.rs`; both call sites use it.\n- Left the naming nit alone, it                          reads fine in context.\n\n```usine-fixes\n{\
                    \"v\":1,\
                    \"outcomes\":[{\"id\":0,\"outcome\":\"partial\",\"note\":\"Extracted                     the helper, but the second call site still passes its own buffer — worth a                     follow-up.\"}]}\n```"
                    .into(),
                cost_usd: 0.12,
                usage: Usage {
                    input_tokens: 4_000,
                    output_tokens: 900,
                },
            });
        }
        RunMode::Review => {
            emit!(AgentEvent::Progress {
                text: "🔎 Reviewing the committed diff…".into()
            });
            pause!(500);
            // The `issue`/`opinion` fields feed self-review parsing; `body` feeds
            // PR-review parsing (both flows use RunMode::Review). Extra fields are
            // ignored by whichever parser doesn't use them.
            emit!(AgentEvent::Done {
                result: "Self-review complete.\n\n```usine-review\n[\
                    {\"path\":\"src/lib.rs\",\"line\":12,\"issue\":\"This helper duplicates logic in util.rs\",\"body\":\"This helper duplicates logic in util.rs — consider extracting it.\",\"severity\":\"medium\",\"worth_fixing\":true,\"opinion\":\"Worth extracting to avoid drift.\"},\
                    {\"path\":\"src/main.rs\",\"line\":5,\"issue\":\"Unclear variable name `x`\",\"body\":\"Unclear variable name `x`.\",\"severity\":\"low\",\"worth_fixing\":false,\"opinion\":\"Optional nit.\"}\
                    ]\n```".into(),
                cost_usd: 0.08,
                usage: Usage {
                    input_tokens: 6_000,
                    output_tokens: 1_200,
                },
            });
        }
        RunMode::Question => {
            emit!(AgentEvent::Progress {
                text: "📖 Reading the work in this worktree…".into()
            });
            pause!(400);
            emit!(AgentEvent::Progress {
                text: "💬 Writing an answer…".into()
            });
            pause!(400);
            // Read-only by contract: a prose answer, no file writes.
            emit!(AgentEvent::Done {
                result: "Good question. The change keeps the existing call sites untouched by \
                         adapting at the boundary, so nothing downstream needs to migrate; the \
                         flag currently defaults to on. No defect found while checking this."
                    .into(),
                cost_usd: 0.03,
                usage: Usage {
                    input_tokens: 2_000,
                    output_tokens: 400,
                },
            });
        }
        RunMode::Investigate => {
            emit!(AgentEvent::Progress {
                text: "🔎 Reading the code paths in question…".into()
            });
            pause!(500);
            emit!(AgentEvent::Progress {
                text: "🧵 Tracing call sites…".into()
            });
            pause!(500);
            emit!(AgentEvent::Done {
                result: "## Findings\n\n\
                    - The request cache in `src/cache.rs:42` is keyed by URL and never evicted; \
                    it grows unboundedly under varied traffic.\n\
                    - `src/handler.rs:118` clones the whole response body into the cache even for \
                    streaming responses.\n\n\
                    ## Verdict\n\n\
                    The cache is NOT bounded. Adding an LRU cap (~1k entries) at the insert site \
                    in `src/cache.rs:42` would fix both issues; the streaming clone should be \
                    skipped outright.\n\n\
                    If you want, a follow-up could size the cap against production traffic.\n\n\
                    ```usine-findings\n{\
                    \"v\":1,\
                    \"verdict\":\"The cache is NOT bounded; an LRU cap at the insert site fixes it.\",\
                    \"findings\":[\
                    {\"claim\":\"The request cache is keyed by URL and never evicted.\",\
                    \"evidence\":[{\"path\":\"src/cache.rs\",\"line\":42}],\"confidence\":\"high\"},\
                    {\"claim\":\"Streaming responses are cloned into the cache in full.\",\
                    \"evidence\":[{\"path\":\"src/handler.rs\",\"line\":118}],\"confidence\":\"medium\"}],\
                    \"open_questions\":[\"The right cap depends on production traffic, which is not \
                    visible from the code.\"]}\n```\n\n\
                    ```usine-questions\n[{\"question\":\"How should the cache be bounded?\",\
                    \"options\":[\"LRU, ~1k entries\",\"Time-based eviction\",\"Both\"]}]\n```"
                    .into(),
                cost_usd: 0.06,
                usage: Usage {
                    input_tokens: 5_000,
                    output_tokens: 900,
                },
            });
        }
        RunMode::Triage => {
            emit!(AgentEvent::Progress {
                text: "🧭 Triaging review comments…".into()
            });
            pause!(500);
            emit!(AgentEvent::Done {
                result: "Triage complete.\n\n```usine-review\n[\
                    {\"id\":1,\"severity\":\"medium\",\"worth_fixing\":true,\"opinion\":\"Valid — extracting improves clarity.\",\"reply\":\"\"},\
                    {\"id\":2,\"severity\":\"low\",\"worth_fixing\":false,\"opinion\":\"Just a nit.\",\"reply\":\"Thanks — leaving this as-is; it reads clearly enough.\"},\
                    {\"id\":3,\"severity\":\"critical\",\"worth_fixing\":true,\"opinion\":\"Real panic risk.\",\"reply\":\"\"}\
                    ]\n```".into(),
                cost_usd: 0.05,
                usage: Usage {
                    input_tokens: 3_000,
                    output_tokens: 700,
                },
            });
        }
    }
}

/// Factory that produces simulators for every provider (Phase A).
pub struct SimFactory;

impl ProviderFactory for SimFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(SimProvider::new(provider))
    }

    fn usage_source(&self) -> UsageSource {
        UsageSource::Simulated
    }
}

/// Mock rate-limit usage for the demo board's bar: no CLI, no network. The
/// numbers wobble with the clock so the bar's refresh button visibly does
/// something, and every window stays well above 0% — a 0% window renders no
/// gauge, which would make the demo bar look broken.
pub fn simulated_usage(now_secs: i64) -> UsageSnapshot {
    // Claude reports a pre-formatted local time, Codex a unix timestamp, so
    // the mock exercises both of the bar's reset-rendering branches.
    UsageSnapshot {
        claude: Some(ProviderUsage {
            session: Some(RateLimitWindow {
                used_percent: wobble(now_secs, 1, 8.0, 37.0),
                resets_text: Some("today at 8pm".into()),
                ..Default::default()
            }),
            weekly: Some(RateLimitWindow {
                used_percent: wobble(now_secs, 2, 45.0, 64.0),
                resets_text: Some("Sun at 9am".into()),
                ..Default::default()
            }),
            // Fable's separate weekly cap, so the demo bar shows the per-model
            // gauge too.
            weekly_model: Some((
                "Fable".into(),
                RateLimitWindow {
                    used_percent: wobble(now_secs, 5, 12.0, 29.0),
                    resets_text: Some("Sun at 9am".into()),
                    ..Default::default()
                },
            )),
        }),
        codex: Some(ProviderUsage {
            session: Some(RateLimitWindow {
                used_percent: wobble(now_secs, 3, 30.0, 54.0),
                resets_at: Some(now_secs + 2 * 3_600 + 15 * 60),
                ..Default::default()
            }),
            weekly: Some(RateLimitWindow {
                used_percent: wobble(now_secs, 4, 72.0, 91.0),
                resets_at: Some(now_secs + 3 * 86_400),
                ..Default::default()
            }),
            weekly_model: None,
        }),
        // Stamped by the executor, which knows when the "poll" happened.
        refreshed_at: None,
    }
}

/// A deterministic percentage inside `[lo, hi]` from the clock and a per-window
/// salt — a cheap scramble, not randomness; it only has to move between ticks.
fn wobble(now_secs: i64, salt: u64, lo: f64, hi: f64) -> f64 {
    let mut h = (now_secs as u64)
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(salt);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 29;
    lo + (h % 1_000) as f64 / 1_000.0 * (hi - lo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_windows_stay_visible_and_in_band() {
        for now in [0_i64, 1, 1_800_000_000, 1_800_000_037, 2_100_000_000] {
            let snap = simulated_usage(now);
            let claude = snap.claude.expect("claude usage");
            let codex = snap.codex.expect("codex usage");
            for (window, lo, hi) in [
                (claude.session.unwrap(), 8.0, 37.0),
                (claude.weekly.unwrap(), 45.0, 64.0),
                (codex.session.clone().unwrap(), 30.0, 54.0),
                (codex.weekly.clone().unwrap(), 72.0, 91.0),
            ] {
                // > 0 matters on its own: a 0% window renders no gauge, which
                // would drop the demo bar into its "No usage data" branch.
                assert!(window.used_percent > 0.0, "{window:?} at {now}");
                assert!(
                    (lo..=hi).contains(&window.used_percent),
                    "{window:?} outside [{lo}, {hi}] at {now}"
                );
            }
            for window in [codex.session.unwrap(), codex.weekly.unwrap()] {
                assert!(window.resets_at.is_some_and(|at| at > now), "{window:?}");
            }
        }
    }
}
