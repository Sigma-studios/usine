//! The provider abstraction: one trait both agent CLIs (and the simulator)
//! implement, so the executor never knows which backend it's driving.
//!
//! A run is started with a [`RunConfig`] and returns a [`RunHandle`]: a stream
//! of normalized [`AgentEvent`]s plus a control channel to answer mid-flight
//! questions or cancel. Model + effort are expressed uniformly via
//! [`crate::domain::model::ModelSpec`]; each provider clamps effort to what it supports.

pub mod claude;
pub mod codex;
pub mod sim;
pub mod stream;

use std::path::PathBuf;
use std::sync::Arc;

use futures::channel::mpsc::UnboundedSender;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent::events::{AgentEvent, RunControl};
use crate::domain::model::{ModelSpec, Provider};
use crate::error::Result;

/// Which phase a run represents. Determines permission/sandbox level and which
/// [`ModelSpec`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunMode {
    Plan,
    Implement,
    ApplyFixes,
    /// Read-only self-review over the committed diff; emits structured verdicts.
    Review,
    /// Read-only triage of PR review comments; emits structured verdicts.
    Triage,
    /// Read-only Q&A about the card's current work: the agent inspects and
    /// answers in prose, never editing files. Changes go through the request-
    /// changes flows instead.
    Question,
}

/// Everything a provider needs to launch one run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub provider: Provider,
    /// Working directory: the repo root for `Plan`, the worktree otherwise.
    pub project_dir: PathBuf,
    pub spec: ModelSpec,
    pub mode: RunMode,
    /// Stable session id reused across phases to support resume.
    pub session_id: Uuid,
    /// The task prompt (sent as the first message, not as an argv flag).
    pub prompt: String,
    /// Extra context appended to the prompt (e.g. the approved plan, or the
    /// selected review fixes).
    pub extra_prompt: Option<String>,
    /// When set, resume this provider session (`claude --resume <id>` /
    /// `codex exec resume <id>`) to continue the conversation instead of
    /// starting fresh.
    pub resume_session: Option<String>,
    /// The card's attached files (absolute paths to the managed copies). The
    /// prompt lists them for the agent to read; the Codex provider additionally
    /// passes image attachments via `codex exec -i` (its shell tools can read
    /// text but not see images, unlike Claude's vision-capable Read tool).
    pub attachments: Vec<PathBuf>,
}

impl RunConfig {
    /// The full prompt text handed to the agent (prompt + any extra context).
    pub fn full_prompt(&self) -> String {
        match &self.extra_prompt {
            Some(extra) if !extra.is_empty() => format!("{}\n\n{}", self.prompt, extra),
            _ => self.prompt.clone(),
        }
    }
}

/// A live run: a normalized event stream plus a control channel.
pub struct RunHandle {
    pub events: BoxStream<'static, AgentEvent>,
    pub control: UnboundedSender<RunControl>,
}

/// A backend that can run an agent.
#[async_trait::async_trait]
pub trait AgentProvider: Send + Sync {
    fn provider(&self) -> Provider;
    async fn start(&self, cfg: RunConfig) -> Result<RunHandle>;

    /// Whether a live run can absorb a [`RunControl::Answer`](crate::agent::events::RunControl::Answer)
    /// over its control channel and keep going (`true`), or is one-shot and must
    /// be *resumed* with a fresh run to incorporate an answer (`false`). The real
    /// CLIs are one-shot; only the simulator is interactive. The executor uses
    /// this to decide whether to forward an answer or resume the session.
    fn interactive(&self) -> bool {
        false
    }
}

/// Builds a provider implementation for a given [`Provider`]. The executor is
/// constructed with a factory so Phase A can inject the simulator and Phase B
/// can inject the real CLIs without touching the executor.
pub trait ProviderFactory: Send + Sync {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider>;

    /// Whether the executor should poll the real CLIs' account rate-limit
    /// usage for the usage bar. Off by default so the simulator (which
    /// promises no network) and test factories never shell out to `claude`.
    fn polls_usage(&self) -> bool {
        false
    }
}

/// Factory producing the real CLI-backed providers (`claude` / `codex`).
pub struct RealFactory;

impl ProviderFactory for RealFactory {
    fn polls_usage(&self) -> bool {
        true
    }

    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        match provider {
            Provider::Claude => Arc::new(claude::ClaudeProvider),
            Provider::Codex => Arc::new(codex::CodexProvider),
        }
    }
}
