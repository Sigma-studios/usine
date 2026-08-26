//! `AppState`: the reactive bridge between the core executor and the Dioxus UI.
//!
//! It is `Copy` (every field is a `Signal`, including the non-`Copy` backend and
//! the event receiver, which live inside signals) so it can be captured freely
//! in event handlers. A single reducer ([`AppState::apply_event`]) is the one
//! place executor events mutate signals, keeping re-renders predictable.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use dioxus::prelude::*;
use futures::channel::mpsc::UnboundedReceiver;
use usine_core::{
    spawn_executor, AdoptProbe, AppSettings, Card, CardState, DesignSub, DiffState, DirtyAction,
    ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, ExecutorHandle, Forge,
    GhForge, GitOps, Handoff, PrInfo, PreviewStatus, PreviewUrl, Project, Provider,
    ProviderFactory, RealFactory, RealGit, ReviewSub, ReviewTask, RunSub, Severity, SimFactory,
    SimForge, SimGit, Store, UsageSnapshot,
};
use uuid::Uuid;

use crate::toast::push_toast;

/// Message stamped on runs interrupted by an app restart. The "Interrupted"
/// prefix is what the UI keys on to label the recovery button "Resume".
pub(crate) const INTERRUPTED_MSG: &str =
    "Interrupted — the app closed while this run was active. Resume to continue.";

/// Which view is selected in the sidebar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelectedView {
    Global,
    Project(Uuid),
}

/// Whether the board shows the normal card lanes or the PR-review lanes. Review
/// mode is per-project and entered from the sidebar's review icon.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BoardMode {
    Normal,
    Review,
}

/// The non-`Copy` executor handle, parked inside a `Signal` so `AppState` stays
/// `Copy`. The UI never holds the `Store` directly — all reads are seeded into
/// signals at startup and all writes go through the executor.
#[derive(Clone)]
struct Backend {
    exec: ExecutorHandle,
}

#[derive(Clone, Copy)]
pub struct AppState {
    backend: Signal<Backend>,
    pub projects: Signal<Vec<Project>>,
    pub cards: Signal<Vec<Card>>,
    pub settings: Signal<AppSettings>,
    pub selected_view: Signal<SelectedView>,
    /// Normal card board vs. the PR-review board (per selected project).
    pub board_mode: Signal<BoardMode>,
    pub selected_card: Signal<Option<Uuid>>,
    /// The review task open in the detail panel while the review board is up —
    /// the review-mode counterpart of `selected_card`.
    pub selected_review: Signal<Option<Uuid>>,
    pub transcripts: Signal<HashMap<Uuid, Vec<(i64, String)>>>,
    /// Per-project reviewer candidates (GitHub logins), fetched lazily on demand.
    pub reviewers: Signal<HashMap<Uuid, Vec<String>>>,
    /// Per-project adoptable branches (local + remote, minus base/usine/card
    /// refs), fetched when the adopt dialog opens. In-memory only.
    pub adopt_sources: Signal<HashMap<Uuid, Vec<String>>>,
    /// The latest adopt-probe result per project. The dialog matches the
    /// probe's `source_ref` against its current pick, so a stale response for a
    /// previously picked branch is ignored. In-memory only.
    pub adopt_probes: Signal<HashMap<Uuid, AdoptProbe>>,
    /// Per-project PR-review tasks (other contributors' PRs), seeded at startup
    /// and kept in sync via `ReviewTasksUpdated`/`ReviewTaskUpdated` events.
    pub review_tasks: Signal<HashMap<Uuid, Vec<ReviewTask>>>,
    /// Per-card "skip plan" flags, seeded from the store at startup and kept in
    /// sync via `SkipPlanChanged` events — so the UI never reads the store.
    pub skip_plans: Signal<HashMap<Uuid, bool>>,
    /// Per-card auto-review flags (true = the self-review auto-starts when the
    /// implementation finishes; on by default), seeded from the store at startup
    /// and kept in sync via `AutoReviewChanged` events.
    pub auto_reviews: Signal<HashMap<Uuid, bool>>,
    /// Per-card attached image paths (Claude-only), seeded at startup and kept in
    /// sync via `AttachmentsChanged` events.
    pub attachments: Signal<HashMap<Uuid, Vec<PathBuf>>>,
    /// Per-card fixes recap, seeded at startup and updated via `RecapUpdated`.
    pub review_recaps: Signal<HashMap<Uuid, String>>,
    /// Per-card Agent Chat exchange — the last question asked and its prose
    /// answer — seeded at startup and updated via `AnswerUpdated` (an empty
    /// answer removes the entry).
    pub answers: Signal<HashMap<Uuid, (String, String)>>,
    /// Per-card implementation hand-off — the recap, open questions, and testing
    /// checklist the implement run left for its reviewer. Seeded at startup and
    /// updated via `HandoffUpdated`.
    pub handoffs: Signal<HashMap<Uuid, Handoff>>,
    /// Per-card in-worktree preview state (status + clickable URLs), kept in sync
    /// via `PreviewUpdated` events. In-memory only — previews don't survive a
    /// restart (their processes don't either), so this starts empty each run.
    pub previews: Signal<HashMap<Uuid, (PreviewStatus, Vec<PreviewUrl>)>>,
    /// Per-card computed diff (the card's committed contribution), requested on
    /// demand via `ComputeDiff` and replaced wholesale via `DiffUpdated` events.
    /// In-memory only — recomputed each time the user opens the diff.
    pub diffs: Signal<HashMap<Uuid, DiffState>>,
    /// Cards with a lifecycle-advancing command in flight, kept in sync via
    /// `CardBusy` events. Those commands do their git/forge work before
    /// transitioning, so the card's own state can't show that anything is
    /// happening — this is what drives the spinner and the disabled actions in
    /// that window. In-memory only: nothing is in flight at startup.
    pub busy: Signal<HashSet<Uuid>>,
    /// The providers' account-level rate-limit usage (session/weekly windows),
    /// refreshed by the executor's background poll and rendered by the bottom
    /// usage bar. In-memory only — it's refetched shortly after launch.
    pub usage: Signal<UsageSnapshot>,
    event_rx: Signal<Option<UnboundedReceiver<ExecutorEvent>>>,
}

impl AppState {
    /// Open the store, load data, spawn the executor, seed a demo project on
    /// first run. Runs once, inside the root `use_context_provider`.
    pub fn init() -> Self {
        let store = open_store();

        // First startup only (no settings record yet, real mode): default the
        // provider to whichever agent CLI is actually installed — Codex when it
        // is the sole one present, Claude otherwise. Persisted as soon as a CLI
        // is found, so it never overrides a later user choice; with none found
        // the Claude fallback stays unpersisted and detection re-runs next
        // launch. Demo mode uses simulated backends and must stay
        // deterministic.
        if !demo_mode() {
            let claude = usine_core::binary_on_path(Provider::Claude.binary());
            let codex = usine_core::binary_on_path(Provider::Codex.binary());
            match usine_core::seed_default_provider(&store, |p| match p {
                Provider::Claude => claude,
                Provider::Codex => codex,
            }) {
                Ok(Some(Provider::Codex)) => {
                    push_toast(Severity::Info, "Claude CLI not found — defaulting to Codex")
                }
                Ok(Some(Provider::Claude)) if !claude && !codex => push_toast(
                    Severity::Warning,
                    "No agent CLI found on PATH — install claude or codex",
                ),
                _ => {}
            }
        }

        let settings = store.settings().unwrap_or_default();

        // Seed the sample board only in demo mode; real mode starts empty.
        if demo_mode() && store.list_projects().map(|p| p.is_empty()).unwrap_or(false) {
            if let Err(e) = seed_demo(&store, &settings) {
                push_toast(Severity::Warning, format!("could not seed demo data: {e}"));
            }
        }

        // Startup reconciliation lives in core (testable; the view stays thin):
        // sync each project's base branch with its repo, and mark any run left
        // "running" by a crash as interrupted so the user can resume it. Demo
        // mode keeps its intentional fakes (running cards, default branches).
        if !demo_mode() {
            let _ = usine_core::sync_base_branches(&store);
            let _ = usine_core::reconcile_interrupted_runs(&store, INTERRUPTED_MSG);
            let _ = usine_core::reconcile_interrupted_reviews(&store, INTERRUPTED_MSG);
            // Detached, unlike the reconciliations above: this one shells out to
            // Docker once per stranded stack, so a slow — or wedged — daemon
            // would otherwise hold the board off the screen for as long as it
            // takes. Nothing on screen depends on the result, and the stacks it
            // reclaims have already been stranded for however long, so finishing
            // a few seconds after launch is soon enough.
            std::thread::spawn(usine_core::reconcile_orphaned_worktree_stacks);
        }

        // Rewrite stored records into canonical form so ones written before newer
        // `#[serde(default)]` fields (or the Cost change) round-trip — otherwise
        // native_db rejects the next edit/delete of them with IncorrectInputData.
        store.canonicalize_records();

        let projects = store.list_projects().unwrap_or_default();
        let cards = store.list_cards().unwrap_or_default();
        let skip_plans = store.skip_plan_flags().unwrap_or_default();
        let auto_reviews = store.auto_review_flags().unwrap_or_default();
        let attachments = store.all_attachments().unwrap_or_default();
        let review_recaps = store.all_review_recaps().unwrap_or_default();
        let answers = store.all_answers().unwrap_or_default();
        let handoffs = store.all_handoffs().unwrap_or_default();
        // Group persisted review tasks by project for the review board.
        let mut review_tasks: HashMap<Uuid, Vec<ReviewTask>> = HashMap::new();
        for t in store.list_review_tasks().unwrap_or_default() {
            review_tasks.entry(t.project_id).or_default().push(t);
        }

        let (providers, forge, git) = backends();
        // The executor takes the store; the UI keeps no handle of its own.
        let (exec, rx) = spawn_executor(ExecutorConfig {
            store,
            providers,
            forge,
            git,
        });
        if demo_mode() {
            push_toast(Severity::Info, "Demo mode — agents & GitHub are simulated");
        }

        AppState {
            backend: Signal::new(Backend { exec }),
            projects: Signal::new(projects),
            cards: Signal::new(cards),
            settings: Signal::new(settings),
            selected_view: Signal::new(SelectedView::Global),
            board_mode: Signal::new(BoardMode::Normal),
            selected_card: Signal::new(None),
            selected_review: Signal::new(None),
            transcripts: Signal::new(HashMap::new()),
            reviewers: Signal::new(HashMap::new()),
            adopt_sources: Signal::new(HashMap::new()),
            adopt_probes: Signal::new(HashMap::new()),
            review_tasks: Signal::new(review_tasks),
            skip_plans: Signal::new(skip_plans),
            auto_reviews: Signal::new(auto_reviews),
            attachments: Signal::new(attachments),
            review_recaps: Signal::new(review_recaps),
            answers: Signal::new(answers),
            handoffs: Signal::new(handoffs),
            previews: Signal::new(HashMap::new()),
            diffs: Signal::new(HashMap::new()),
            busy: Signal::new(HashSet::new()),
            usage: Signal::new(UsageSnapshot::default()),
            event_rx: Signal::new(Some(rx)),
        }
    }

    pub fn send(&self, cmd: ExecutorCommand) {
        self.backend.read().exec.send(cmd);
    }

    /// An owned clone of the executor handle, for callers that must run *without*
    /// touching signals — notably the window-close cleanup hook, which fires as
    /// the Dioxus runtime tears down (reading a signal then could panic).
    pub fn executor_handle(&self) -> ExecutorHandle {
        self.backend.read().exec.clone()
    }

    pub fn select_card(&self, id: Option<Uuid>) {
        let mut sc = self.selected_card;
        sc.set(id);
    }

    pub fn select_review(&self, id: Option<Uuid>) {
        let mut sr = self.selected_review;
        sr.set(id);
    }

    pub fn select_view(&self, view: SelectedView) {
        let mut sv = self.selected_view;
        sv.set(view);
        // Navigating always returns to the normal card board; review mode is
        // entered explicitly via the sidebar review icon.
        let mut bm = self.board_mode;
        bm.set(BoardMode::Normal);
        self.select_review(None);
    }

    /// Take the executor event receiver (once) so the root can drain it.
    pub fn take_event_rx(&self) -> Option<UnboundedReceiver<ExecutorEvent>> {
        let mut rx = self.event_rx;
        let taken = rx.write().take();
        taken
    }

    /// The single place executor events mutate UI signals.
    pub fn apply_event(&self, evt: ExecutorEvent) {
        match evt.kind {
            ExecutorEventKind::CardUpdated(card) => {
                let card = *card;
                let mut cards = self.cards;
                let mut list = cards.write();
                match list.iter_mut().find(|c| c.id == card.id) {
                    Some(existing) => *existing = card,
                    None => list.push(card),
                }
            }
            ExecutorEventKind::CardRemoved => {
                let id = evt.card_id;
                let mut cards = self.cards;
                cards.write().retain(|c| c.id != id);
                let mut transcripts = self.transcripts;
                transcripts.write().remove(&id);
                let mut attachments = self.attachments;
                attachments.write().remove(&id);
                let mut answers = self.answers;
                answers.write().remove(&id);
                crate::ui::drafts::forget_owner(id);
                if *self.selected_card.read() == Some(id) {
                    let mut selected = self.selected_card;
                    selected.set(None);
                }
            }
            ExecutorEventKind::ProjectUpserted(project) => {
                let project = *project;
                let mut projects = self.projects;
                let mut list = projects.write();
                match list.iter_mut().find(|p| p.id == project.id) {
                    Some(existing) => *existing = project,
                    None => list.push(project),
                }
            }
            ExecutorEventKind::ProjectRemoved { project_id } => {
                let mut projects = self.projects;
                projects.write().retain(|p| p.id != project_id);
                let mut cards = self.cards;
                // The removed cards' drafts go with them — collect their ids
                // before the retain drops them.
                let removed: Vec<Uuid> = cards
                    .read()
                    .iter()
                    .filter(|c| c.project_id == project_id)
                    .map(|c| c.id)
                    .collect();
                cards.write().retain(|c| c.project_id != project_id);
                for id in removed {
                    crate::ui::drafts::forget_owner(id);
                }
                // The project's review tasks go too, drafts included.
                let mut rt = self.review_tasks;
                if let Some(tasks) = rt.write().remove(&project_id) {
                    for t in tasks {
                        crate::ui::drafts::forget_owner(t.id);
                    }
                }
                // Copy the id out first (same borrow rule as below).
                let selected_review = *self.selected_review.read();
                if let Some(rid) = selected_review {
                    if self.review_task(rid).is_none() {
                        self.select_review(None);
                    }
                }
                // Fall back to the global view if we were viewing this project.
                if matches!(*self.selected_view.read(), SelectedView::Project(pid) if pid == project_id)
                {
                    let mut view = self.selected_view;
                    view.set(SelectedView::Global);
                }
                // Clear the detail selection if its card went with the project.
                // Copy the id out first: an `if let` scrutinee keeps the read
                // guard alive through the body, which writes the same signal.
                let selected_id = *self.selected_card.read();
                if let Some(cid) = selected_id {
                    if !self.cards.read().iter().any(|c| c.id == cid) {
                        let mut selected = self.selected_card;
                        selected.set(None);
                    }
                }
            }
            ExecutorEventKind::SettingsUpdated(settings) => {
                let mut s = self.settings;
                s.set(*settings);
            }
            ExecutorEventKind::SkipPlanChanged { skip } => {
                let id = evt.card_id;
                let mut skip_plans = self.skip_plans;
                skip_plans.write().insert(id, skip);
            }
            ExecutorEventKind::AutoReviewChanged { auto } => {
                let id = evt.card_id;
                let mut auto_reviews = self.auto_reviews;
                auto_reviews.write().insert(id, auto);
            }
            ExecutorEventKind::CardBusy { busy } => {
                let id = evt.card_id;
                let mut cards_busy = self.busy;
                if busy {
                    cards_busy.write().insert(id);
                } else {
                    cards_busy.write().remove(&id);
                }
            }
            ExecutorEventKind::AttachmentsChanged { paths } => {
                let id = evt.card_id;
                let mut attachments = self.attachments;
                attachments.write().insert(id, paths);
            }
            ExecutorEventKind::Transcript { ts, line } => {
                let mut transcripts = self.transcripts;
                transcripts
                    .write()
                    .entry(evt.card_id)
                    .or_default()
                    .push((ts, line));
            }
            ExecutorEventKind::Reviewers { project_id, logins } => {
                let mut reviewers = self.reviewers;
                reviewers.write().insert(project_id, logins);
            }
            ExecutorEventKind::AdoptSources { project_id, refs } => {
                let mut sources = self.adopt_sources;
                sources.write().insert(project_id, refs);
            }
            ExecutorEventKind::AdoptProbe { project_id, probe } => {
                let mut probes = self.adopt_probes;
                probes.write().insert(project_id, probe);
            }
            ExecutorEventKind::ReviewTasksUpdated { project_id, tasks } => {
                let mut rt = self.review_tasks;
                // Tasks a rescan or dismissal dropped take their drafts
                // (e.g. "review.guidance") with them.
                let dropped: Vec<Uuid> = rt
                    .read()
                    .get(&project_id)
                    .map(|old| {
                        old.iter()
                            .filter(|t| !tasks.iter().any(|n| n.id == t.id))
                            .map(|t| t.id)
                            .collect()
                    })
                    .unwrap_or_default();
                for id in dropped {
                    crate::ui::drafts::forget_owner(id);
                }
                rt.write().insert(project_id, tasks);
                // A rescan or a dismissal can drop the task the panel is showing.
                // Copy the id out first: an `if let` scrutinee keeps the read
                // guard alive through the body, and `select_review` writes the
                // same signal (AlreadyBorrowed panic otherwise).
                let selected = *self.selected_review.read();
                if let Some(id) = selected {
                    if self.review_task(id).is_none() {
                        self.select_review(None);
                    }
                }
            }
            ExecutorEventKind::ReviewTaskUpdated(task) => {
                let task = *task;
                let mut rt = self.review_tasks;
                let mut map = rt.write();
                let list = map.entry(task.project_id).or_default();
                match list.iter_mut().find(|t| t.id == task.id) {
                    Some(existing) => *existing = task,
                    None => list.push(task),
                }
            }
            ExecutorEventKind::RecapUpdated { recap } => {
                let mut recaps = self.review_recaps;
                recaps.write().insert(evt.card_id, recap);
            }
            // An empty answer means the exchange was cleared ("back to start",
            // or a write run superseding it): drop the entry so the panel shows
            // nothing.
            ExecutorEventKind::AnswerUpdated { question, answer } => {
                let mut answers = self.answers;
                if answer.is_empty() {
                    answers.write().remove(&evt.card_id);
                } else {
                    answers.write().insert(evt.card_id, (question, answer));
                }
            }
            // An empty hand-off means the run left none: drop the entry so the
            // panel shows nothing rather than the previous attempt's recap.
            ExecutorEventKind::HandoffUpdated { handoff } => {
                let mut handoffs = self.handoffs;
                if handoff.is_empty() {
                    handoffs.write().remove(&evt.card_id);
                } else {
                    handoffs.write().insert(evt.card_id, handoff);
                }
            }
            ExecutorEventKind::PreviewUpdated { status, urls } => {
                let mut previews = self.previews;
                previews.write().insert(evt.card_id, (status, urls));
            }
            ExecutorEventKind::DiffUpdated { state } => {
                let mut diffs = self.diffs;
                diffs.write().insert(evt.card_id, state);
            }
            // The merge was refused because the PR conflicts with its base. The
            // card is still `ReadyToMerge`, so this is an offer, not an error:
            // declining leaves the user free to resolve the conflicts by hand.
            ExecutorEventKind::MergeConflict { pr_number, base } => {
                let card_id = evt.card_id;
                crate::ui::request_confirm(crate::ui::ConfirmRequest {
                    title: "Merge conflicts".into(),
                    message: format!(
                        "PR #{pr_number} can't be merged — it conflicts with `{base}`.\n\n\
                         Have the agent merge `{base}` into the branch, resolve the conflicts, and \
                         push? You can review the result before merging again."
                    ),
                    confirm_label: "Resolve with AI".into(),
                    danger: false,
                    action: crate::ui::ConfirmAction::Send(ExecutorCommand::ResolveConflicts {
                        card_id,
                    }),
                });
            }
            // The merge was refused because the PR's CI checks are failing. Same
            // shape as the conflict offer: the card is still `ReadyToMerge`, and
            // declining leaves the detail pane's "Merge anyway" available.
            ExecutorEventKind::ChecksFailed { pr_number, failed } => {
                let card_id = evt.card_id;
                let names = if failed.is_empty() {
                    String::new()
                } else {
                    format!("\n\nFailing: {}", failed.join(", "))
                };
                crate::ui::request_confirm(crate::ui::ConfirmRequest {
                    title: "CI checks failing".into(),
                    message: format!(
                        "PR #{pr_number} can't be merged — its CI checks are failing.{names}\n\n\
                         Have the agent investigate the failures, fix them, and push? \
                         The checks re-run on the push."
                    ),
                    confirm_label: "Fix with AI".into(),
                    danger: false,
                    action: crate::ui::ConfirmAction::Send(ExecutorCommand::FixChecks { card_id }),
                });
            }
            ExecutorEventKind::UsageUpdated(snapshot) => {
                let mut usage = self.usage;
                usage.set(snapshot);
            }
            ExecutorEventKind::Toast { severity, message } => push_toast(severity, message),
        }
    }

    /// Ask the executor for a project's reviewer candidates (collaborators). The
    /// result arrives asynchronously as a `Reviewers` event.
    pub fn fetch_reviewers(&self, project_id: Uuid) {
        self.send(ExecutorCommand::ListReviewers { project_id });
    }

    /// Ask the executor for a project's adoptable branches (the adopt dialog's
    /// picker). The result arrives as an `AdoptSources` event.
    pub fn fetch_adopt_sources(&self, project_id: Uuid) {
        self.send(ExecutorCommand::ListAdoptSources { project_id });
    }

    /// Probe one candidate branch for the adopt dialog (commits ahead, dirty
    /// checkout, open PR). The result arrives as an `AdoptProbe` event.
    pub fn probe_adopt_source(&self, project_id: Uuid, source_ref: String) {
        self.send(ExecutorCommand::ProbeAdoptSource {
            project_id,
            source_ref,
        });
    }

    /// Adopt a branch into a new card that starts in the self-review pipeline.
    pub fn adopt_branch(
        &self,
        project_id: Uuid,
        source_ref: String,
        title: String,
        description: String,
        retire_original: bool,
        dirty_action: DirtyAction,
    ) {
        self.send(ExecutorCommand::AdoptBranch {
            project_id,
            source_ref,
            title,
            description,
            retire_original,
            dirty_action,
        });
    }

    /// Refresh the usage bar's rate-limit data now (its refresh button) rather
    /// than waiting for the next background poll. The result arrives as a
    /// `UsageUpdated` event.
    pub fn refresh_usage(&self) {
        self.send(ExecutorCommand::RefreshUsage);
    }

    /// Ask the executor to refresh the submitted reviews *and* the comment counts
    /// on a card's PR now, rather than waiting for the next background poll. The
    /// result arrives as a `CardUpdated` event carrying the card's new `reviews`,
    /// `reviewer_comment_count` (the dock badge), and `comment_count` (which
    /// surfaces the triage button the moment any reviewer's comment lands).
    pub fn fetch_reviews(&self, card_id: Uuid) {
        self.send(ExecutorCommand::RefreshReviews { card_id });
    }

    /// The current in-worktree preview status + URLs for a card, if one is (or
    /// was) active this session.
    pub fn preview(&self, card_id: Uuid) -> Option<(PreviewStatus, Vec<PreviewUrl>)> {
        self.previews.read().get(&card_id).cloned()
    }

    /// The current computed diff state for a card, if one has been requested this
    /// session. `None` means the user hasn't opened the diff yet.
    pub fn diff(&self, card_id: Uuid) -> Option<DiffState> {
        self.diffs.read().get(&card_id).cloned()
    }

    // --- PR review workflow -------------------------------------------------

    pub fn board_mode(&self) -> BoardMode {
        *self.board_mode.read()
    }

    /// Enter the PR-review board for a project: select it, flip to review mode,
    /// and kick off a scan so the columns populate.
    pub fn enter_review_mode(&self, project_id: Uuid) {
        self.select_view(SelectedView::Project(project_id));
        let mut bm = self.board_mode;
        bm.set(BoardMode::Review);
        self.scan_reviews(project_id);
    }

    pub fn exit_review_mode(&self) {
        let mut bm = self.board_mode;
        bm.set(BoardMode::Normal);
        self.select_review(None);
    }

    /// The PR-review tasks for a project (unsorted; the board buckets by column).
    pub fn review_tasks_for(&self, project_id: Uuid) -> Vec<ReviewTask> {
        self.review_tasks
            .read()
            .get(&project_id)
            .cloned()
            .unwrap_or_default()
    }

    /// A single review task by id, across every project's list.
    pub fn review_task(&self, review_id: Uuid) -> Option<ReviewTask> {
        self.review_tasks
            .read()
            .values()
            .flatten()
            .find(|t| t.id == review_id)
            .cloned()
    }

    /// How many of a project's review tasks need the user's attention (drives the
    /// sidebar badge): PRs to review, drafts awaiting validation, or failures.
    pub fn project_review_count(&self, project_id: Uuid) -> usize {
        self.review_tasks
            .read()
            .get(&project_id)
            .map(|v| v.iter().filter(|t| t.status.needs_attention()).count())
            .unwrap_or(0)
    }

    /// (waiting, urgent) card counts for a project's sidebar row — the
    /// per-project slice of the dock badge's card count. `urgent` is the
    /// subset that is failed or blocked on a question (red dot).
    pub fn project_attention_counts(&self, project_id: Uuid) -> (usize, usize) {
        let cards = self.cards.read();
        let mut waiting = 0;
        let mut urgent = 0;
        for c in cards.iter().filter(|c| c.project_id == project_id) {
            if c.needs_attention() {
                waiting += 1;
                if c.needs_urgent_attention() {
                    urgent += 1;
                }
            }
        }
        (waiting, urgent)
    }

    /// Total review tasks needing attention across all projects (dock badge).
    /// Only called from the macOS-only dock badge effect, so it's dead code on
    /// other platforms.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn review_attention_count(&self) -> usize {
        self.review_tasks
            .read()
            .values()
            .flatten()
            .filter(|t| t.status.needs_attention())
            .count()
    }

    pub fn scan_reviews(&self, project_id: Uuid) {
        self.send(ExecutorCommand::ScanReviews { project_id });
    }

    /// Start (or retry) a PR review. `guidance` is the user's steering for this
    /// pass — the detail panel sends what's in its box, the board sends whatever
    /// the task already carries, so a retry from a card doesn't drop it.
    pub fn start_review(&self, review_id: Uuid, guidance: String) {
        self.send(ExecutorCommand::StartReview {
            review_id,
            guidance,
        });
    }

    // Publishing and dismissing a review are both routed through the confirm
    // dialogs in `ui` (`confirm_publish_review` / `confirm_dismiss_review`) —
    // one posts to someone else's PR, the other is a permanent dismissal — so
    // there are deliberately no bare send helpers for them here.

    /// Run the project's app from a PR's review checkout (checked out on demand).
    pub fn start_review_preview(&self, review_id: Uuid) {
        self.send(ExecutorCommand::StartReviewPreview { review_id });
    }

    pub fn stop_review_preview(&self, review_id: Uuid) {
        self.send(ExecutorCommand::StopReviewPreview { review_id });
    }

    /// Persist edits to a project's config (e.g. its review contributors). The
    /// project signal updates on the `ProjectUpserted` echo.
    pub fn save_project(&self, project: Project) {
        self.send(ExecutorCommand::SaveProject {
            project: Box::new(project),
        });
    }

    /// Whether a card is marked to skip planning. Read from the signal seeded at
    /// startup and kept current by `SkipPlanChanged` events (no store access).
    pub fn card_skip_plan(&self, card_id: Uuid) -> bool {
        self.skip_plans
            .read()
            .get(&card_id)
            .copied()
            .unwrap_or(false)
    }

    /// Mark/unmark a card to skip planning. Only meaningful before it starts.
    pub fn set_card_skip_plan(&self, card_id: Uuid, skip: bool) {
        self.send(ExecutorCommand::SetSkipPlan { card_id, skip });
    }

    /// Whether a card auto-starts its self-review when the implementation
    /// finishes (on by default). Read from the signal seeded at startup and kept
    /// current by `AutoReviewChanged` events (no store access).
    pub fn card_auto_review(&self, card_id: Uuid) -> bool {
        self.auto_reviews
            .read()
            .get(&card_id)
            .copied()
            .unwrap_or(true)
    }

    /// Turn a card's automatic self-review on/off.
    pub fn set_card_auto_review(&self, card_id: Uuid, auto: bool) {
        self.send(ExecutorCommand::SetAutoReview { card_id, auto });
    }

    /// A card's attached image paths (the managed copies). Read from the signal
    /// seeded at startup and kept current by `AttachmentsChanged` events.
    pub fn card_attachments(&self, card_id: Uuid) -> Vec<PathBuf> {
        self.attachments
            .read()
            .get(&card_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Attach picked image files to a card. The executor copies each into the
    /// card's managed dir (outside any repo) and echoes `AttachmentsChanged`.
    pub fn attach_images(&self, card_id: Uuid, srcs: Vec<PathBuf>) {
        for src in srcs {
            self.send(ExecutorCommand::AttachImage { card_id, src });
        }
    }

    /// Attach a pasted image (PNG bytes) to a card.
    pub fn attach_image_bytes(&self, card_id: Uuid, data: Vec<u8>) {
        self.send(ExecutorCommand::AttachImageBytes { card_id, data });
    }

    /// Remove a previously attached image by its managed path.
    pub fn detach_image(&self, card_id: Uuid, path: PathBuf) {
        self.send(ExecutorCommand::DetachImage { card_id, path });
    }

    // --- direct CRUD --------------------------------------------------------
    //
    // These aren't agent-driven, but the store write is still offloaded to the
    // executor (off the UI thread). The model is write-then-event: the UI sends
    // a command, the executor persists and echoes an event the UI applies in
    // `apply_event`. Only *navigation* (selecting a new card/project) updates
    // immediately here; the data itself arrives via the event.

    pub fn add_project(&self, path: PathBuf) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let mut config = self.settings.read().new_project_config();
        // Fork worktrees from the repo's integration branch (prefers `dev`).
        if !demo_mode() {
            config.base_branch = usine_core::infra::git::detect_base_branch(&path);
        }
        let project = Project::new(name, path, config);
        let id = project.id;
        // Navigate now; the project itself arrives via `ProjectUpserted`.
        let mut view = self.selected_view;
        view.set(SelectedView::Project(id));
        self.send(ExecutorCommand::AddProject {
            project: Box::new(project),
        });
    }

    /// Remove a project and its cards from Usine (the folder/git repo on disk is
    /// not touched). Removal + view/selection fixup happen on `ProjectRemoved`.
    pub fn delete_project(&self, id: Uuid) {
        self.send(ExecutorCommand::DeleteProject { project_id: id });
    }

    pub fn create_card(&self, project_id: Uuid, title: String, description: String) {
        // Read the project's defaults from signals (no store read on the UI thread).
        let config = match self.projects.read().iter().find(|p| p.id == project_id) {
            Some(p) => p.config.new_card_config(),
            None => {
                push_toast(Severity::Error, "unknown project");
                return;
            }
        };
        let card = Card::new(project_id, title, description, config);
        // Select now; the card itself arrives via `CardUpdated`.
        let mut selected = self.selected_card;
        selected.set(Some(card.id));
        self.send(ExecutorCommand::CreateCard {
            card: Box::new(card),
        });
    }

    /// Persist global settings (default models for new projects/cards). The
    /// settings signal updates on the `SettingsUpdated` echo.
    pub fn save_settings(&self, settings: AppSettings) {
        self.send(ExecutorCommand::SaveSettings {
            settings: Box::new(settings),
        });
    }

    /// Persist a locally-edited card (title/description/config). The card signal
    /// updates on the `CardUpdated` echo.
    pub fn save_card(&self, card: Card) {
        self.send(ExecutorCommand::SaveCard {
            card: Box::new(card),
        });
    }

    /// Read the current card from the signal, apply `f`, and persist via
    /// `save_card`. Centralizes the clone-mutate-save pattern so call sites read
    /// as `state.update_card(id, |c| c.config.plan = spec)`.
    pub fn update_card(&self, id: Uuid, f: impl FnOnce(&mut Card)) {
        let Some(mut card) = self.cards.read().iter().find(|c| c.id == id).cloned() else {
            return;
        };
        f(&mut card);
        self.save_card(card);
    }

    /// Delete a card. Removal + selection clearing happen on the `CardRemoved`
    /// echo from the executor.
    pub fn delete_card(&self, id: Uuid) {
        self.send(ExecutorCommand::DeleteCard { card_id: id });
    }

    // --- derived views ---------------------------------------------------

    pub fn visible_cards(&self) -> Vec<Card> {
        let view = *self.selected_view.read();
        let mut out: Vec<Card> = self
            .cards
            .read()
            .iter()
            .filter(|c| match view {
                SelectedView::Global => true,
                SelectedView::Project(id) => c.project_id == id,
            })
            .cloned()
            .collect();
        // Creation order, oldest first. Without this the board follows the
        // store's primary-key scan (UUID-string order), which reshuffles every
        // column across restarts.
        out.sort_by_key(|c| c.created_at);
        out
    }

    pub fn project_name(&self, id: Uuid) -> String {
        self.projects
            .read()
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "—".into())
    }
}

/// The app runs the real `claude`/`codex`/`gh`/`git` by default. Opting into
/// demo mode with `USINE_SIM=1` swaps in simulated backends (no tokens spent, no
/// network), seeds a sample board, and uses a separate database — a safe
/// playground for exploring the UI.
pub(crate) fn demo_mode() -> bool {
    std::env::var("USINE_SIM")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn backends() -> (Arc<dyn ProviderFactory>, Arc<dyn Forge>, Arc<dyn GitOps>) {
    if demo_mode() {
        (Arc::new(SimFactory), Arc::new(SimForge), Arc::new(SimGit))
    } else {
        (Arc::new(RealFactory), Arc::new(GhForge), Arc::new(RealGit))
    }
}

fn open_store() -> Store {
    // Path is owned by core so the app, executor, and CLI can't disagree.
    let path = usine_core::infra::paths::store_path(demo_mode());
    match Store::open(&path) {
        Ok(store) => store,
        Err(e) => {
            let hint = if e.is_db_locked() {
                " Quit any other Usine instance — the database allows only one writer."
            } else {
                ""
            };
            push_toast(
                Severity::Error,
                format!(
                    "could not open database ({e}).{hint} Falling back to a temporary \
                     in-memory store — the board starts empty and changes will NOT be saved"
                ),
            );
            Store::open_in_memory().expect("in-memory store")
        }
    }
}

/// Seed a demo project with cards spread across columns so a first-run board
/// demonstrates the whole pipeline.
fn seed_demo(store: &Store, settings: &AppSettings) -> usine_core::Result<()> {
    let path = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut config = settings.new_project_config();
    // Without contributors to watch, a scan finds nothing and the review board
    // stays empty — these are the authors `SimForge::list_review_prs` returns.
    config.review_contributors = vec!["octocat".into(), "hubot".into()];
    let project = Project::new("Demo project", path, config);
    store.upsert_project(&project)?;

    let mk = |title: &str, desc: &str, state: CardState| {
        let mut c = Card::new(project.id, title, desc, project.config.new_card_config());
        c.state = state;
        c
    };

    let mut awaiting = mk(
        "Speed up cold start",
        "Profile and reduce the app's cold-start time.",
        CardState::AwaitingReview(ReviewSub::ReadyForReview),
    );
    awaiting.branch = Some("usine/speed-up-cold-start".into());
    // The hand-off a real implement run would have left at this gate.
    store.set_handoff(
        awaiting.id,
        &Handoff {
            summary: "Deferred the syntax-highlighting theme load until the first diff is opened, \
                      which is where the 300ms went. Cold start is now ~90ms on my machine. I did \
                      not touch the database open path — it looked cheap enough that the added \
                      complexity wasn't worth it."
                .into(),
            questions: vec![
                "The theme now loads on the first diff open, so that click is ~200ms slower. \
                 Worth pre-warming it in the background instead?"
                    .into(),
            ],
            tests: vec![
                "Launch the app cold — the board should paint before the window finishes animating"
                    .into(),
                "Open a card's diff — highlighting should appear, not plain text".into(),
            ],
        },
    )?;

    let mut ready = mk(
        "Bump dependencies",
        "Update all dependencies to their latest compatible versions.",
        CardState::ReadyToMerge,
    );
    ready.branch = Some("usine/bump-dependencies".into());
    ready.pr = Some(PrInfo {
        number: 17,
        url: "https://github.com/example/repo/pull/17".into(),
        title: "Bump dependencies".into(),
        state: "open".into(),
        reviewer: Some("octocat".into()),
        reviewer_recorded: true,
    });

    let cards = vec![
        mk(
            "Add dark mode",
            "Add a dark theme toggle to the settings screen.",
            CardState::StartingBlock,
        ),
        mk(
            "Refactor the auth module",
            "Split the monolithic auth module into smaller units.",
            CardState::Designing(DesignSub::AwaitingApproval {
                plan: "## Plan\n\n1. Extract token handling.\n2. Add tests.\n3. Wire it back in."
                    .into(),
            }),
        ),
        mk(
            "Fix flaky integration test",
            "Investigate and fix the intermittently failing test.",
            CardState::Implementing(RunSub::Running),
        ),
        awaiting,
        ready,
    ];
    for card in &cards {
        store.upsert_card(card)?;
    }
    Ok(())
}
