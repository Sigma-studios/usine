//! Domain model: providers, effort, cards, projects, and the card state.
//!
//! [`CardState`] is the single source of truth for where a card sits in its
//! lifecycle. The board column is *derived* from the state, never stored
//! independently, so the two can never disagree.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::config::CardConfig;

/// Wall-clock milliseconds since the Unix epoch. Used for created/updated stamps.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A USD monetary amount, stored as a whole number of **cents**.
///
/// A card's cost is accumulated across many runs and shown to the user rounded
/// to the cent, so cents is the natural unit. Keeping it an integer rather than
/// an `f64` number of dollars matters for two reasons:
///
/// * **Exact accumulation** — repeated `f64` addition drifts, producing values
///   like `10.125862999999999`.
/// * **Deterministic serialization** — `native_db`'s update/remove re-encode the
///   record and compare it byte-for-byte against the stored bytes; a drifted
///   float doesn't round-trip through JSON identically, which previously left
///   such cards impossible to delete.
///
/// On disk it stays a JSON number of **dollars**, rounded to the cent (e.g.
/// `10.13`) — the same shape older records and the previous `f64` field used, so
/// the format is unchanged and no migration is needed. Determinism holds because
/// we only ever emit `cents / 100`, and every value serde_json emits is a fixed
/// point of its own parse→serialize. The [`Deserialize`] impl also accepts a
/// bare integer (read as cents) for robustness.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cost {
    cents: u64,
}

impl Cost {
    pub const ZERO: Cost = Cost { cents: 0 };

    /// Round a USD *dollar* amount (e.g. a provider's `total_cost_usd`) to the
    /// nearest cent. Non-finite or negative inputs clamp to zero.
    pub fn from_usd(dollars: f64) -> Cost {
        let cents = (dollars * 100.0).round();
        Cost {
            cents: if cents.is_finite() && cents > 0.0 {
                cents as u64
            } else {
                0
            },
        }
    }

    pub const fn from_cents(cents: u64) -> Cost {
        Cost { cents }
    }

    pub const fn cents(self) -> u64 {
        self.cents
    }

    /// The amount as a number of dollars, for display and formatting.
    pub fn as_usd(self) -> f64 {
        self.cents as f64 / 100.0
    }

    pub const fn is_zero(self) -> bool {
        self.cents == 0
    }
}

impl std::ops::Add for Cost {
    type Output = Cost;
    fn add(self, rhs: Cost) -> Cost {
        Cost {
            cents: self.cents + rhs.cents,
        }
    }
}

impl std::ops::AddAssign for Cost {
    fn add_assign(&mut self, rhs: Cost) {
        self.cents += rhs.cents;
    }
}

/// Renders as `$0.00`.
impl std::fmt::Display for Cost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "${:.2}", self.as_usd())
    }
}

impl Serialize for Cost {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        // A cent-valued dollars float: canonical (a fixed point of JSON
        // parse→serialize) and byte-identical to the format older records used.
        s.serialize_f64(self.as_usd())
    }
}

impl<'de> Deserialize<'de> for Cost {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Cost, D::Error> {
        struct CostVisitor;
        impl<'de> serde::de::Visitor<'de> for CostVisitor {
            type Value = Cost;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("cost as a USD-dollars number (or integer cents)")
            }
            // On-disk form: dollars, rounded to the cent on the way in.
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Cost, E> {
                Ok(Cost::from_usd(v))
            }
            // Defensive: a bare integer is read as cents.
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Cost, E> {
                Ok(Cost::from_cents(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Cost, E> {
                Ok(Cost::from_cents(v.max(0) as u64))
            }
        }
        d.deserialize_any(CostVisitor)
    }
}

// ---------------------------------------------------------------------------
// Providers & effort
// ---------------------------------------------------------------------------

/// Which agent CLI backs a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }
    /// Name of the binary we shell out to.
    pub fn binary(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        }
    }
    pub fn all() -> [Provider; 2] {
        [Provider::Claude, Provider::Codex]
    }
}

/// Unified reasoning-effort knob. Each model honors a subset (see
/// [`supported_efforts`]); specs are clamped to it before a run.
///
/// Declared least-capable first so the derived [`Ord`] matches capability order —
/// [`Effort::clamp_to`] relies on that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

impl Effort {
    pub fn label(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
            Effort::Ultra => "ultra",
        }
    }
    /// Value for `claude --effort <e>` (Claude supports the full set).
    pub fn claude_flag(self) -> &'static str {
        self.label()
    }
    /// Value for `codex -c model_reasoning_effort=<e>`.
    pub fn codex_value(self) -> &'static str {
        self.label()
    }
    pub fn all() -> [Effort; 6] {
        [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
            Effort::Ultra,
        ]
    }

    /// Clamp to the nearest level `supported` actually offers: the highest one at
    /// or below `self`, else the lowest supported. Mirrors how Claude Code itself
    /// clamps an unsupported `--effort`, and keeps us from handing Codex a level
    /// it would reject (e.g. `xhigh` on a non-max model). `supported` is assumed
    /// ascending, as [`supported_efforts`] returns it.
    pub fn clamp_to(self, supported: &[Effort]) -> Effort {
        let mut best = None;
        for &e in supported {
            if e <= self {
                best = Some(e);
            }
        }
        best.or_else(|| supported.first().copied()).unwrap_or(self)
    }
}

/// The reasoning-effort levels a given model actually honors, ordered least- to
/// most-capable. The picker offers only these, and specs are clamped to them (see
/// [`Effort::clamp_to`]), so a run never asks a model for a level it would reject
/// or silently drop.
///
/// * **Claude** — the `opus` (4.8), `sonnet` (5), and `fable` (5) aliases take the
///   full range; Haiku 4.5 has no effort tiers, so it gets a single neutral entry.
/// * **Codex** — GPT-5.6 Sol and Terra reach `ultra`, while Luna reaches `max`.
///   GPT-5.3 through GPT-5.5 reach `xhigh`, as did the retiring
///   `gpt-5.1-codex-max`; legacy or unknown ids stay at `high`.
///
/// Verified against the Claude Code model-config docs and OpenAI's Codex model
/// docs (Jul 2026 — the pre-5.3 Codex lineup shuts down 2026-07-23). Claude Code
/// clamps unsupported levels itself; Codex does not, which is why clamping
/// happens before the CLI call.
pub fn supported_efforts(provider: Provider, model: &str) -> &'static [Effort] {
    use Effort::*;
    match provider {
        Provider::Claude => match model {
            "haiku" => &[Medium],
            // opus / sonnet / fable — and any newer alias — take the full range.
            _ => &[Low, Medium, High, XHigh, Max],
        },
        Provider::Codex => match model {
            "gpt-5.6-sol" | "gpt-5.6-terra" => &[Low, Medium, High, XHigh, Max, Ultra],
            "gpt-5.6-luna" => &[Low, Medium, High, XHigh, Max],
            "gpt-5.3-codex" | "gpt-5.4" | "gpt-5.4-mini" | "gpt-5.5" | "gpt-5.1-codex-max" => {
                &[Low, Medium, High, XHigh]
            }
            _ => &[Low, Medium, High],
        },
    }
}

/// A model id plus the effort to run it at. Plan and implement phases each get
/// their own [`ModelSpec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSpec {
    pub model: String,
    pub effort: Effort,
}

impl ModelSpec {
    pub fn new(model: impl Into<String>, effort: Effort) -> Self {
        ModelSpec {
            model: model.into(),
            effort,
        }
    }
}

// ---------------------------------------------------------------------------
// Board columns
// ---------------------------------------------------------------------------

/// The fixed kanban columns. Derived from [`CardState`] via [`CardState::column`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Column {
    StartingBlock,
    Designing,
    Implementing,
    AwaitingReview,
    SelfReview,
    PrReview,
    ReadyToMerge,
    #[serde(alias = "Merged")]
    Done,
}

impl Column {
    pub fn title(self) -> &'static str {
        match self {
            Column::StartingBlock => "Starting block",
            Column::Designing => "Designing",
            Column::Implementing => "Implementing",
            Column::AwaitingReview => "Awaiting review",
            Column::SelfReview => "Self-review",
            Column::PrReview => "PR review",
            Column::ReadyToMerge => "Ready to merge",
            Column::Done => "Done",
        }
    }
    /// Columns shown on the board, in order.
    pub fn board() -> [Column; 8] {
        [
            Column::StartingBlock,
            Column::Designing,
            Column::Implementing,
            Column::AwaitingReview,
            Column::SelfReview,
            Column::PrReview,
            Column::ReadyToMerge,
            Column::Done,
        ]
    }
}

/// The columns of the PR-review workflow (reviewing *other* contributors' PRs).
/// Derived from [`ReviewStatus`] via [`ReviewStatus::column`]; the analogue of
/// [`Column`] for the review board.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReviewColumn {
    ToReview,
    Reviewing,
    AwaitingValidation,
    Reviewed,
}

impl ReviewColumn {
    pub fn title(self) -> &'static str {
        match self {
            ReviewColumn::ToReview => "To review",
            ReviewColumn::Reviewing => "Reviewing",
            ReviewColumn::AwaitingValidation => "Awaiting validation",
            ReviewColumn::Reviewed => "Reviewed",
        }
    }
    /// Columns shown on the review board, in order.
    pub fn board() -> [ReviewColumn; 4] {
        [
            ReviewColumn::ToReview,
            ReviewColumn::Reviewing,
            ReviewColumn::AwaitingValidation,
            ReviewColumn::Reviewed,
        ]
    }
}

// ---------------------------------------------------------------------------
// Card state machine states
// ---------------------------------------------------------------------------

/// A clarifying question raised by the agent that requires the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intervention {
    /// Provider-specific id used to route the answer back to the right request.
    pub request_id: String,
    pub question: String,
    pub options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesignSub {
    Running,
    Intervention(Intervention),
    AwaitingApproval { plan: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunSub {
    Running,
    Intervention(Intervention),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrReviewSub {
    /// PR created; awaiting the user's "read review comments" action.
    Idle,
    FetchingComments,
    SelectingFixes {
        verdicts: Vec<FixVerdict>,
    },
    ApplyingFixes,
    /// The agent is applying a user-requested free-form change to the already-open
    /// PR — a reprompt from `Idle`, not tied to reviewer comments. Loops back to
    /// `Idle` so the card stays in the PR-review gate (unlike `ApplyingFixes`, the
    /// comment-triage run, which advances to `ReadyToMerge`).
    ApplyingChange,
}

/// Sub-states of the pre-PR review gate. Implementation finishes with the work
/// committed in the card's own isolated worktree; the card then sits here while
/// the user reviews it (opening the worktree in their editor or running the
/// preview app), optionally runs a self-review pass (checkbox + agent fix), then
/// opens the PR. Review, fixes, and testing all stay in the card's worktree — the
/// branch never enters the user's main working copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewSub {
    /// Implemented and committed in the worktree; awaiting the user's review.
    /// Offer a self-review pass, skip straight to the PR, or bounce back to the
    /// agent for changes.
    ///
    /// The serde aliases load cards written under the retired hand-checkout states
    /// (`PreCheckout` → not yet reviewed, `AwaitingCommit` → mid hand-review,
    /// `CheckingOut` → mid checkout) into this single "ready for review" state; a
    /// startup pass re-establishes their worktree (see `reconcile_review_worktrees`).
    #[serde(alias = "PreCheckout", alias = "AwaitingCommit", alias = "CheckingOut")]
    ReadyForReview,
    /// The self-review agent is running over the committed diff.
    Reviewing,
    /// Self-review produced verdicts; the user picks which to apply.
    SelectingFixes { verdicts: Vec<FixVerdict> },
    /// The agent is applying the selected self-review fixes.
    ApplyingFixes,
    /// The project's validate command is running in the card's worktree.
    /// `attempt` is 1-based: which check run of the current gate pass this is.
    Validating { attempt: u32 },
    /// An agent run is fixing the `attempt`-th validation failure. `output` is
    /// the capped tail of that failure, kept in the state so a crash-resume can
    /// rebuild the fix prompt.
    FixingValidation { attempt: u32, output: String },
    /// The attempt budget is exhausted; parked for the user with the last
    /// failing output. Offers: re-run the gate, one more agent fix, or open
    /// the PR anyway.
    ValidationFailed { attempt: u32, output: String },
    /// Review done (or skipped) and validation passed (or skipped, or not
    /// configured); ready to open the pull request.
    ReadyForPr,
}

/// The full lifecycle state of a card. The board column is derived from this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CardState {
    StartingBlock,
    Designing(DesignSub),
    /// A read-only investigation run (an "investigate only" card — see
    /// [`crate::domain::config::CardKind`]). Shares [`RunSub`] with
    /// `Implementing` so mid-run questions park on the same intervention flow.
    Investigating(RunSub),
    /// The investigation finished; its conclusion awaits the user. From here the
    /// card can take a follow-up round (back to `Investigating`), convert into
    /// an implementation (via `ResetToStart`), or be marked done.
    Concluded {
        conclusion: String,
    },
    Implementing(RunSub),
    AwaitingReview(ReviewSub),
    PrReview(PrReviewSub),
    ReadyToMerge,
    /// Terminal: the card is finished — reached by merging its PR or by the user
    /// marking it done. (Serialized as `Done`; older records used `Merged`.)
    #[serde(alias = "Merged")]
    Done,
    /// A run faulted. Carries the state to return to on retry.
    Failed {
        previous: Box<CardState>,
        message: String,
    },
    /// A read-only question run (the user asked the agent something while the
    /// work was parked). Carries the state to return to once answered, like
    /// `Failed`, so a crash/retry re-runs the question — not the parked phase.
    Answering {
        previous: Box<CardState>,
        question: String,
    },
}

impl CardState {
    pub fn column(&self) -> Column {
        match self {
            CardState::StartingBlock => Column::StartingBlock,
            CardState::Designing(_) => Column::Designing,
            // Investigations borrow the two "thinking" columns: the run sits
            // with Designing, and the finished conclusion parks with the other
            // work awaiting the user's read.
            CardState::Investigating(_) => Column::Designing,
            CardState::Concluded { .. } => Column::AwaitingReview,
            CardState::Implementing(_) => Column::Implementing,
            // The pre-PR gate spans two columns: the freshly-implemented work
            // awaiting the user's review sits in `AwaitingReview`; the active
            // self-review pass (running, fix picker, ready-for-PR) sits in
            // `SelfReview`, mirroring the `PrReview` column.
            CardState::AwaitingReview(ReviewSub::ReadyForReview) => Column::AwaitingReview,
            CardState::AwaitingReview(
                ReviewSub::Reviewing
                | ReviewSub::SelectingFixes { .. }
                | ReviewSub::ApplyingFixes
                | ReviewSub::Validating { .. }
                | ReviewSub::FixingValidation { .. }
                | ReviewSub::ValidationFailed { .. }
                | ReviewSub::ReadyForPr,
            ) => Column::SelfReview,
            CardState::PrReview(_) => Column::PrReview,
            CardState::ReadyToMerge => Column::ReadyToMerge,
            CardState::Done => Column::Done,
            CardState::Failed { previous, .. } => previous.column(),
            CardState::Answering { previous, .. } => previous.column(),
        }
    }

    /// The card's underlying lifecycle state, looking through a fault to the
    /// state it will return to on retry. Lifecycle *buttons* key off the raw
    /// state (a faulted card offers Retry, not Merge), but affordances tied to
    /// on-disk artifacts — the worktree preview, the diff — should stay reachable
    /// while a run is faulted, since the worktree is still there. Non-faulted
    /// states return themselves.
    pub fn effective(&self) -> &CardState {
        match self {
            CardState::Failed { previous, .. } => previous.effective(),
            CardState::Answering { previous, .. } => previous.effective(),
            other => other,
        }
    }

    /// True when the card is blocked waiting on a human decision.
    pub fn needs_intervention(&self) -> bool {
        matches!(
            self,
            CardState::Designing(DesignSub::Intervention(_))
                | CardState::Investigating(RunSub::Intervention(_))
                | CardState::Implementing(RunSub::Intervention(_))
                | CardState::Designing(DesignSub::AwaitingApproval { .. })
                | CardState::PrReview(PrReviewSub::SelectingFixes { .. })
                | CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        )
    }

    /// True when the card is parked waiting for the user to act. A superset of
    /// [`Self::needs_intervention`] that also covers the hand-off points between
    /// phases (review the finished work, merge) and faulted runs. Drives the
    /// macOS dock badge. Deliberately excludes `StartingBlock` (created but not
    /// yet started by the user), the running sub-states, and the terminal `Done`.
    ///
    /// Note `PrReview(Idle)` (PR open, awaiting comments) is intentionally *not*
    /// here: whether it warrants the badge depends on the fetched reviewer
    /// comments, which only [`Card::needs_attention`] can see.
    pub fn needs_attention(&self) -> bool {
        self.needs_intervention()
            || matches!(
                self,
                CardState::AwaitingReview(
                    ReviewSub::ReadyForReview
                        | ReviewSub::ReadyForPr
                        | ReviewSub::ValidationFailed { .. }
                ) | CardState::Concluded { .. }
                    | CardState::ReadyToMerge
                    | CardState::Failed { .. }
            )
    }

    /// The urgent slice of [`Self::needs_attention`]: a run faulted, validation
    /// gave up, or the agent is blocked on a question. Routine hand-offs (plan
    /// approval, review, merge, fix selection) are deliberately excluded so the
    /// red sidebar dot keeps meaning "something went wrong".
    pub fn needs_urgent_attention(&self) -> bool {
        matches!(
            self,
            CardState::Designing(DesignSub::Intervention(_))
                | CardState::Investigating(RunSub::Intervention(_))
                | CardState::Implementing(RunSub::Intervention(_))
                | CardState::AwaitingReview(ReviewSub::ValidationFailed { .. })
                | CardState::Failed { .. }
        )
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, CardState::Failed { .. })
    }

    /// True while an agent run is actively executing (no user action possible).
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            CardState::Designing(DesignSub::Running)
                | CardState::Investigating(RunSub::Running)
                | CardState::Implementing(RunSub::Running)
                | CardState::AwaitingReview(
                    ReviewSub::Reviewing
                        | ReviewSub::ApplyingFixes
                        | ReviewSub::Validating { .. }
                        | ReviewSub::FixingValidation { .. }
                )
                | CardState::PrReview(PrReviewSub::FetchingComments)
                | CardState::PrReview(PrReviewSub::ApplyingFixes)
                | CardState::PrReview(PrReviewSub::ApplyingChange)
                | CardState::Answering { .. }
        )
    }

    /// The active intervention question, if any.
    pub fn intervention(&self) -> Option<&Intervention> {
        match self {
            CardState::Designing(DesignSub::Intervention(i)) => Some(i),
            CardState::Investigating(RunSub::Intervention(i)) => Some(i),
            CardState::Implementing(RunSub::Intervention(i)) => Some(i),
            _ => None,
        }
    }

    /// Short human label for the current sub-state (for badges).
    pub fn status_label(&self) -> &'static str {
        match self {
            CardState::StartingBlock => "configured",
            CardState::Designing(DesignSub::Running) => "designing…",
            CardState::Designing(DesignSub::Intervention(_)) => "needs answer",
            CardState::Designing(DesignSub::AwaitingApproval { plan }) => {
                // Not "ready" while the agent is still waiting on answers.
                if crate::agent::plan::parse_plan(plan).1.is_empty() {
                    "plan ready"
                } else {
                    "questions to answer"
                }
            }
            CardState::Investigating(RunSub::Running) => "investigating…",
            CardState::Investigating(RunSub::Intervention(_)) => "needs answer",
            CardState::Concluded { .. } => "conclusion ready",
            CardState::Implementing(RunSub::Running) => "implementing…",
            CardState::Implementing(RunSub::Intervention(_)) => "needs answer",
            CardState::AwaitingReview(ReviewSub::ReadyForReview) => "awaiting review",
            CardState::AwaitingReview(ReviewSub::Reviewing) => "self-reviewing…",
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. }) => "select fixes",
            CardState::AwaitingReview(ReviewSub::ApplyingFixes) => "applying fixes…",
            CardState::AwaitingReview(ReviewSub::Validating { .. }) => "validating…",
            CardState::AwaitingReview(ReviewSub::FixingValidation { .. }) => "fixing validation…",
            CardState::AwaitingReview(ReviewSub::ValidationFailed { .. }) => "validation failed",
            CardState::AwaitingReview(ReviewSub::ReadyForPr) => "ready for PR",
            CardState::PrReview(PrReviewSub::Idle) => "PR open",
            CardState::PrReview(PrReviewSub::FetchingComments) => "reading comments…",
            CardState::PrReview(PrReviewSub::SelectingFixes { .. }) => "select fixes",
            CardState::PrReview(PrReviewSub::ApplyingFixes) => "applying fixes…",
            CardState::PrReview(PrReviewSub::ApplyingChange) => "applying change…",
            CardState::ReadyToMerge => "ready to merge",
            CardState::Done => "done",
            CardState::Failed { .. } => "failed",
            CardState::Answering { .. } => "answering…",
        }
    }
}

/// Runtime status of a card's in-worktree preview — the project's app launched
/// straight from the card's worktree so the user can test the change without the
/// code ever leaving the worktree. Held in memory by the executor (not persisted);
/// the UI mirrors it into a signal off `PreviewUpdated` events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewStatus {
    /// Running the setup script (deps, isolated DB, per-worktree ports).
    SettingUp,
    /// The app is up; the accompanying `urls` point at it.
    Running,
    /// Not running (never started, or stopped by the user / because it exited).
    Stopped,
    /// Setup or launch failed; carries the reason for a toast.
    Failed(String),
}

/// A clickable address of a running preview (`label` = service, e.g. "frontend").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewUrl {
    pub label: String,
    pub url: String,
}

// ---------------------------------------------------------------------------
// PR review data
// ---------------------------------------------------------------------------

/// A single inline review comment fetched from the forge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub author: String,
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
}

/// The agent's verdict on whether a review comment is worth fixing, plus the
/// user's keep/skip selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixVerdict {
    pub comment: ReviewComment,
    pub worth_fixing: bool,
    /// The agent's honest severity assessment, independent of `worth_fixing`:
    /// `critical` | `high` | `medium` | `low` (empty if unrated).
    #[serde(default)]
    pub severity: String,
    pub rationale: String,
    pub selected: bool,
    /// A short explanation the agent will post as a reply on the comment when the
    /// user chooses *not* to fix it. Only used for PR-comment triage (comments
    /// there are real GitHub comments); empty for local self-review verdicts.
    #[serde(default)]
    pub reply: String,
}

/// One review thread on a PR: the root comment plus its replies, with the
/// thread-level facts the flat comment list can't carry — GitHub's resolved
/// flag and who spoke last. Fetched via GraphQL (`Forge::list_threads`); the
/// node `id` is what the resolve mutation is keyed by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    /// GraphQL node id of the thread (not a comment id).
    pub id: String,
    /// Marked resolved on GitHub (by us after a fix, or by anyone by hand).
    pub resolved: bool,
    /// Database ids of the thread's comments, oldest first. These match
    /// [`ReviewComment::id`] from the REST comment list; the first is the
    /// thread's top-level comment — the only id GitHub accepts replies to.
    pub comment_ids: Vec<u64>,
    /// Whether the thread's last comment was authored by the authenticated
    /// user (i.e. we — or the agent acting as us — have the last word).
    pub last_by_viewer: bool,
}

impl ReviewThread {
    /// Whether this thread still awaits an answer from us: not resolved, and
    /// the reviewer (not the viewer) has the last word. This is what gates the
    /// merge gate's "reevaluate comments" offer and what a triage pass reads —
    /// threads we fixed (resolved) or replied to are done, unless the reviewer
    /// followed up after our reply.
    pub fn is_unanswered(&self) -> bool {
        !self.resolved && !self.comment_ids.is_empty() && !self.last_by_viewer
    }
}

/// A submitted PR review: who reviewed and their verdict. Distinct from the
/// *requested* reviewers (collaborators) returned by `list_reviewers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub author: String,
    /// `APPROVED` / `CHANGES_REQUESTED` / `COMMENTED` / `DISMISSED`.
    pub state: String,
}

impl ReviewSummary {
    /// Whether this review approves the PR. GitHub uppercases these, but we
    /// compare case-insensitively so a stray casing can't silently drop the
    /// badge (or strand a card short of the merge gate).
    pub fn is_approved(&self) -> bool {
        self.state.eq_ignore_ascii_case("APPROVED")
    }

    /// Whether this review is blocking on changes the user still owes.
    pub fn requests_changes(&self) -> bool {
        self.state.eq_ignore_ascii_case("CHANGES_REQUESTED")
    }

    /// Whether this review is a definitive verdict the user needs to act on — an
    /// approval to merge, or a change request to address. A bare `COMMENTED`
    /// review (frequently an automated one) or a `DISMISSED` prior review isn't;
    /// any inline comments it carried are surfaced separately via the reviewer
    /// comment count.
    pub fn is_actionable(&self) -> bool {
        self.is_approved() || self.requests_changes()
    }
}

/// Metadata about a created pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrInfo {
    pub number: u64,
    pub url: String,
    pub title: String,
    pub state: String,
    /// The GitHub login requested to review this PR when it was opened. Anchors
    /// the reviewer-comment count that drives the dock badge — comparing comment
    /// authors against this, not a project-wide setting, so a later config change
    /// (or a per-PR override) doesn't misattribute feedback. `None` for PRs
    /// opened with no reviewer or before this was captured; `#[serde(default)]`
    /// keeps those records loadable.
    #[serde(default)]
    pub reviewer: Option<String>,
    /// Whether `reviewer` records the actual choice made when the PR was
    /// opened — which makes `None` mean "the user explicitly asked for no
    /// reviewer", not "unknown". Records persisted before this was captured
    /// deserialize `false`, and only for them does
    /// [`Self::effective_reviewer`] assume the project's configured reviewer.
    #[serde(default)]
    pub reviewer_recorded: bool,
}

impl PrInfo {
    /// The reviewer whose verdict this PR is waiting on, shared by the eager
    /// advance at PR creation, the background poll, and the manual refresh.
    ///
    /// `fallback` is the project's configured reviewer. It applies only to
    /// legacy records that never captured a per-PR choice: on those, `None`
    /// most plausibly means the PR was opened for the configured reviewer
    /// before we stored it here. On a PR that did record its choice, `None` is
    /// the user's explicit "no reviewer" and the fallback must NOT override it
    /// — nobody was requested on GitHub, so no approval will ever land, and
    /// waiting on the configured reviewer would strand the card at the PR gate.
    pub fn effective_reviewer<'a>(&'a self, fallback: Option<&'a str>) -> Option<&'a str> {
        if self.reviewer_recorded {
            self.reviewer.as_deref()
        } else {
            self.reviewer.as_deref().or(fallback)
        }
    }
}

/// Token usage reported by a provider run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

// ---------------------------------------------------------------------------
// PR review workflow (reviewing other contributors' PRs)
// ---------------------------------------------------------------------------

/// The verdict attached to a submitted GitHub review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ReviewEvent {
    /// A plain review with comments and no explicit approval (`COMMENT`).
    #[default]
    Comment,
    /// Request changes (`REQUEST_CHANGES`).
    RequestChanges,
    /// Approve the PR (`APPROVE`).
    Approve,
}

impl ReviewEvent {
    /// The value the GitHub reviews API expects for `event`.
    pub fn api_value(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Approve => "APPROVE",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "Comment",
            ReviewEvent::RequestChanges => "Request changes",
            ReviewEvent::Approve => "Approve",
        }
    }
    pub fn all() -> [ReviewEvent; 3] {
        [
            ReviewEvent::Comment,
            ReviewEvent::RequestChanges,
            ReviewEvent::Approve,
        ]
    }
}

/// A single inline comment the review agent drafted, plus the user's keep/skip
/// selection. Becomes a GitHub review comment when the review is published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftComment {
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
    /// `critical` | `high` | `medium` | `low` (empty if unrated).
    #[serde(default)]
    pub severity: String,
    /// Whether the user has this comment checked for inclusion in the review.
    pub selected: bool,
}

/// The rolled-up state of a PR's CI checks, collapsed from GitHub's
/// `statusCheckRollup` to the four outcomes the board distinguishes. A failing
/// or still-running build is worth knowing *before* spending a review pass on
/// the PR, so it rides on the task rather than requiring a trip to GitHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum CheckStatus {
    /// No checks are configured, or none have reported yet.
    #[default]
    None,
    /// At least one check is queued or in progress, and none has failed.
    Pending,
    /// Every check that reported concluded successfully (or was skipped).
    Passing,
    /// At least one check failed, errored, or timed out.
    Failing,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::None => "no checks",
            CheckStatus::Pending => "checks running",
            CheckStatus::Passing => "checks passing",
            CheckStatus::Failing => "checks failing",
        }
    }

    /// A compact glyph for the board card, where space is at a premium.
    pub fn glyph(self) -> &'static str {
        match self {
            CheckStatus::None => "–",
            CheckStatus::Pending => "•",
            CheckStatus::Passing => "✓",
            CheckStatus::Failing => "✗",
        }
    }

    /// The CSS modifier for this status (`check check-<x>`).
    pub fn css_class(self) -> &'static str {
        match self {
            CheckStatus::None => "check",
            CheckStatus::Pending => "check check-pending",
            CheckStatus::Passing => "check check-pass",
            CheckStatus::Failing => "check check-fail",
        }
    }

    /// Whether this is worth showing at all — "no checks" is noise on a repo
    /// that runs none.
    pub fn is_reportable(self) -> bool {
        !matches!(self, CheckStatus::None)
    }
}

/// Whether a PR merges cleanly into its base, from GitHub's `mergeable` field.
/// `Unknown` is GitHub still computing it (or not reporting), and is not shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Mergeable {
    #[default]
    Unknown,
    /// Merges cleanly.
    Clean,
    /// Conflicts with the base branch.
    Conflicting,
}

impl Mergeable {
    pub fn is_conflicting(self) -> bool {
        matches!(self, Mergeable::Conflicting)
    }
}

/// The lifecycle state of a PR under review. The review-board column is derived
/// from this via [`ReviewStatus::column`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    /// Discovered by the poll; no review run started yet.
    ToReview,
    /// The branch is checked out in a worktree and the review agent is running.
    Reviewing,
    /// The agent finished; the user picks/edits which drafted comments to send.
    AwaitingValidation {
        drafts: Vec<DraftComment>,
        summary: String,
        event: ReviewEvent,
    },
    /// The review was submitted to GitHub.
    Reviewed,
    /// A run faulted. Carries the state to return to on retry.
    Failed {
        previous: Box<ReviewStatus>,
        message: String,
    },
}

impl ReviewStatus {
    pub fn column(&self) -> ReviewColumn {
        match self {
            ReviewStatus::ToReview => ReviewColumn::ToReview,
            ReviewStatus::Reviewing => ReviewColumn::Reviewing,
            ReviewStatus::AwaitingValidation { .. } => ReviewColumn::AwaitingValidation,
            ReviewStatus::Reviewed => ReviewColumn::Reviewed,
            ReviewStatus::Failed { previous, .. } => previous.column(),
        }
    }

    /// True while the review agent is actively running (drives startup recovery).
    pub fn is_running(&self) -> bool {
        matches!(self, ReviewStatus::Reviewing)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, ReviewStatus::Failed { .. })
    }

    /// True when the user must act: a PR waiting to be reviewed, drafts waiting
    /// to be validated, or a faulted run. Feeds the sidebar / dock badge counts.
    pub fn needs_attention(&self) -> bool {
        matches!(
            self,
            ReviewStatus::ToReview
                | ReviewStatus::AwaitingValidation { .. }
                | ReviewStatus::Failed { .. }
        )
    }

    pub fn status_label(&self) -> &'static str {
        match self {
            ReviewStatus::ToReview => "to review",
            ReviewStatus::Reviewing => "reviewing…",
            ReviewStatus::AwaitingValidation { .. } => "validate",
            ReviewStatus::Reviewed => "reviewed",
            ReviewStatus::Failed { .. } => "failed",
        }
    }
}

/// A pull request authored by someone else that the user is reviewing. Distinct
/// from [`Card`] (which represents the user's *own* work and owns its branch/PR).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewTask {
    pub id: Uuid,
    pub project_id: Uuid,
    pub pr_number: u64,
    pub pr_title: String,
    pub author: String,
    pub url: String,
    /// The PR's description as written by its author. Reviewing without it means
    /// judging the diff with no stated intent, so the scan stores it and the
    /// detail panel shows it above everything else. `#[serde(default)]` keeps
    /// tasks recorded before this existed loadable.
    #[serde(default)]
    pub body: String,
    /// Rolled-up CI state, refreshed by every scan.
    #[serde(default)]
    pub checks: CheckStatus,
    /// Whether the PR merges cleanly, refreshed by every scan.
    #[serde(default)]
    pub mergeable: Mergeable,
    /// The PR's head branch name (for display; the review checkout uses the
    /// `pull/<n>/head` ref so it works for forks too).
    pub head_ref: String,
    /// The branch the PR targets, used as the diff base for the review agent.
    pub base_ref: String,
    pub status: ReviewStatus,
    /// The user's free-form steering for this review ("focus on the migration",
    /// "the author is new — explain your reasoning"), folded into the run's
    /// prompt. Set from the panel when the review is started, so a retry keeps
    /// steering the same way. `#[serde(default)]` keeps older tasks loadable.
    #[serde(default)]
    pub guidance: String,
    pub worktree_path: Option<PathBuf>,
    /// Stable id minted at task creation, surfaced to providers via `RunConfig`.
    pub session_id: Uuid,
    /// Provider session id of the most recent review run (for `--resume`).
    pub last_session: Option<String>,
    #[serde(rename = "cost_usd", default)]
    pub cost: Cost,
    pub created_at: i64,
    pub updated_at: i64,
}

impl ReviewTask {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        project_id: Uuid,
        pr_number: u64,
        pr_title: impl Into<String>,
        author: impl Into<String>,
        url: impl Into<String>,
        head_ref: impl Into<String>,
        base_ref: impl Into<String>,
    ) -> Self {
        let now = now_millis();
        ReviewTask {
            id: Uuid::new_v4(),
            project_id,
            pr_number,
            pr_title: pr_title.into(),
            author: author.into(),
            url: url.into(),
            body: String::new(),
            checks: CheckStatus::None,
            mergeable: Mergeable::Unknown,
            head_ref: head_ref.into(),
            base_ref: base_ref.into(),
            status: ReviewStatus::ToReview,
            guidance: String::new(),
            worktree_path: None,
            session_id: Uuid::new_v4(),
            last_session: None,
            cost: Cost::ZERO,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn column(&self) -> ReviewColumn {
        self.status.column()
    }

    /// The local branch a review checkout lands on. Stable per PR, so the diff
    /// viewer and the review run agree on what to read without coordinating.
    pub fn local_branch(&self) -> String {
        format!("usine-review/{}", self.pr_number)
    }

    /// The base to diff this PR against, falling back to the project's default
    /// when GitHub reported none.
    pub fn diff_base(&self, project_default: &str) -> String {
        if self.base_ref.is_empty() {
            project_default.to_string()
        } else {
            self.base_ref.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// Project & card
// ---------------------------------------------------------------------------

/// A project is a local folder (expected to be a git repo).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub path: PathBuf,
    pub config: crate::domain::config::ProjectConfig,
}

impl Project {
    pub fn new(
        name: impl Into<String>,
        path: PathBuf,
        config: crate::domain::config::ProjectConfig,
    ) -> Self {
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path,
            config,
        }
    }
}

/// A unit of work moving across the board.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub id: Uuid,
    pub project_id: Uuid,
    pub title: String,
    /// The task prompt handed to the agent.
    pub description: String,
    pub state: CardState,
    pub config: CardConfig,
    /// A stable id minted at card creation and surfaced to providers via
    /// [`crate::agent::provider::RunConfig`]. The real CLIs derive and report their own
    /// session id through the run's init event (captured in `last_session`); the
    /// simulator uses this one directly.
    pub session_id: Uuid,
    /// The provider (Claude) session id of the most recent run, captured from
    /// its init event. Used to `--resume` that conversation on a Resume or when
    /// answering a mid-run question.
    pub last_session: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub pr: Option<PrInfo>,
    #[serde(rename = "cost_usd", default)]
    pub cost: Cost,
    /// Clarifying questions answered and change requests made during this card's
    /// runs. Captured so "send back to the starting block" can fold them into the
    /// prompt for a clean restart. Each entry is a self-contained, readable block.
    /// `#[serde(default)]` keeps records written before this field existed loadable.
    #[serde(default)]
    pub qa_log: Vec<String>,
    /// How many review comments this card's PR has from the **assigned** reviewer
    /// (the one requested on the PR, or the project default), as of the last
    /// background poll or manual refresh. Drives the dock badge for a
    /// `PrReview(Idle)` card, so *only* the assigned reviewer's feedback nags — a
    /// comment from another reviewer is surfaced through [`Self::comment_count`]
    /// (the panel's triage button) without lighting the badge.
    /// `#[serde(default)]` keeps older records loadable.
    #[serde(default)]
    pub reviewer_comment_count: usize,
    /// How many review comments this card's PR has in total, across *all*
    /// reviewers, as of the last background poll or manual refresh. Gates the
    /// panel's triage button so feedback from any reviewer — not just the assigned
    /// one — can be read and addressed, even when (per [`Self::reviewer_comment_count`])
    /// it doesn't badge the card. `#[serde(default)]` keeps older records loadable.
    #[serde(default)]
    pub comment_count: usize,
    /// How many review *threads* on this card's PR still await an answer from
    /// us (unresolved, with the reviewer having the last word — see
    /// [`ReviewThread::is_unanswered`]), as of the last background poll or
    /// refresh. Unlike the raw comment counts this goes back to zero once a
    /// triage pass fixes or replies to everything, so it can gate the merge
    /// gate's "reevaluate comments" offer: it is non-zero exactly when comments
    /// landed after (or survived) the last pass. `#[serde(default)]` keeps
    /// older records loadable.
    #[serde(default)]
    pub unanswered_count: usize,
    /// The reviews submitted on this card's PR (who reviewed, and their verdict),
    /// as of the last background poll or manual refresh. Kept on the card — rather
    /// than fetched by the panel on open — so the same poll that refreshes
    /// [`Self::reviewer_comment_count`] keeps this list in step with it.
    /// `#[serde(default)]` keeps older records loadable.
    #[serde(default)]
    pub reviews: Vec<ReviewSummary>,
    /// The rolled-up CI state of this card's PR, as of the last background poll
    /// or manual refresh. Gates the merge button: a red or still-running build
    /// blocks the merge (with an explicit "merge anyway" override), and a red
    /// one offers an agent fix. `#[serde(default)]` keeps older records loadable.
    #[serde(default)]
    pub checks: CheckStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Card {
    pub fn new(
        project_id: Uuid,
        title: impl Into<String>,
        description: impl Into<String>,
        config: CardConfig,
    ) -> Self {
        let now = now_millis();
        Card {
            id: Uuid::new_v4(),
            project_id,
            title: title.into(),
            description: description.into(),
            state: CardState::StartingBlock,
            config,
            session_id: Uuid::new_v4(),
            last_session: None,
            worktree_path: None,
            branch: None,
            pr: None,
            cost: Cost::ZERO,
            qa_log: Vec::new(),
            reviewer_comment_count: 0,
            comment_count: 0,
            unanswered_count: 0,
            reviews: Vec::new(),
            checks: CheckStatus::None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn column(&self) -> Column {
        self.state.column()
    }

    /// Whether this card should light the dock badge. Delegates to
    /// [`CardState::needs_attention`], with the two corrections the state alone
    /// can't make because they turn on what the PR poll found.
    ///
    /// A PR parked in `PrReview(Idle)` only counts once a review has actually
    /// landed — either the assigned reviewer left comments
    /// ([`Self::reviewer_comment_count`]) or a submitted review carries an
    /// actionable verdict ([`ReviewSummary::is_actionable`]). The latter is what
    /// catches an approval or change request that came in with no inline
    /// comments, which the count alone would miss. The background poll keeps both
    /// fresh, so a freshly opened PR awaiting review still doesn't nag on its own.
    ///
    /// And a card at the merge gate stops counting while its build is in flight —
    /// see [`Self::merge_gate_waits_on_ci`].
    pub fn needs_attention(&self) -> bool {
        if self.merge_gate_waits_on_ci() {
            return false;
        }
        self.state.needs_attention()
            || (matches!(self.state, CardState::PrReview(PrReviewSub::Idle))
                && (self.reviewer_comment_count > 0
                    || self.reviews.iter().any(ReviewSummary::is_actionable)))
    }

    /// True when `ReadyToMerge` is waiting on CI rather than on the user: the
    /// build is still running and no review thread is outstanding.
    ///
    /// The merge gate offers nothing but "refresh" and the explicit "merge
    /// anyway" override in that window — the executor refuses a plain merge while
    /// checks are pending — so badging it nags for an action the user can't
    /// usefully take. Same reasoning that keeps `PrReview(Idle)` out of
    /// [`CardState::needs_attention`]. The poll refreshes checks on
    /// `ReadyToMerge` cards, so the badge returns on its own once the build goes
    /// green or red.
    ///
    /// Deliberately narrow on both sides. Only `Pending` suppresses:
    /// [`CheckStatus::None`] — no checks configured, or nothing polled yet —
    /// badges as before, and since a failed `pr_checks` leaves the last-known
    /// status on the card, a broken `gh` can't silence a mergeable card. And an
    /// unanswered review thread keeps the badge lit regardless, because that one
    /// *is* actionable while CI runs (the gate's "reevaluate comments" offer) and
    /// it's exactly the late comment the poll covers `ReadyToMerge` cards to
    /// catch.
    fn merge_gate_waits_on_ci(&self) -> bool {
        matches!(self.state, CardState::ReadyToMerge)
            && self.checks == CheckStatus::Pending
            && self.unanswered_count == 0
    }

    /// The urgent slice of [`Self::needs_attention`]. Pure delegation to
    /// [`CardState::needs_urgent_attention`]: a landed review on a parked PR is
    /// "waiting", never urgent.
    pub fn needs_urgent_attention(&self) -> bool {
        self.state.needs_urgent_attention()
    }

    /// Whether an approval has landed with nothing left to triage, so the card
    /// can go straight to the merge gate.
    ///
    /// The comment-triage chain (`FetchComments` → … → `AgentFixesDone`) is the
    /// other route into `ReadyToMerge` (besides [`Self::no_reviewer_clears_merge`]),
    /// and it needs a comment to chew on: an approval submitted with no inline
    /// comments leaves the card parked in `PrReview(Idle)` badging for attention
    /// that the panel offers no action for. This is the predicate that unsticks
    /// it — see
    /// [`Transition::ReviewApproved`](crate::domain::state_machine::Transition).
    ///
    /// Deliberately conservative on both sides. Any comment at all (from the
    /// assigned reviewer or another one) means triage still has work to do, so
    /// the user reads it before merging rather than having the card slide past.
    /// And a change request anywhere blocks: `latestReviews` reports one verdict
    /// per reviewer, so an approval from one reviewer must not clear a second
    /// reviewer's outstanding objection.
    pub fn approval_clears_merge(&self) -> bool {
        matches!(self.state, CardState::PrReview(PrReviewSub::Idle))
            && self.comment_count == 0
            && self.reviews.iter().any(ReviewSummary::is_approved)
            && !self.reviews.iter().any(ReviewSummary::requests_changes)
    }

    /// The reviewers whose latest review approves this card's PR.
    /// `latestReviews` keeps one verdict per reviewer, so this is the set of
    /// standing approvals — what the board badge and the merge panel restate,
    /// so the approval that carried the card to the merge gate stays visible
    /// there. Empty when none has landed (including the no-reviewer route to
    /// `ReadyToMerge`).
    pub fn approved_by(&self) -> Vec<&str> {
        self.reviews
            .iter()
            .filter(|r| r.is_approved())
            .map(|r| r.author.as_str())
            .collect()
    }

    /// Whether a PR with no reviewer to wait on can go straight to the merge
    /// gate. A project without a configured reviewer opens PRs nobody is asked
    /// to review: no approval will ever land, so [`Self::approval_clears_merge`]
    /// never fires and the card would strand at the PR gate forever.
    ///
    /// `reviewer` is the *effective* reviewer — see
    /// [`PrInfo::effective_reviewer`], which every caller goes through. Same
    /// conservatism as the approval route: any comment from anyone
    /// means the user triages it before merging, and an unsolicited
    /// CHANGES_REQUESTED review blocks even though nobody was asked for it.
    pub fn no_reviewer_clears_merge(&self, reviewer: Option<&str>) -> bool {
        matches!(self.state, CardState::PrReview(PrReviewSub::Idle))
            && reviewer.is_none()
            && self.comment_count == 0
            && !self.reviews.iter().any(ReviewSummary::requests_changes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_rounds_serializes_deterministically_and_reads_both_forms() {
        // Provider dollar amounts round to the nearest cent.
        assert_eq!(Cost::from_usd(10.125_862_999_999_999).cents(), 1013);
        assert_eq!(Cost::from_usd(0.004).cents(), 0);
        assert_eq!(Cost::from_usd(-1.0), Cost::ZERO);

        // On disk it's a cent-valued dollars float...
        let c = Cost::from_cents(1013);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "10.13");
        // ...which round-trips byte-for-byte — the fixed-point property native_db's
        // update/remove depend on, and which a drifted f64 like 10.125862999999999
        // (re-serializes to 10.125863) did NOT have.
        assert_eq!(serde_json::from_str::<Cost>(&json).unwrap(), c);
        let reparsed =
            serde_json::to_string(&serde_json::from_str::<Cost>(&json).unwrap()).unwrap();
        assert_eq!(reparsed, json);

        // A messy legacy float loads (rounded) and then serializes to that same
        // canonical, stable form.
        let from_legacy = serde_json::from_str::<Cost>("10.125862999999999").unwrap();
        assert_eq!(from_legacy, c);
        assert_eq!(serde_json::to_string(&from_legacy).unwrap(), "10.13");
        assert_eq!(serde_json::from_str::<Cost>("0.0").unwrap(), Cost::ZERO);
        // A bare integer is accepted as cents (defensive).
        assert_eq!(serde_json::from_str::<Cost>("1013").unwrap(), c);

        // Accumulation is exact integer addition, not f64 drift.
        let mut total = Cost::ZERO;
        for _ in 0..3 {
            total += Cost::from_usd(0.10);
        }
        assert_eq!(total.cents(), 30);
        assert_eq!(total.to_string(), "$0.30");
    }

    fn intervention() -> Intervention {
        Intervention {
            request_id: "r1".into(),
            question: "?".into(),
            options: vec![],
        }
    }

    #[test]
    fn needs_attention_covers_user_action_states() {
        // States where the user must act → badge.
        for s in [
            CardState::Designing(DesignSub::Intervention(intervention())),
            CardState::Implementing(RunSub::Intervention(intervention())),
            CardState::Designing(DesignSub::AwaitingApproval { plan: "p".into() }),
            CardState::PrReview(PrReviewSub::SelectingFixes { verdicts: vec![] }),
            CardState::Investigating(RunSub::Intervention(intervention())),
            CardState::Concluded {
                conclusion: "the cache is bounded".into(),
            },
            CardState::AwaitingReview(ReviewSub::ReadyForReview),
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts: vec![] }),
            CardState::AwaitingReview(ReviewSub::ReadyForPr),
            CardState::ReadyToMerge,
            CardState::Failed {
                previous: Box::new(CardState::Implementing(RunSub::Running)),
                message: "boom".into(),
            },
        ] {
            assert!(s.needs_attention(), "{s:?} should need attention");
        }
    }

    #[test]
    fn needs_attention_excludes_running_unstarted_and_done() {
        // Running, freshly-created, and terminal states → no badge.
        for s in [
            CardState::StartingBlock,
            CardState::Designing(DesignSub::Running),
            CardState::Investigating(RunSub::Running),
            CardState::Implementing(RunSub::Running),
            CardState::AwaitingReview(ReviewSub::Reviewing),
            CardState::AwaitingReview(ReviewSub::ApplyingFixes),
            CardState::PrReview(PrReviewSub::FetchingComments),
            CardState::PrReview(PrReviewSub::ApplyingFixes),
            // Idle by itself is not attention-worthy — only once the assigned
            // reviewer has left comments (see `card_idle_pr_badges_only_with_reviewer_comments`).
            CardState::PrReview(PrReviewSub::Idle),
            CardState::Done,
        ] {
            assert!(!s.needs_attention(), "{s:?} should not need attention");
        }
    }

    #[test]
    fn urgent_attention_is_the_broken_subset_of_attention() {
        // Faulted, exhausted validation, and agent questions → red tier.
        let urgent = [
            CardState::Designing(DesignSub::Intervention(intervention())),
            CardState::Investigating(RunSub::Intervention(intervention())),
            CardState::Implementing(RunSub::Intervention(intervention())),
            CardState::AwaitingReview(ReviewSub::ValidationFailed {
                attempt: 3,
                output: "boom".into(),
            }),
            CardState::Failed {
                previous: Box::new(CardState::Implementing(RunSub::Running)),
                message: "boom".into(),
            },
        ];
        for s in urgent {
            assert!(s.needs_urgent_attention(), "{s:?} should be urgent");
            // Urgent ⊆ attention.
            assert!(s.needs_attention(), "{s:?} urgent implies attention");
        }

        // Routine hand-offs badge but stay in the accent "waiting" tier.
        for s in [
            CardState::Designing(DesignSub::AwaitingApproval { plan: "p".into() }),
            CardState::PrReview(PrReviewSub::SelectingFixes { verdicts: vec![] }),
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts: vec![] }),
            CardState::AwaitingReview(ReviewSub::ReadyForReview),
            CardState::AwaitingReview(ReviewSub::ReadyForPr),
            CardState::ReadyToMerge,
        ] {
            assert!(s.needs_attention(), "{s:?} should need attention");
            assert!(!s.needs_urgent_attention(), "{s:?} should not be urgent");
        }
    }

    #[test]
    fn validation_states_membership_and_serde() {
        let validating = CardState::AwaitingReview(ReviewSub::Validating { attempt: 1 });
        let fixing = CardState::AwaitingReview(ReviewSub::FixingValidation {
            attempt: 1,
            output: "boom".into(),
        });
        let parked = CardState::AwaitingReview(ReviewSub::ValidationFailed {
            attempt: 3,
            output: "boom".into(),
        });

        // The check and the fix run behave as running states; the exhausted
        // failure is a park that badges but is not a fix-picker intervention.
        assert!(validating.is_running() && fixing.is_running());
        assert!(!parked.is_running());
        assert!(parked.needs_attention() && !parked.needs_intervention());
        assert!(!validating.needs_attention() && !fixing.needs_attention());

        // All three live in the self-review column, like the rest of the gate.
        for s in [&validating, &fixing, &parked] {
            assert_eq!(s.column(), Column::SelfReview);
        }

        // The new variants round-trip through the store's JSON codec shape.
        for s in [validating, fixing, parked] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<CardState>(&json).unwrap(), s);
        }
    }

    #[test]
    fn investigation_states_membership_columns_and_serde() {
        let running = CardState::Investigating(RunSub::Running);
        let parked = CardState::Investigating(RunSub::Intervention(intervention()));
        let concluded = CardState::Concluded {
            conclusion: "the auth flow re-validates on every request".into(),
        };

        // The run behaves like any other agent run; the conclusion is a park
        // that badges (it *is* the notification) without being an intervention.
        assert!(running.is_running() && !running.needs_attention());
        assert!(parked.needs_intervention());
        assert_eq!(parked.intervention().unwrap().request_id, "r1");
        assert!(concluded.needs_attention() && !concluded.needs_intervention());
        assert!(!concluded.is_running());

        // Investigating renders with Designing; the conclusion parks with the
        // other work awaiting the user's read.
        assert_eq!(running.column(), Column::Designing);
        assert_eq!(parked.column(), Column::Designing);
        assert_eq!(concluded.column(), Column::AwaitingReview);

        // The new variants round-trip through the store's JSON codec shape.
        for s in [running, parked, concluded] {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<CardState>(&json).unwrap(), s);
        }
    }

    #[test]
    fn answering_state_membership_columns_and_serde() {
        // A question run wraps the state it was asked from; membership and
        // column delegate to it, like `Failed`.
        let entries = [
            (
                CardState::Designing(DesignSub::AwaitingApproval { plan: "p".into() }),
                Column::Designing,
            ),
            (
                CardState::AwaitingReview(ReviewSub::ReadyForReview),
                Column::AwaitingReview,
            ),
            (
                CardState::AwaitingReview(ReviewSub::ValidationFailed {
                    attempt: 3,
                    output: "boom".into(),
                }),
                Column::SelfReview,
            ),
            (CardState::PrReview(PrReviewSub::Idle), Column::PrReview),
            (CardState::ReadyToMerge, Column::ReadyToMerge),
        ];
        for (previous, column) in entries {
            let s = CardState::Answering {
                previous: Box::new(previous.clone()),
                question: "why this approach?".into(),
            };
            // It's a running state — no attention tier badges while it runs —
            // and `effective` looks through it so diff/preview stay reachable.
            assert!(s.is_running(), "{s:?} should be running");
            assert!(!s.needs_attention(), "{s:?} should not need attention");
            assert!(!s.needs_intervention() && !s.needs_urgent_attention());
            assert!(s.intervention().is_none());
            assert_eq!(s.column(), column);
            assert_eq!(*s.effective(), previous);
            assert_eq!(s.status_label(), "answering…");

            // Round-trips through the store's JSON codec shape.
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(serde_json::from_str::<CardState>(&json).unwrap(), s);
        }
    }

    #[test]
    fn card_idle_pr_badges_on_reviewer_comments_or_a_verdict() {
        let mut card = Card::new(Uuid::new_v4(), "t", "d", CardConfig::default());
        card.state = CardState::PrReview(PrReviewSub::Idle);

        // PR open, nothing back from the reviewer yet → no badge.
        assert_eq!(card.reviewer_comment_count, 0);
        assert!(card.reviews.is_empty());
        assert!(!card.needs_attention());

        // A review that landed with no inline comments — the count alone stays 0,
        // but a bare `COMMENTED`/`DISMISSED` verdict still isn't actionable.
        card.reviews = vec![
            ReviewSummary {
                author: "bot".into(),
                state: "COMMENTED".into(),
            },
            ReviewSummary {
                author: "old".into(),
                state: "DISMISSED".into(),
            },
        ];
        assert!(!card.needs_attention());

        // An approval with no inline comments → badge (this is the case the
        // comment count misses entirely).
        card.reviews.push(ReviewSummary {
            author: "octocat".into(),
            state: "APPROVED".into(),
        });
        assert!(card.needs_attention());

        // And the comment path still stands on its own.
        card.reviews.clear();
        card.reviewer_comment_count = 2;
        assert!(card.needs_attention());
    }

    #[test]
    fn merge_gate_does_not_badge_while_checks_are_running() {
        let mut card = Card::new(Uuid::new_v4(), "t", "d", CardConfig::default());
        card.state = CardState::ReadyToMerge;

        // Nothing polled yet — the state alone badges, as it always has.
        assert_eq!(card.checks, CheckStatus::None);
        assert!(card.needs_attention());

        // Build in flight: waiting on CI, not on the user.
        card.checks = CheckStatus::Pending;
        assert!(!card.needs_attention());

        // Every settled outcome badges again — green means merge, red means fix.
        for status in [CheckStatus::Passing, CheckStatus::Failing] {
            card.checks = status;
            assert!(card.needs_attention(), "{status:?} should badge");
        }

        // A thread left unanswered is actionable while the build runs, so it
        // outranks the suppression.
        card.checks = CheckStatus::Pending;
        card.unanswered_count = 1;
        assert!(card.needs_attention());

        // Suppressed or not, the merge gate is never the urgent tier.
        card.unanswered_count = 0;
        assert!(!card.needs_urgent_attention());
    }

    #[test]
    fn approval_clears_merge_only_when_there_is_nothing_left_to_triage() {
        let mut card = Card::new(Uuid::new_v4(), "t", "d", CardConfig::default());
        card.state = CardState::PrReview(PrReviewSub::Idle);

        // Open PR, no verdict yet → stay put and wait for the reviewer.
        assert!(!card.approval_clears_merge());

        // A non-verdict review doesn't clear anything either.
        card.reviews = vec![ReviewSummary {
            author: "bot".into(),
            state: "COMMENTED".into(),
        }];
        assert!(!card.approval_clears_merge());

        // Approval, no inline comments → the case with no triage chain to ride.
        card.reviews.push(ReviewSummary {
            author: "octocat".into(),
            state: "APPROVED".into(),
        });
        assert!(card.approval_clears_merge());

        // Any comment at all means triage has work; the user reads it first.
        card.comment_count = 1;
        assert!(!card.approval_clears_merge());
        card.comment_count = 0;

        // A second reviewer still requesting changes blocks the approval —
        // `latestReviews` carries one verdict per reviewer.
        card.reviews.push(ReviewSummary {
            author: "hubot".into(),
            state: "CHANGES_REQUESTED".into(),
        });
        assert!(!card.approval_clears_merge());
        card.reviews.pop();

        // Only from the idle PR gate: mid-triage the chain is already driving.
        for s in [
            CardState::PrReview(PrReviewSub::FetchingComments),
            CardState::PrReview(PrReviewSub::ApplyingFixes),
            CardState::ReadyToMerge,
            CardState::Done,
        ] {
            card.state = s.clone();
            assert!(
                !card.approval_clears_merge(),
                "{s:?} should not auto-advance"
            );
        }
    }

    #[test]
    fn no_reviewer_clears_merge_only_when_nobody_is_asked_and_nothing_landed() {
        let mut card = Card::new(Uuid::new_v4(), "t", "d", CardConfig::default());
        card.state = CardState::PrReview(PrReviewSub::Idle);

        // Nobody to wait on, nothing landed → straight to the merge gate.
        assert!(card.no_reviewer_clears_merge(None));

        // An effective reviewer means the PR gate waits for their verdict.
        assert!(!card.no_reviewer_clears_merge(Some("octocat")));

        // Any comment at all means triage has work; the user reads it first.
        card.comment_count = 1;
        assert!(!card.no_reviewer_clears_merge(None));
        card.comment_count = 0;

        // A non-blocking drive-by review doesn't hold the card back...
        card.reviews = vec![
            ReviewSummary {
                author: "bot".into(),
                state: "COMMENTED".into(),
            },
            ReviewSummary {
                author: "octocat".into(),
                state: "APPROVED".into(),
            },
        ];
        assert!(card.no_reviewer_clears_merge(None));

        // ...but an unsolicited change request does.
        card.reviews.push(ReviewSummary {
            author: "hubot".into(),
            state: "CHANGES_REQUESTED".into(),
        });
        assert!(!card.no_reviewer_clears_merge(None));
        card.reviews.pop();

        // Only from the idle PR gate.
        for s in [
            CardState::PrReview(PrReviewSub::FetchingComments),
            CardState::PrReview(PrReviewSub::ApplyingFixes),
            CardState::ReadyToMerge,
            CardState::Done,
        ] {
            card.state = s.clone();
            assert!(
                !card.no_reviewer_clears_merge(None),
                "{s:?} should not auto-advance"
            );
        }
    }

    #[test]
    fn a_thread_is_unanswered_only_while_the_reviewer_has_the_last_word() {
        let thread = ReviewThread {
            id: "T_1".into(),
            resolved: false,
            comment_ids: vec![10],
            last_by_viewer: false,
        };
        assert!(thread.is_unanswered());

        // Resolved (a fix landed, or someone resolved it by hand) → answered.
        let resolved = ReviewThread {
            resolved: true,
            ..thread.clone()
        };
        assert!(!resolved.is_unanswered());

        // We (or the agent replying as us) spoke last → answered...
        let replied = ReviewThread {
            last_by_viewer: true,
            ..thread.clone()
        };
        assert!(!replied.is_unanswered());

        // ...until the reviewer follows up after our reply.
        let followed_up = ReviewThread {
            comment_ids: vec![10, 11, 12],
            last_by_viewer: false,
            ..thread.clone()
        };
        assert!(followed_up.is_unanswered());

        // A thread with no comments (GraphQL hiccup) never demands attention.
        let empty = ReviewThread {
            comment_ids: vec![],
            ..thread
        };
        assert!(!empty.is_unanswered());
    }

    #[test]
    fn retired_review_states_decode_into_ready_for_review() {
        // Cards written under the retired hand-checkout states must still load
        // (the store's JSON codec matches enum variants by name, and the serde
        // aliases fold the old names into the single `ReadyForReview`).
        for old in ["PreCheckout", "AwaitingCommit", "CheckingOut"] {
            let json = format!("{{\"AwaitingReview\":\"{old}\"}}");
            let state: CardState = serde_json::from_str(&json).unwrap();
            assert!(
                matches!(state, CardState::AwaitingReview(ReviewSub::ReadyForReview)),
                "{old} should decode into ReadyForReview, got {state:?}"
            );
        }
        // The current name still decodes to itself.
        let state: CardState =
            serde_json::from_str("{\"AwaitingReview\":\"ReadyForReview\"}").unwrap();
        assert!(matches!(
            state,
            CardState::AwaitingReview(ReviewSub::ReadyForReview)
        ));
    }

    #[test]
    fn pre_pr_gate_splits_across_awaiting_and_self_review_columns() {
        // The freshly-implemented work awaiting the user's review lives in the
        // Awaiting review column.
        assert_eq!(
            CardState::AwaitingReview(ReviewSub::ReadyForReview).column(),
            Column::AwaitingReview,
            "ReadyForReview → Awaiting review"
        );
        // The active self-review pass (running → ready for PR) lives in the
        // Self-review column.
        for s in [
            ReviewSub::Reviewing,
            ReviewSub::SelectingFixes { verdicts: vec![] },
            ReviewSub::ApplyingFixes,
            ReviewSub::ReadyForPr,
        ] {
            assert_eq!(
                CardState::AwaitingReview(s.clone()).column(),
                Column::SelfReview,
                "{s:?} → Self-review"
            );
        }
    }

    #[test]
    fn review_status_maps_to_the_right_column() {
        assert_eq!(ReviewStatus::ToReview.column(), ReviewColumn::ToReview);
        assert_eq!(ReviewStatus::Reviewing.column(), ReviewColumn::Reviewing);
        assert_eq!(
            ReviewStatus::AwaitingValidation {
                drafts: vec![],
                summary: String::new(),
                event: ReviewEvent::Comment,
            }
            .column(),
            ReviewColumn::AwaitingValidation
        );
        assert_eq!(ReviewStatus::Reviewed.column(), ReviewColumn::Reviewed);
        // Failed keeps the column of the state it came from.
        let failed = ReviewStatus::Failed {
            previous: Box::new(ReviewStatus::Reviewing),
            message: "boom".into(),
        };
        assert_eq!(failed.column(), ReviewColumn::Reviewing);
    }

    #[test]
    fn review_status_attention_and_running() {
        // The user must act on these → badge.
        assert!(ReviewStatus::ToReview.needs_attention());
        assert!(ReviewStatus::AwaitingValidation {
            drafts: vec![],
            summary: String::new(),
            event: ReviewEvent::Comment,
        }
        .needs_attention());
        assert!(ReviewStatus::Failed {
            previous: Box::new(ReviewStatus::Reviewing),
            message: "x".into(),
        }
        .needs_attention());
        // These don't.
        assert!(!ReviewStatus::Reviewing.needs_attention());
        assert!(!ReviewStatus::Reviewed.needs_attention());
        // Only Reviewing is a running state (drives startup reconciliation).
        assert!(ReviewStatus::Reviewing.is_running());
        assert!(!ReviewStatus::ToReview.is_running());
    }
}
