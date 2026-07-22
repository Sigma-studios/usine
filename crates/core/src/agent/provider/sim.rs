//! A simulated provider used in Phase A so the whole board — interventions,
//! plan approval, implementing, fixes — is fully navigable without spawning a
//! real agent. It exercises the exact same executor code path the real
//! providers will use.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::StreamExt;

use crate::agent::events::{AgentEvent, RunControl};
use crate::domain::model::{Provider, Usage};
use crate::error::Result;

use super::{AgentProvider, ProviderFactory, RunConfig, RunHandle, RunMode};

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
                    \"summary\":\"Added the new module and wired it into the app. Kept the existing \
                    call sites untouched by adapting at the boundary, which was simpler than \
                    threading the new type through. The docs are updated; the changelog entry is \
                    not, since the release notes are generated.\",\
                    \"questions\":[\"The flag defaults to on — should it ship behind an opt-in instead?\"],\
                    \"tests\":[\"Create a card and start it — the new module should log its setup once\",\
                    \"Restart with an existing database — old records should still load\"]\
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
            emit!(AgentEvent::Done {
                result: "Applied selected fixes.".into(),
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
                    If you want, a follow-up could size the cap against production traffic."
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
}
