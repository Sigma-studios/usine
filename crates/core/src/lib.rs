//! usine-core: UI-agnostic domain logic for the Usine agent-workflow board.
//!
//! The crate has no UI dependencies. It owns the domain model, the pure card
//! state machine, typed persistence, the provider abstraction, git/forge
//! integration, and the async executor that ties them together.
//!
//! The modules are grouped by role: [`domain`] (pure types + state machine),
//! [`infra`] (storage, paths, git/forge), and [`agent`] (the provider
//! abstraction, executor, agent-I/O protocol, and reconciliation). [`error`] is
//! cross-cutting and lives at the crate root. The re-exports below are the flat
//! public surface the UI and CLI consume, so the internal grouping is free to
//! change without touching them.

pub mod agent;
pub mod diff;
pub mod domain;
pub mod error;
pub mod infra;
// Unix-only: the server's transport is a Unix domain socket.
#[cfg(all(feature = "mcp", unix))]
pub mod mcp;

pub use agent::events::{
    AdoptProbe, AgentEvent, DirtyAction, ExecutorCommand, ExecutorEvent, ExecutorEventKind,
    OpenTarget, QueuedTarget, RunControl, Severity,
};
pub use agent::executor::{
    fix_prompt, spawn as spawn_executor, ExecutorConfig, ExecutorHandle, CONFLICT_INTERVENTION_ID,
};
pub use agent::handoff::{parse_handoff, Handoff, HANDOFF_INSTRUCTION};
pub use agent::investigate::{investigate_instruction, INVESTIGATE_INSTRUCTION};
pub use agent::plan::{
    parse_plan, parse_questions, plan_block_malformed, plan_instruction, PlanQuestion,
    PLAN_QUESTIONS_INSTRUCTION,
};
pub use agent::provider::claude::ClaudeProvider;
pub use agent::provider::codex::CodexProvider;
pub use agent::provider::sim::SimFactory;
pub use agent::provider::{
    AgentProvider, ProviderFactory, RealFactory, RunConfig, RunHandle, RunMode, UsageSource,
};
pub use agent::review::{
    find_review_prompt, normalize_severity, parse_pr_review, parse_self_review, parse_triage,
    severity_marker, severity_rank, PR_REVIEW_INSTRUCTION, REVIEW_TRIAGE_INSTRUCTION,
    SELF_REVIEW_INSTRUCTION, SEVERITY_LEVELS,
};
pub use agent::startup::{
    binary_on_path, reconcile_interrupted_reviews, reconcile_interrupted_runs,
    reconcile_orphaned_worktree_stacks, seed_default_provider, sync_base_branches,
};
pub use agent::usage::{ProviderUsage, RateLimitWindow, UsageSnapshot};
pub use diff::{
    anchor_drafts, compute_branch_diff, compute_card_diff, fold_unanchorable, DiffData, DiffFile,
    DiffHunk, DiffLine, DiffLineKind, DiffState, DraftAnchors, FileStatus, Token,
};
pub use domain::config::{AppSettings, CardConfig, CardKind, PreviewPort, ProjectConfig};
pub use domain::model::{
    now_millis, supported_efforts, Card, CardAnswers, CardState, CheckStatus, Column, Cost,
    DesignSub, DraftComment, Effort, FixVerdict, Intervention, Mergeable, ModelSpec, PrInfo,
    PrReviewSub, PreviewStatus, PreviewUrl, Project, Provider, QaExchange, ReviewColumn,
    ReviewComment, ReviewEvent, ReviewStatus, ReviewSub, ReviewSummary, ReviewTask, ReviewThread,
    RunSub, Usage, CI_REGISTER_GRACE,
};
pub use domain::state_machine::{transition, Transition, MAX_VALIDATION_ATTEMPTS};
pub use error::{CoreError, Result};
pub use infra::forge::{
    normalize_login, FailedCheck, Forge, GhForge, LivePrState, PrPushTarget, PrSummary,
    ReviewScope, SimForge,
};
pub use infra::git::{
    remote_tracking_base, sanitize_branch_name, GitOps, MergeOutcome, RealGit, SimGit,
};
pub use infra::persistence::{rebuild_database, Store};
