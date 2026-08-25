//! The three message types that flow between the UI, the executor, and the
//! agent providers.
//!
//! - [`ExecutorCommand`]: UI → executor (user intent).
//! - [`AgentEvent`]: provider → executor (normalized agent output).
//! - [`ExecutorEvent`]: executor → UI (state updates, transcript, toasts).

use std::path::PathBuf;

use uuid::Uuid;

use crate::agent::handoff::Handoff;
use crate::agent::usage::UsageSnapshot;
use crate::domain::config::AppSettings;
use crate::domain::model::{
    Card, DraftComment, FixVerdict, PrInfo, PreviewStatus, PreviewUrl, Project, ReviewEvent,
    ReviewTask, Usage,
};

/// Severity for a user-facing toast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Success,
    Warning,
    Error,
}

/// A normalized event emitted by any [`crate::agent::provider::AgentProvider`].
///
/// Each concrete provider (Claude stream-json, Codex `--json`, the simulator)
/// maps its native output onto this enum so the executor is provider-agnostic.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// The run has started; carries the provider session id (for resume).
    Started { session_id: String },
    /// Incremental output to append to the card transcript.
    Progress { text: String },
    /// The agent needs a human answer before continuing.
    NeedsInput {
        request_id: String,
        question: String,
        options: Vec<String>,
    },
    /// A plan-mode run produced a plan and is awaiting approval.
    PlanReady { plan: String },
    /// The run finished successfully.
    Done {
        result: String,
        cost_usd: f64,
        usage: Usage,
    },
    /// The run failed.
    Error { message: String },
}

/// Control messages sent *into* an active run (UI → provider via executor).
#[derive(Debug, Clone)]
pub enum RunControl {
    /// Answer the pending intervention with free text.
    Answer { text: String },
    /// Ask the agent to stop the current turn.
    Interrupt,
    /// Tear the run down entirely.
    Cancel,
}

/// What [`ExecutorCommand::AdoptBranch`] should do about uncommitted changes
/// sitting in a checkout of the adopted branch. "Cancel" never reaches the
/// executor — it's just the dialog's cancel button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyAction {
    /// Snapshot the checkout's uncommitted changes (tracked edits + untracked
    /// files) and commit them in the card's worktree. The user's checkout is
    /// copied from, never touched — it stays dirty.
    Include,
    /// Adopt only the committed work; the checkout keeps its dirty state.
    Ignore,
}

/// What [`ExecutorCommand::ProbeAdoptSource`] found out about a candidate
/// branch, driving the adopt dialog's warnings and prefills. Every field is
/// best-effort except `refusal`: a set refusal means adoption would be
/// rejected, and the dialog blocks submission up front.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoptProbe {
    /// The ref this probe describes, echoed back so the dialog can drop a
    /// stale response after the user picked another branch.
    pub source_ref: String,
    /// Why this ref can't be adopted (doesn't resolve, is the base, belongs to
    /// a card…), or `None` when it can.
    pub refusal: Option<String>,
    /// Commits on the branch since its merge base with the project base.
    pub commits_ahead: usize,
    /// Those commits' subjects, newest first — the description prefill.
    pub subjects: Vec<String>,
    /// The working tree that has the branch checked out with uncommitted
    /// changes, when one does — triggers the include/ignore choice.
    pub dirty_checkout: Option<PathBuf>,
    /// An open PR already on this branch, if the forge knows of one. Adoption
    /// stays allowed; the dialog warns that a second PR would be opened.
    pub open_pr: Option<PrInfo>,
}

/// Which app to launch for [`ExecutorCommand::OpenWorktree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenTarget {
    /// A terminal, opened at the directory.
    Terminal,
    /// An editor/IDE, with the directory as its workspace.
    Editor,
}

/// A command from the UI expressing user intent. Most are card lifecycle
/// actions; the trailing group are direct CRUD operations that exist so the UI
/// never writes to the store on its own thread — it sends these and updates its
/// signals optimistically, and the executor performs the (off-UI-thread) write.
#[derive(Debug, Clone)]
pub enum ExecutorCommand {
    /// Start the card: the design/plan phase, or straight to implementing if the
    /// card is marked "skip plan".
    Start { card_id: Uuid },
    /// Answer the active intervention.
    Answer { card_id: Uuid, text: String },
    /// Approve the proposed plan and begin implementing.
    ApprovePlan { card_id: Uuid },
    /// Reject the plan and send the agent back to designing with feedback.
    RejectPlan { card_id: Uuid, feedback: String },
    /// Fetch and triage PR review comments.
    FetchComments { card_id: Uuid },
    /// Mark every pending review *body* on the card's PR as read — the
    /// one-click dismissal for a body-only review (e.g. a bot's pass report)
    /// the user has read on GitHub and doesn't need an agent to triage.
    MarkReviewBodiesRead { card_id: Uuid },
    /// Apply the (edited, checked) review-comment fixes — the verdicts as the
    /// user left them, edits included: checked bodies go to the fix run,
    /// unchecked replies get posted on GitHub. `note` is the user's own
    /// free-form instruction, applied alongside the checked comments; a run
    /// happens when either is non-empty.
    ApplyFixes {
        card_id: Uuid,
        verdicts: Vec<FixVerdict>,
        note: String,
    },
    /// Create the pull request with the (already confirmed) fields. `branch` is
    /// the final head branch name; if it differs from the card's current branch
    /// the executor renames it before the first push. `draft` opens it as a draft
    /// (so the user can add screenshots then mark it ready) vs. ready-for-review.
    CreatePr {
        card_id: Uuid,
        branch: String,
        title: String,
        body: String,
        reviewer: Option<String>,
        draft: bool,
    },
    /// From "awaiting review", send the implementation back to the agent with
    /// requested changes; the card returns to Implementing in its worktree.
    ReviseImplementation { card_id: Uuid, feedback: String },
    /// Ask the agent a question about its work without sending it back for
    /// changes: a strictly read-only turn from any Agent Chat panel (plan
    /// approval, awaiting review, PR idle, ready-to-merge). The card wraps
    /// into `CardState::Answering` while answering and returns to the exact
    /// state it was asked from; the answer arrives via `AnswerUpdated`.
    AskQuestion { card_id: Uuid, question: String },
    /// From `Concluded`: dig deeper — re-run the investigation with the prior
    /// conclusion, the earlier rounds, and this follow-up as context (the
    /// investigation twin of `RejectPlan`'s re-plan loop).
    FollowUpInvestigation { card_id: Uuid, feedback: String },
    /// From `Concluded`: convert the card, in place, into an implementation —
    /// fold the conclusion into the description under a findings section, flip
    /// the kind to Task, and reset to the starting block so the user shapes the
    /// implementation prompt from there. Cost is kept (the investigation was
    /// real spend on this card); the session is cleared so the next run is fresh.
    ConvertToImplementation { card_id: Uuid },
    /// List the GitHub users who can review a project's PRs. Project-scoped
    /// (not tied to a card); the result comes back as a `Reviewers` event.
    ListReviewers { project_id: Uuid },
    /// List the GitHub logins with an open PR on a project's repo — the
    /// contributor picker's suggestions. Result: a `PrAuthors` event.
    ListPrAuthors { project_id: Uuid },

    // --- branch adoption (turning an existing branch into a card) ---------
    /// List a project's branches that could be adopted into a card (local +
    /// `origin/*`, minus the base, usine-owned refs, and branches already
    /// belonging to cards). Result: an `AdoptSources` event.
    ListAdoptSources { project_id: Uuid },
    /// Inspect one candidate branch for the adopt dialog: commits ahead of the
    /// base (with subjects, for the description prefill), a dirty checkout, an
    /// open PR. Result: an `AdoptProbe` event.
    ProbeAdoptSource {
        project_id: Uuid,
        source_ref: String,
    },
    /// Adopt `source_ref` into a new card: cut the card's own `usine/` branch
    /// at its tip, optionally fold in the checkout's uncommitted changes
    /// (`dirty_action`), optionally delete the original local branch, and drop
    /// the card straight into the self-review pipeline. `description` is
    /// mandatory — it's the task statement every downstream agent run reads.
    AdoptBranch {
        project_id: Uuid,
        source_ref: String,
        title: String,
        description: String,
        retire_original: bool,
        dirty_action: DirtyAction,
    },

    // --- PR review workflow (reviewing other contributors' PRs) ----------
    /// Poll a project for open PRs (by its configured contributors) that the user
    /// hasn't reviewed, creating `ToReview` tasks. Result: `ReviewTasksUpdated`.
    ScanReviews { project_id: Uuid },
    /// Start reviewing a PR: fetch its branch into a worktree and run the review
    /// agent. Moves the task To Review → Reviewing. `guidance` is the user's
    /// free-form steering for this pass ("focus on the migration"), empty for an
    /// unsteered review; it is stored on the task so a retry keeps it.
    StartReview { review_id: Uuid, guidance: String },
    /// Publish the (edited, checked) drafted comments as a GitHub review, then move
    /// the task to Reviewed.
    PublishReview {
        review_id: Uuid,
        drafts: Vec<DraftComment>,
        event: ReviewEvent,
        body: String,
    },
    /// Drop a review task (e.g. a stale one, or one reviewed elsewhere), cancelling
    /// any run and tearing down its worktree.
    DismissReview { review_id: Uuid },
    /// Compute the diff of a PR under review, against the branch it targets. The
    /// PR head is fetched first if it isn't local yet, so the diff is readable
    /// before committing to a review pass. Result: a `DiffUpdated` event keyed by
    /// the *review* id.
    ComputeReviewDiff { review_id: Uuid },
    /// Open a PR-under-review's checkout in a terminal or editor, materializing
    /// the checkout first if it doesn't exist.
    OpenReviewWorktree { review_id: Uuid, target: OpenTarget },
    /// Run the project's app from a PR-under-review's checkout, so a UI change can
    /// be reviewed by using it. Checks out the PR on demand.
    StartReviewPreview { review_id: Uuid },
    /// Stop a PR-under-review's preview (kill the process tree, run teardown).
    StopReviewPreview { review_id: Uuid },
    /// Merge the pull request. `delete_branch` also deletes the head branch on the
    /// forge after a successful merge. `force` skips the CI-checks pre-check (the
    /// user's explicit "merge anyway" for flaky or still-running checks); the
    /// conflict handling downstream is never skipped.
    Merge {
        card_id: Uuid,
        delete_branch: bool,
        force: bool,
    },
    /// From `ReadyToMerge`, after a merge failed on conflicts: merge the base
    /// branch into the card's branch inside its worktree and hand the conflicts
    /// to an agent. Loops back through applying fixes to `ReadyToMerge`.
    ResolveConflicts { card_id: Uuid },
    /// From `ReadyToMerge`, after the merge gate found the PR's CI checks red:
    /// hand the failing checks (with their run logs) to an agent in the card's
    /// worktree. Loops back through applying fixes to `ReadyToMerge`; the push
    /// re-triggers CI.
    FixChecks { card_id: Uuid },
    /// From `AwaitingReview(ReadyForReview)`: skip the self-review pass entirely.
    SkipReview { card_id: Uuid },
    /// From `AwaitingReview(ReadyForReview)`: run the self-review agent over the
    /// committed diff (using the project's `review.md` or a default prompt).
    SelfReview { card_id: Uuid },
    /// Apply the (edited, checked) self-review fixes — the verdicts as the user
    /// left them, edits included; the checked bodies are what the fix run
    /// receives. `note` is the user's own free-form instruction, applied
    /// alongside the checked findings; a run happens when either is non-empty.
    ApplySelfFixes {
        card_id: Uuid,
        verdicts: Vec<FixVerdict>,
        note: String,
    },
    /// From the self-review fix picker: skip applying fixes and open the PR.
    SkipToPr { card_id: Uuid },
    /// Run the project's validate command in the card's worktree (the pre-PR
    /// validation gate). Dual-use: sent internally whenever a card reaches
    /// "ready for PR" with a validate command configured (and to continue the
    /// fix loop), and by the user's "Run validation again" from the parked
    /// failure. A no-op when the card isn't at the gate.
    RunValidation { card_id: Uuid },
    /// Launch the agent run that fixes a validation failure. Dual-use: sent
    /// internally when a check fails inside the attempt budget, and by the
    /// user's "Send to agent again" from the parked failure.
    FixValidation { card_id: Uuid },
    /// From the parked validation failure: give up on the gate and open the PR
    /// form anyway.
    SkipValidation { card_id: Uuid },
    /// Fetch the submitted reviews on the card's PR (who reviewed + verdict).
    RefreshReviews { card_id: Uuid },
    /// Flip the card's draft PR to ready-for-review.
    MarkPrReady { card_id: Uuid },
    /// Run the project's app straight from the card's worktree for testing: run
    /// the setup script (deps, isolated DB, per-worktree ports), then launch the
    /// project's `run_script` as a persistent process, surfacing
    /// the clickable preview URLs. No effect on the card's lifecycle state.
    StartPreview { card_id: Uuid },
    /// Stop the card's running preview: kill the process tree, then run the
    /// project's teardown script (e.g. `docker compose down -v`).
    StopPreview { card_id: Uuid },
    /// Restart the running preview in place (pick up code changes): kill the app
    /// process and relaunch `run_script`, keeping the worktree's DB/infra up.
    RelaunchPreview { card_id: Uuid },
    /// Internal: sent by `launch` alongside every write run so the card's preview
    /// comes up with the agent (setup can take minutes, so it must not block the
    /// launching command). Unlike `StartPreview` it tolerates every "nothing to
    /// do" case — preview already up, no run script, no worktree — silently.
    EnsurePreview { card_id: Uuid },
    /// Open the card's working directory in a terminal or editor. Opens the
    /// card's isolated worktree when it has one, else the project's main
    /// checkout. Purely a convenience — no effect on the card's lifecycle state.
    OpenWorktree { card_id: Uuid, target: OpenTarget },
    /// Compute the card's committed diff over its fork point (for the in-app diff
    /// viewer). Read-only; no effect on the card's lifecycle state. The result
    /// comes back as a `DiffUpdated` event.
    ComputeDiff { card_id: Uuid },
    /// From `ReadyToMerge`: send another change to the agent (a reviewer follow-up
    /// or an unsatisfying fix) and loop back through applying fixes.
    RequestPostPrChange { card_id: Uuid, feedback: String },
    /// Internal: emitted when a PR-comment fix run finishes so the executor can
    /// mark the fixed comments' review threads resolved on GitHub (best-effort).
    /// The fixed comment ids are stashed on the card's review record.
    ResolveFixedComments { card_id: Uuid },
    /// Cancel the active run.
    Cancel { card_id: Uuid },
    /// Send the card back to the starting block (a "do-over"): cancel any run,
    /// clear its execution artifacts, and fold any clarifying Q&A / change
    /// requests into the prompt so the re-run keeps that context.
    BackToStart { card_id: Uuid },
    /// Mark the card done (terminal) from wherever it is, cancelling any run.
    MarkDone { card_id: Uuid },
    /// Retry after a failure.
    Retry { card_id: Uuid },
    /// Internal: a `--resume` run failed; relaunch the phase fresh (no resume).
    RetryFresh { card_id: Uuid },

    // --- direct CRUD (persisted off the UI thread) -----------------------
    /// Persist a newly created card.
    CreateCard { card: Box<Card> },
    /// Persist edits to an existing card (title / description / config).
    SaveCard { card: Box<Card> },
    /// Delete a card and its associated transcript/plan/options.
    DeleteCard { card_id: Uuid },
    /// Persist a newly added project.
    AddProject { project: Box<Project> },
    /// Persist edits to an existing project's config (e.g. review contributors).
    SaveProject { project: Box<Project> },
    /// Delete a project and (cascading) its cards.
    DeleteProject { project_id: Uuid },
    /// Persist global settings.
    SaveSettings { settings: Box<AppSettings> },
    /// Set a card's "skip plan" option.
    SetSkipPlan { card_id: Uuid, skip: bool },
    /// Set whether the card auto-starts its self-review pass when the
    /// implementation finishes (on by default).
    SetAutoReview { card_id: Uuid, auto: bool },
    /// Copy an image into the card's managed attachments dir (`src` = the file
    /// the user picked). Claude-only; the path is injected into the prompt.
    AttachImage { card_id: Uuid, src: PathBuf },
    /// Write pasted image bytes (already PNG-encoded) into the card's managed
    /// attachments dir. Same as `AttachImage` but sourced from the clipboard.
    AttachImageBytes { card_id: Uuid, data: Vec<u8> },
    /// Remove a previously attached image (by its managed path).
    DetachImage { card_id: Uuid, path: PathBuf },
    /// Refresh the usage bar's rate-limit data now (its manual refresh button)
    /// instead of waiting for the next background poll.
    RefreshUsage,
}

impl ExecutorCommand {
    /// The entity this command acts on: a card id for card commands, a
    /// review-task id for the PR-review ones, and nil for project-scoped and
    /// global commands.
    ///
    /// It keys the in-flight gate, the busy signal, and the error toast, all of
    /// which are per-entity rather than per-card — the UI holds cards and review
    /// tasks in the same id-keyed maps, so a review id flows through unchanged.
    pub fn target_id(&self) -> Uuid {
        match self {
            ExecutorCommand::Start { card_id }
            | ExecutorCommand::Answer { card_id, .. }
            | ExecutorCommand::ApprovePlan { card_id }
            | ExecutorCommand::RejectPlan { card_id, .. }
            | ExecutorCommand::FetchComments { card_id }
            | ExecutorCommand::MarkReviewBodiesRead { card_id }
            | ExecutorCommand::ApplyFixes { card_id, .. }
            | ExecutorCommand::CreatePr { card_id, .. }
            | ExecutorCommand::ReviseImplementation { card_id, .. }
            | ExecutorCommand::AskQuestion { card_id, .. }
            | ExecutorCommand::FollowUpInvestigation { card_id, .. }
            | ExecutorCommand::ConvertToImplementation { card_id }
            | ExecutorCommand::Merge { card_id, .. }
            | ExecutorCommand::ResolveConflicts { card_id }
            | ExecutorCommand::FixChecks { card_id }
            | ExecutorCommand::SkipReview { card_id }
            | ExecutorCommand::SelfReview { card_id }
            | ExecutorCommand::ApplySelfFixes { card_id, .. }
            | ExecutorCommand::SkipToPr { card_id }
            | ExecutorCommand::RunValidation { card_id }
            | ExecutorCommand::FixValidation { card_id }
            | ExecutorCommand::SkipValidation { card_id }
            | ExecutorCommand::RefreshReviews { card_id }
            | ExecutorCommand::MarkPrReady { card_id }
            | ExecutorCommand::RequestPostPrChange { card_id, .. }
            | ExecutorCommand::ResolveFixedComments { card_id }
            | ExecutorCommand::StartPreview { card_id }
            | ExecutorCommand::StopPreview { card_id }
            | ExecutorCommand::RelaunchPreview { card_id }
            | ExecutorCommand::EnsurePreview { card_id }
            | ExecutorCommand::OpenWorktree { card_id, .. }
            | ExecutorCommand::ComputeDiff { card_id }
            | ExecutorCommand::Cancel { card_id }
            | ExecutorCommand::BackToStart { card_id }
            | ExecutorCommand::MarkDone { card_id }
            | ExecutorCommand::Retry { card_id }
            | ExecutorCommand::RetryFresh { card_id }
            | ExecutorCommand::DeleteCard { card_id }
            | ExecutorCommand::SetSkipPlan { card_id, .. }
            | ExecutorCommand::SetAutoReview { card_id, .. }
            | ExecutorCommand::AttachImage { card_id, .. }
            | ExecutorCommand::AttachImageBytes { card_id, .. }
            | ExecutorCommand::DetachImage { card_id, .. } => *card_id,
            ExecutorCommand::CreateCard { card } | ExecutorCommand::SaveCard { card } => card.id,
            // PR-review workflow: keyed by the review task, which is a separate
            // entity from any card but flows through the same per-id plumbing.
            ExecutorCommand::StartReview { review_id, .. }
            | ExecutorCommand::PublishReview { review_id, .. }
            | ExecutorCommand::DismissReview { review_id }
            | ExecutorCommand::ComputeReviewDiff { review_id }
            | ExecutorCommand::OpenReviewWorktree { review_id, .. }
            | ExecutorCommand::StartReviewPreview { review_id }
            | ExecutorCommand::StopReviewPreview { review_id } => *review_id,
            // Project-scoped / global — no single entity. `AdoptBranch`
            // creates its card mid-handler and claims it there.
            ExecutorCommand::ListReviewers { .. }
            | ExecutorCommand::ListPrAuthors { .. }
            | ExecutorCommand::ListAdoptSources { .. }
            | ExecutorCommand::ProbeAdoptSource { .. }
            | ExecutorCommand::AdoptBranch { .. }
            | ExecutorCommand::AddProject { .. }
            | ExecutorCommand::SaveProject { .. }
            | ExecutorCommand::DeleteProject { .. }
            | ExecutorCommand::SaveSettings { .. }
            | ExecutorCommand::ScanReviews { .. }
            | ExecutorCommand::RefreshUsage => Uuid::nil(),
        }
    }

    /// Fast, ordered local commands the dispatcher runs inline (DB writes plus
    /// small file copies for attachments) so a dependent later command — e.g. a
    /// `Start` right after `CreateCard`, or a run that reads a just-attached
    /// image — sees the result. Everything else is an agent run or a git/forge
    /// effect that the dispatcher spawns so one slow call can't block the rest.
    ///
    /// Deletes are deliberately *not* here: they now reap the target's runtime
    /// (cancel runs, kill previews, remove worktrees) before the DB delete, which
    /// can block on a teardown script or a stubborn worktree — work that must be
    /// spawned. Nothing depends on a delete having completed, so ordering is moot.
    /// Commands that advance a card's lifecycle: they read the card's state,
    /// do slow git/forge work, then apply a transition. Two of them in flight
    /// for the same card interleave — both pass the state guard against the
    /// *old* state, both perform their side effects, and the loser fails at the
    /// transition with a confusing "illegal transition" toast. The dispatcher
    /// therefore runs at most one per card (see `InFlight`).
    ///
    /// Deliberately excluded are the commands that must never be dropped even
    /// while one of the above is running: `Cancel` (interrupts it), the delete /
    /// preview / open / diff commands (no lifecycle transition, and each is
    /// idempotent or carries its own generation check), and the project-scoped
    /// commands (not keyed by an entity at all).
    ///
    /// `DismissReview` is excluded for the same reason as `Cancel`: it is the way
    /// out of a review that is running or stuck, so it must land even while
    /// `StartReview` holds the slot.
    pub fn is_exclusive(&self) -> bool {
        matches!(
            self,
            ExecutorCommand::Start { .. }
                | ExecutorCommand::StartReview { .. }
                | ExecutorCommand::PublishReview { .. }
                | ExecutorCommand::Answer { .. }
                | ExecutorCommand::ApprovePlan { .. }
                | ExecutorCommand::RejectPlan { .. }
                | ExecutorCommand::FetchComments { .. }
                | ExecutorCommand::ApplyFixes { .. }
                | ExecutorCommand::CreatePr { .. }
                | ExecutorCommand::ReviseImplementation { .. }
                | ExecutorCommand::AskQuestion { .. }
                | ExecutorCommand::FollowUpInvestigation { .. }
                | ExecutorCommand::ConvertToImplementation { .. }
                | ExecutorCommand::Merge { .. }
                | ExecutorCommand::ResolveConflicts { .. }
                | ExecutorCommand::FixChecks { .. }
                | ExecutorCommand::SkipReview { .. }
                | ExecutorCommand::SelfReview { .. }
                | ExecutorCommand::ApplySelfFixes { .. }
                | ExecutorCommand::SkipToPr { .. }
                | ExecutorCommand::RunValidation { .. }
                | ExecutorCommand::FixValidation { .. }
                | ExecutorCommand::SkipValidation { .. }
                | ExecutorCommand::MarkPrReady { .. }
                | ExecutorCommand::RequestPostPrChange { .. }
                | ExecutorCommand::BackToStart { .. }
                | ExecutorCommand::MarkDone { .. }
                | ExecutorCommand::Retry { .. }
                | ExecutorCommand::RetryFresh { .. }
        )
    }

    pub fn is_persistence(&self) -> bool {
        matches!(
            self,
            ExecutorCommand::CreateCard { .. }
                | ExecutorCommand::SaveCard { .. }
                | ExecutorCommand::MarkReviewBodiesRead { .. }
                | ExecutorCommand::AddProject { .. }
                | ExecutorCommand::SaveProject { .. }
                | ExecutorCommand::SaveSettings { .. }
                | ExecutorCommand::SetSkipPlan { .. }
                | ExecutorCommand::SetAutoReview { .. }
                | ExecutorCommand::AttachImage { .. }
                | ExecutorCommand::AttachImageBytes { .. }
                | ExecutorCommand::DetachImage { .. }
        )
    }
}

/// One entry waiting in the run queue, as shown to the UI: the card (any card
/// run or validation check) or contributor-PR review task holding the place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedTarget {
    Card(Uuid),
    Review(Uuid),
}

impl QueuedTarget {
    /// The slot-holder id (card id or review-task id).
    pub fn id(&self) -> Uuid {
        match self {
            QueuedTarget::Card(id) | QueuedTarget::Review(id) => *id,
        }
    }
}

/// An event from the executor to the UI.
#[derive(Debug, Clone)]
pub struct ExecutorEvent {
    pub card_id: Uuid,
    pub kind: ExecutorEventKind,
}

#[derive(Debug, Clone)]
pub enum ExecutorEventKind {
    /// The card changed (or was just created); the UI upserts its copy.
    CardUpdated(Box<Card>),
    /// The card was deleted; the UI drops it (`card_id` on the event).
    CardRemoved,
    /// A project was added or changed; the UI upserts it.
    ProjectUpserted(Box<Project>),
    /// A project was deleted; the UI drops it and its cards.
    ProjectRemoved { project_id: Uuid },
    /// Global settings changed; the UI replaces its copy.
    SettingsUpdated(Box<AppSettings>),
    /// A card's skip-plan flag changed (`card_id` on the event).
    SkipPlanChanged { skip: bool },
    /// A card's auto-review flag changed (`card_id` on the event).
    AutoReviewChanged { auto: bool },
    /// A lifecycle-advancing command started (`busy: true`) or finished
    /// (`busy: false`) for the card. Those commands do their git/forge work
    /// *before* transitioning, so between the click and the resulting
    /// `CardUpdated` the card looks idle while it isn't. The UI uses this to
    /// show progress and disable the actions the executor would drop anyway
    /// (see `ExecutorCommand::is_exclusive`) — same source of truth, so what the UI
    /// blocks and what the dispatcher drops can't drift apart.
    CardBusy { busy: bool },
    /// A card's attachment list changed (`card_id` on the event).
    AttachmentsChanged { paths: Vec<PathBuf> },
    /// Append a line to the card's live transcript (`ts` = unix millis).
    Transcript { ts: i64, line: String },
    /// The GitHub logins that can review a project's PRs (project-scoped).
    Reviewers {
        project_id: Uuid,
        logins: Vec<String>,
    },
    /// The GitHub logins with an open PR on a project's repo (project-scoped).
    PrAuthors {
        project_id: Uuid,
        logins: Vec<String>,
    },
    /// A project's adoptable branches (project-scoped; the adopt dialog's
    /// picker replaces its list).
    AdoptSources { project_id: Uuid, refs: Vec<String> },
    /// What probing one adopt candidate found (project-scoped; the dialog
    /// matches `probe.source_ref` against its current pick to drop stale
    /// responses).
    AdoptProbe { project_id: Uuid, probe: AdoptProbe },
    /// A project's full set of PR-review tasks (after a scan or a removal). The UI
    /// replaces its per-project list.
    ReviewTasksUpdated {
        project_id: Uuid,
        tasks: Vec<ReviewTask>,
    },
    /// A single PR-review task changed; the UI upserts it into its project list.
    ReviewTaskUpdated(Box<ReviewTask>),
    /// A card's fixes recap changed (`card_id` on the event).
    RecapUpdated { recap: String },
    /// A card's Agent Chat exchange changed (`card_id` on the event). An empty
    /// `answer` means it was cleared (e.g. "back to start", or a write run
    /// superseding it) and the UI drops its entry.
    AnswerUpdated { question: String, answer: String },
    /// A card's implementation hand-off changed (`card_id` on the event). An
    /// empty [`Handoff`] means the latest implement run produced none, and the UI
    /// drops the previous attempt's.
    HandoffUpdated { handoff: Handoff },
    /// A card's in-worktree preview changed status (`card_id` on the event). The
    /// UI mirrors `status` + `urls` into its per-card preview signal.
    PreviewUpdated {
        status: PreviewStatus,
        urls: Vec<PreviewUrl>,
    },
    /// A card's computed diff changed (`card_id` on the event). The UI replaces
    /// its per-card diff signal wholesale (computing → ready/empty/failed).
    DiffUpdated { state: crate::diff::DiffState },
    /// The card's merge was refused because its PR conflicts with `base`
    /// (`card_id` on the event). The card is left in `ReadyToMerge`; the UI asks
    /// whether an agent should resolve the conflicts (`ResolveConflicts`).
    MergeConflict { pr_number: u64, base: String },
    /// The card's merge was refused because its PR's CI checks are failing
    /// (`card_id` on the event). `failed` names the failing checks. The card is
    /// left in `ReadyToMerge`; the UI asks whether an agent should fix the
    /// checks (`FixChecks`).
    ChecksFailed { pr_number: u64, failed: Vec<String> },
    /// The providers' account-level rate-limit usage changed (session/weekly
    /// windows for the usage bar); the UI replaces its snapshot wholesale.
    /// Not card-scoped.
    UsageUpdated(UsageSnapshot),
    /// The run queue changed (an entry queued, launched, or was purged). The
    /// full ordered queue — index = place in line; the UI replaces its copy
    /// wholesale. Not card-scoped. In-memory only: the queue doesn't survive a
    /// restart (interrupted-run recovery picks the queued cards up as `Failed`).
    RunQueueChanged { entries: Vec<QueuedTarget> },
    /// Show a toast (often an error).
    Toast { severity: Severity, message: String },
}

impl ExecutorEvent {
    pub fn updated(card: Card) -> Self {
        ExecutorEvent {
            card_id: card.id,
            kind: ExecutorEventKind::CardUpdated(Box::new(card)),
        }
    }
    pub fn card_removed(card_id: Uuid) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::CardRemoved,
        }
    }
    pub fn project_upserted(project: Project) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::ProjectUpserted(Box::new(project)),
        }
    }
    pub fn project_removed(project_id: Uuid) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::ProjectRemoved { project_id },
        }
    }
    pub fn settings_updated(settings: AppSettings) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::SettingsUpdated(Box::new(settings)),
        }
    }
    pub fn skip_plan_changed(card_id: Uuid, skip: bool) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::SkipPlanChanged { skip },
        }
    }
    pub fn auto_review_changed(card_id: Uuid, auto: bool) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::AutoReviewChanged { auto },
        }
    }
    pub fn attachments_changed(card_id: Uuid, paths: Vec<PathBuf>) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::AttachmentsChanged { paths },
        }
    }
    pub fn run_queue_changed(entries: Vec<QueuedTarget>) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::RunQueueChanged { entries },
        }
    }
    pub fn card_busy(card_id: Uuid, busy: bool) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::CardBusy { busy },
        }
    }
    pub fn transcript(card_id: Uuid, ts: i64, line: impl Into<String>) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::Transcript {
                ts,
                line: line.into(),
            },
        }
    }
    pub fn reviewers(project_id: Uuid, logins: Vec<String>) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::Reviewers { project_id, logins },
        }
    }
    pub fn pr_authors(project_id: Uuid, logins: Vec<String>) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::PrAuthors { project_id, logins },
        }
    }
    pub fn adopt_sources(project_id: Uuid, refs: Vec<String>) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::AdoptSources { project_id, refs },
        }
    }
    pub fn adopt_probe(project_id: Uuid, probe: AdoptProbe) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::AdoptProbe { project_id, probe },
        }
    }
    pub fn review_tasks_updated(project_id: Uuid, tasks: Vec<ReviewTask>) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::ReviewTasksUpdated { project_id, tasks },
        }
    }
    pub fn review_task_updated(task: ReviewTask) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::ReviewTaskUpdated(Box::new(task)),
        }
    }
    pub fn recap_updated(card_id: Uuid, recap: impl Into<String>) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::RecapUpdated {
                recap: recap.into(),
            },
        }
    }
    pub fn answer_updated(
        card_id: Uuid,
        question: impl Into<String>,
        answer: impl Into<String>,
    ) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::AnswerUpdated {
                question: question.into(),
                answer: answer.into(),
            },
        }
    }
    pub fn handoff_updated(card_id: Uuid, handoff: Handoff) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::HandoffUpdated { handoff },
        }
    }
    pub fn toast(card_id: Uuid, severity: Severity, message: impl Into<String>) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::Toast {
                severity,
                message: message.into(),
            },
        }
    }
    pub fn preview_updated(card_id: Uuid, status: PreviewStatus, urls: Vec<PreviewUrl>) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::PreviewUpdated { status, urls },
        }
    }
    pub fn diff_updated(card_id: Uuid, state: crate::diff::DiffState) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::DiffUpdated { state },
        }
    }
    pub fn usage_updated(snapshot: UsageSnapshot) -> Self {
        ExecutorEvent {
            card_id: Uuid::nil(),
            kind: ExecutorEventKind::UsageUpdated(snapshot),
        }
    }
    pub fn merge_conflict(card_id: Uuid, pr_number: u64, base: impl Into<String>) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::MergeConflict {
                pr_number,
                base: base.into(),
            },
        }
    }
    pub fn checks_failed(card_id: Uuid, pr_number: u64, failed: Vec<String>) -> Self {
        ExecutorEvent {
            card_id,
            kind: ExecutorEventKind::ChecksFailed { pr_number, failed },
        }
    }
}
