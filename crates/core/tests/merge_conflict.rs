//! A merge refused because the PR conflicts with its base is an offer, not an
//! error: the card stays in `ReadyToMerge` and the UI is told to ask whether an
//! agent should resolve the conflicts.
//!
//! Two things have to hold for that to be safe. A conflict must be recognized by
//! *asking the forge* (`mergeable`), never by matching gh's error prose — so
//! every other reason a merge fails (auth, protected branch, failing checks)
//! still reaches the user as the error it is. And `merge_ref` must tell a
//! conflicted merge apart from a broken one, since only the former leaves a
//! worktree an agent can resolve.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, CheckStatus, CoreError,
    DraftComment, ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, Forge, GitOps,
    MergeOutcome, Mergeable, PrInfo, PrSummary, Project, ProjectConfig, Provider, ProviderFactory,
    RealGit, ReviewComment, ReviewEvent, ReviewScope, ReviewSummary, RunConfig, RunHandle, RunMode,
    Severity, SimFactory, SimForge, SimGit, Store,
};

/// A forge whose merge always fails, reporting `status` when asked why.
struct RefusingForge {
    status: Mergeable,
}

#[async_trait]
impl Forge for RefusingForge {
    async fn merge(&self, _: &Path, _: u64) -> usine_core::Result<()> {
        Err(CoreError::forge(
            "gh pr merge 7 --squash failed: not mergeable",
        ))
    }
    async fn is_merged(&self, _: &Path, _: u64) -> usine_core::Result<bool> {
        Ok(false)
    }
    async fn merge_status(&self, _: &Path, _: u64) -> usine_core::Result<Mergeable> {
        Ok(self.status)
    }
    async fn delete_remote_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    // Failing here keeps the startup comment poll from refreshing the card —
    // its immediate first tick would race the assertions on the cached
    // checks/mergeability that the resolve flow itself writes.
    async fn fetch_comments(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewComment>> {
        Err(CoreError::forge("comment poll disabled in this test"))
    }
    // Unreached by a merge; defer to the simulator.
    async fn create_pr(
        &self,
        r: &Path,
        t: &str,
        b: &str,
        base: &str,
        h: &str,
        rev: Option<&str>,
        d: bool,
    ) -> usine_core::Result<PrInfo> {
        SimForge.create_pr(r, t, b, base, h, rev, d).await
    }
    async fn list_review_prs(
        &self,
        r: &Path,
        s: ReviewScope,
    ) -> usine_core::Result<Vec<PrSummary>> {
        SimForge.list_review_prs(r, s).await
    }
    async fn submit_review(
        &self,
        r: &Path,
        n: u64,
        e: ReviewEvent,
        b: &str,
        c: &[DraftComment],
    ) -> usine_core::Result<()> {
        SimForge.submit_review(r, n, e, b, c).await
    }
    async fn list_reviewers(&self, r: &Path) -> usine_core::Result<Vec<String>> {
        SimForge.list_reviewers(r).await
    }
    async fn list_submitted_reviews(
        &self,
        r: &Path,
        n: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        SimForge.list_submitted_reviews(r, n).await
    }
    async fn reply_to_comment(&self, r: &Path, n: u64, c: u64, b: &str) -> usine_core::Result<()> {
        SimForge.reply_to_comment(r, n, c, b).await
    }
    async fn mark_ready(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.mark_ready(r, n).await
    }
    async fn resolve_threads(&self, r: &Path, n: u64, c: &[u64]) -> usine_core::Result<usize> {
        SimForge.resolve_threads(r, n, c).await
    }
    async fn list_threads(
        &self,
        r: &Path,
        n: u64,
    ) -> usine_core::Result<Vec<usine_core::ReviewThread>> {
        SimForge.list_threads(r, n).await
    }
}

async fn wait_for<F, T>(rx: &mut UnboundedReceiver<ExecutorEvent>, mut f: F) -> T
where
    F: FnMut(&ExecutorEvent) -> Option<T>,
{
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(15), rx.next())
            .await
            .expect("timed out waiting for an executor event")
            .expect("event stream closed unexpectedly");
        if let Some(v) = f(&evt) {
            return v;
        }
    }
}

fn ready_to_merge_card(store: &Store, project_id: uuid::Uuid) -> Card {
    let mut card = Card::new(project_id, "t", "d", CardConfig::default());
    card.state = CardState::ReadyToMerge;
    card.branch = Some("feat/thing".into());
    card.pr = Some(PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: None,
        reviewer_recorded: false,
    });
    store.upsert_card(&card).unwrap();
    card
}

fn seeded(status: Mergeable) -> (Store, Card, UnboundedReceiver<ExecutorEvent>) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-conflict"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = ready_to_merge_card(&store, project.id);

    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(RefusingForge { status }),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::Merge {
        card_id: card.id,
        delete_branch: true,
        force: false,
    });
    (store, card, rx)
}

/// The conflict is surfaced as its own event (carrying the PR and the base it
/// conflicts with), and the card stays merge-able rather than being failed.
#[tokio::test]
async fn a_conflicting_merge_offers_to_resolve_instead_of_erroring() {
    let (store, card, mut rx) = seeded(Mergeable::Conflicting);

    let (pr, base) = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::MergeConflict { pr_number, base } if e.card_id == card.id => {
            Some((*pr_number, base.clone()))
        }
        // An error toast here would mean the conflict was reported as a failure.
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => panic!("a conflict must not surface as an error: {message}"),
        _ => None,
    })
    .await;

    assert_eq!(pr, 7);
    assert_eq!(base, "dev", "the dialog needs the branch being merged into");
    assert!(
        matches!(
            store.get_card(card.id).unwrap().state,
            CardState::ReadyToMerge
        ),
        "a conflicting card stays ready to merge — resolving is optional"
    );
}

/// Every *other* merge failure (auth, protected branch, a failing required
/// check) must still reach the user as an error. Matching on gh's error text
/// would have swept these into the conflict dialog.
#[tokio::test]
async fn a_non_conflicting_merge_failure_still_errors() {
    let (store, card, mut rx) = seeded(Mergeable::Clean);

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => Some(message.clone()),
        ExecutorEventKind::MergeConflict { .. } => {
            panic!("a mergeable PR's failure is not a conflict")
        }
        _ => None,
    })
    .await;

    assert!(
        msg.contains("not mergeable"),
        "the forge's error must survive: {msg}"
    );
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::ReadyToMerge
    ));
}

/// GitHub answers `UNKNOWN` while it recomputes mergeability. That is not "no
/// conflict", but it is not a conflict either — after polling, the original
/// merge error stands rather than a dialog appearing on a guess.
#[tokio::test]
async fn an_unknown_merge_status_is_never_guessed_into_a_conflict() {
    let (_store, _card, mut rx) = seeded(Mergeable::Unknown);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            ..
        } => Some(()),
        ExecutorEventKind::MergeConflict { .. } => panic!("UNKNOWN must not be read as a conflict"),
        _ => None,
    })
    .await;
}

// --- resolving: an agent runs only when there is something to resolve --------

/// Git whose merge always stops on a conflict, standing in for a branch whose
/// base has moved. Everything else succeeds. The fields model what the
/// worktree looks like *after* the agent's turn — whether the merge is still in
/// progress, which paths still carry markers — and record whether the commit
/// completing the merge (and its push) actually happened.
#[derive(Default)]
struct ConflictingGit {
    mid_merge: bool,
    unresolved: Vec<String>,
    committed: Arc<Mutex<bool>>,
    pushed: Arc<Mutex<bool>>,
}

#[async_trait]
impl GitOps for ConflictingGit {
    async fn merge_ref(&self, _: &Path, _: &str) -> usine_core::Result<MergeOutcome> {
        Ok(MergeOutcome::Conflicted(vec!["src/lib.rs".into()]))
    }
    async fn merge_in_progress(&self, _: &Path) -> usine_core::Result<bool> {
        Ok(self.mid_merge)
    }
    async fn unresolved_conflicts(&self, _: &Path) -> usine_core::Result<Vec<String>> {
        Ok(self.unresolved.clone())
    }
    async fn fetch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn create_worktree(
        &self,
        _: &Path,
        _: &str,
        _: &Path,
        _: &str,
    ) -> usine_core::Result<()> {
        Ok(())
    }
    async fn remove_worktree(&self, _: &Path, _: &Path) -> usine_core::Result<()> {
        Ok(())
    }
    async fn worktree_add_existing(&self, _: &Path, _: &str, _: &Path) -> usine_core::Result<()> {
        Ok(())
    }
    async fn worktree_add_detached(&self, _: &Path, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn fetch_pr(&self, _: &Path, _: u64, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn reset_mixed(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn rename_branch(&self, _: &Path, _: &str, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn delete_branch(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        Ok(())
    }
    async fn commit_all(&self, _: &Path, _: &str) -> usine_core::Result<bool> {
        *self.committed.lock().unwrap() = true;
        Ok(true)
    }
    async fn push(&self, _: &Path, _: &str) -> usine_core::Result<()> {
        *self.pushed.lock().unwrap() = true;
        Ok(())
    }
}

/// Seed a `ReadyToMerge` card with an isolated worktree on disk (the launch
/// tripwire refuses to run a write agent without one) and ask to resolve.
/// `status` is what the forge answers when the resolve re-reads mergeability;
/// the card itself is seeded with the stale snapshot the button was shown
/// from: `Conflicting`, checks green.
fn resolving(
    git: Arc<dyn GitOps>,
    worktree: &Path,
    status: Mergeable,
) -> (Store, Card, UnboundedReceiver<ExecutorEvent>) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-conflict"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let mut card = ready_to_merge_card(&store, project.id);
    card.worktree_path = Some(worktree.to_path_buf());
    card.mergeable = Mergeable::Conflicting;
    card.checks = CheckStatus::Passing;
    store.upsert_card(&card).unwrap();

    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(RefusingForge { status }),
        git,
    });
    handle.send(ExecutorCommand::ResolveConflicts { card_id: card.id });
    (store, card, rx)
}

/// The resolution run happens in the card's own worktree, through the same
/// applying-fixes loop a post-PR change uses — so it lands back on `ReadyToMerge`.
#[tokio::test]
async fn resolving_conflicts_hands_the_conflicted_worktree_to_an_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let (_store, card, mut rx) = resolving(
        Arc::new(ConflictingGit::default()),
        tmp.path(),
        Mergeable::Conflicting,
    );

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => matches!(
            c.state,
            CardState::PrReview(usine_core::PrReviewSub::ApplyingFixes)
        )
        .then_some(()),
        _ => None,
    })
    .await;
}

/// When the merge turns out clean — the base moved again, or someone updated the
/// branch by hand — there is nothing for an agent to do. Spending a run (and a
/// commit) on it would be waste, so the branch is just pushed and the card left
/// exactly where it was, ready to merge again.
#[tokio::test]
async fn a_conflict_that_resolved_itself_costs_no_agent_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, card, mut rx) = resolving(Arc::new(SimGit), tmp.path(), Mergeable::Conflicting);

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } => Some(message.clone()),
        ExecutorEventKind::CardUpdated(c)
            if c.id == card.id && !matches!(c.state, CardState::ReadyToMerge) =>
        {
            panic!("a clean merge must not move the card: {:?}", c.state)
        }
        _ => None,
    })
    .await;

    assert!(msg.contains("No conflicts left"), "got: {msg}");
    let after = store.get_card(card.id).unwrap();
    assert!(matches!(after.state, CardState::ReadyToMerge));
    // The push published a merge commit, so the whole cached PR snapshot is
    // stale: the green checks must fall back to Pending (a leftover `Passing`
    // would re-show Merge only for the executor to refuse it while CI re-runs)
    // and the mergeability back to Unknown until the poll re-reads both.
    assert_eq!(after.checks, CheckStatus::Pending);
    assert_eq!(after.mergeable, Mergeable::Unknown);
}

/// When the forge itself says the PR merges cleanly, the resolve is a pure
/// no-op: no local merge, no push — the board's "Resolve conflicts" was drawn
/// from a stale snapshot, and mutating the PR (a pointless merge commit that
/// re-triggers CI) would make the button's promise a lie. The card just learns
/// `Clean` so the poll-refreshed board re-shows Merge.
#[tokio::test]
async fn a_conflict_the_forge_says_is_gone_is_not_re_resolved() {
    let tmp = tempfile::tempdir().unwrap();
    // ConflictingGit would hand a conflict to an agent — reaching it at all
    // means the forge's `Clean` answer was ignored, which the panic arm below
    // catches as the card moving off `ReadyToMerge`.
    let (store, card, mut rx) = resolving(
        Arc::new(ConflictingGit::default()),
        tmp.path(),
        Mergeable::Clean,
    );

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } => Some(message.clone()),
        ExecutorEventKind::CardUpdated(c)
            if c.id == card.id && !matches!(c.state, CardState::ReadyToMerge) =>
        {
            panic!("a healed conflict must not start a resolve: {:?}", c.state)
        }
        _ => None,
    })
    .await;

    assert!(msg.contains("merges cleanly"), "got: {msg}");
    let after = store.get_card(card.id).unwrap();
    assert!(matches!(after.state, CardState::ReadyToMerge));
    assert_eq!(after.mergeable, Mergeable::Clean);
    // Nothing was pushed, so the checks the card knew stay as they were.
    assert_eq!(after.checks, CheckStatus::Passing);
}

/// Every prompt handed to a provider, tagged with the run mode that asked for it.
type Prompts = Arc<Mutex<Vec<(RunMode, String)>>>;

/// Wraps the simulator, recording each run's full prompt before delegating —
/// and refusing the first start outright (a CLI that failed to spawn), which
/// is what parks a card at `Failed` with its stashed task intact.
struct FlakyProvider {
    inner: Arc<dyn AgentProvider>,
    prompts: Prompts,
    failed_once: Arc<Mutex<bool>>,
}

#[async_trait]
impl AgentProvider for FlakyProvider {
    fn provider(&self) -> Provider {
        self.inner.provider()
    }
    fn interactive(&self) -> bool {
        self.inner.interactive()
    }
    async fn start(&self, cfg: RunConfig) -> usine_core::Result<RunHandle> {
        self.prompts
            .lock()
            .unwrap()
            .push((cfg.mode, cfg.full_prompt()));
        {
            let mut failed = self.failed_once.lock().unwrap();
            if !*failed {
                *failed = true;
                return Err(CoreError::other("agent CLI failed to start"));
            }
        }
        self.inner.start(cfg).await
    }
}

struct FlakyFactory {
    prompts: Prompts,
    failed_once: Arc<Mutex<bool>>,
}

impl ProviderFactory for FlakyFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(FlakyProvider {
            inner: SimFactory.make(provider),
            prompts: self.prompts.clone(),
            failed_once: self.failed_once.clone(),
        })
    }
}

/// A faulted conflict-resolution run must not forget its task on Retry. The
/// task lives entirely in the launch extra (the conflict brief); `relaunch`
/// rebuilds the prompt from scratch, so without the stashed copy the retried
/// agent resumes into finished work with nothing asking it to conclude the
/// merge — it changes no files, the no-commit guard fails the run, and every
/// further Retry loops the same way.
#[tokio::test]
async fn a_retried_conflict_fix_still_knows_about_the_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-conflict"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let mut card = ready_to_merge_card(&store, project.id);
    card.worktree_path = Some(tmp.path().to_path_buf());
    store.upsert_card(&card).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(FlakyFactory {
            prompts: prompts.clone(),
            failed_once: Arc::new(Mutex::new(false)),
        }),
        forge: Arc::new(RefusingForge {
            status: Mergeable::Conflicting,
        }),
        git: Arc::new(ConflictingGit::default()),
    });

    // The first resolution run's provider dies at start: the card is demoted to
    // `Failed`, recoverable — with the conflict brief stashed at launch.
    handle.send(ExecutorCommand::ResolveConflicts { card_id: card.id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id && c.state.is_failed() => Some(()),
        _ => None,
    })
    .await;
    assert!(
        store
            .get_fix_extra(card.id)
            .unwrap()
            .is_some_and(|e| e.contains("src/lib.rs")),
        "the conflict brief must be stashed at launch for a later retry"
    );

    // The failing launch emits `Failed` while still holding the card's
    // exclusive claim (it has teardown left to await); the dispatcher silently
    // drops any exclusive command sent in that window. The UI can't hit it —
    // the busy flag disables Retry until the claim releases — so wait for the
    // release the same way before retrying.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardBusy { busy: false } if e.card_id == card.id => Some(()),
        _ => None,
    })
    .await;

    handle.send(ExecutorCommand::Retry { card_id: card.id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id && !c.state.is_running() => Some(()),
        _ => None,
    })
    .await;

    let fix_prompts: Vec<String> = prompts
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| *m == RunMode::ApplyFixes)
        .map(|(_, p)| p.clone())
        .collect();
    assert_eq!(fix_prompts.len(), 2, "the retry launched a second fix run");
    assert!(
        fix_prompts[1].contains("cannot be merged") && fix_prompts[1].contains("src/lib.rs"),
        "the retried fix run must restate the conflict task:\n{}",
        fix_prompts[1]
    );
}

// --- asking instead of guessing ---------------------------------------------

/// A one-shot provider (like the real CLIs — `interactive()` is false, so an
/// answer resumes with a fresh run rather than being forwarded over a control
/// channel) that ends each run with the next scripted final message.
struct ScriptedProvider {
    results: Arc<Mutex<Vec<String>>>,
    prompts: Prompts,
}

#[async_trait]
impl AgentProvider for ScriptedProvider {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn interactive(&self) -> bool {
        false
    }
    async fn start(&self, cfg: RunConfig) -> usine_core::Result<RunHandle> {
        self.prompts
            .lock()
            .unwrap()
            .push((cfg.mode, cfg.full_prompt()));
        let result = {
            let mut rs = self.results.lock().unwrap();
            if rs.is_empty() {
                "done".to_string()
            } else {
                rs.remove(0)
            }
        };
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, _ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(usine_core::AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        let _ = evt_tx.unbounded_send(usine_core::AgentEvent::Done {
            result,
            cost_usd: 0.0,
            usage: usine_core::Usage::default(),
        });
        drop(evt_tx);
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

struct ScriptedFactory {
    results: Arc<Mutex<Vec<String>>>,
    prompts: Prompts,
}

impl ProviderFactory for ScriptedFactory {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(ScriptedProvider {
            results: self.results.clone(),
            prompts: self.prompts.clone(),
        })
    }
}

/// Seed a `ReadyToMerge` card with a worktree and drive `ResolveConflicts`
/// against a scripted agent, returning everything the assertions need.
#[allow(clippy::type_complexity)]
fn scripted_resolve(
    git: Arc<ConflictingGit>,
    worktree: &Path,
    results: Vec<String>,
) -> (
    Store,
    Card,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
    Prompts,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-merge-conflict"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let mut card = ready_to_merge_card(&store, project.id);
    card.worktree_path = Some(worktree.to_path_buf());
    card.mergeable = Mergeable::Conflicting;
    store.upsert_card(&card).unwrap();

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(ScriptedFactory {
            results: Arc::new(Mutex::new(results)),
            prompts: prompts.clone(),
        }),
        forge: Arc::new(RefusingForge {
            status: Mergeable::Conflicting,
        }),
        git,
    });
    handle.send(ExecutorCommand::ResolveConflicts { card_id: card.id });
    (store, card, handle, rx, prompts)
}

const ASKING: &str = "I resolved src/a.rs. src/lib.rs needs your call.\n\n\
```usine-questions\n\
[{\"question\":\"Keep the retry loop?\",\"options\":[\"Keep\",\"Drop\"]}]\n\
```\n";

/// The whole point: an agent that can't settle a conflict from the code parks
/// the card on the question instead of guessing — and, crucially, publishes
/// nothing. Committing here would *complete the merge* and push it to the open
/// PR, which no later step could take back.
#[tokio::test]
async fn a_conflict_run_that_asks_parks_the_card_and_publishes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let git = Arc::new(ConflictingGit {
        mid_merge: true,
        unresolved: vec!["src/lib.rs".into()],
        ..Default::default()
    });
    let (store, card, _handle, mut rx, _prompts) =
        scripted_resolve(git.clone(), tmp.path(), vec![ASKING.to_string()]);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => matches!(
            c.state,
            CardState::PrReview(usine_core::PrReviewSub::AwaitingAnswer(_))
        )
        .then_some(()),
        _ => None,
    })
    .await;

    let after = store.get_card(card.id).unwrap();
    let iv = after.state.intervention().expect("the question is parked");
    assert_eq!(iv.question, "Keep the retry loop?");
    assert_eq!(iv.options, vec!["Keep", "Drop"]);
    assert!(!*git.committed.lock().unwrap(), "nothing may be committed");
    assert!(!*git.pushed.lock().unwrap(), "nothing may be pushed");
    // The prose survives as the recap so the user can see what it did get
    // through — but the machine-facing block must not leak into it.
    let recap = store.get_review_recap(card.id).unwrap().unwrap_or_default();
    assert!(recap.contains("src/lib.rs needs your call"), "got: {recap}");
    assert!(!recap.contains("usine-questions"), "got: {recap}");
    // The brief is still stashed: the answering run has to restate it.
    assert!(store.get_fix_extra(card.id).unwrap().is_some());
}

/// Answering resumes the resolution: a fresh run, told both what the merge is
/// and what the user decided — the process that asked is long gone, so neither
/// can be assumed to be in its context.
#[tokio::test]
async fn answering_resumes_the_resolution_with_the_brief_and_the_answer() {
    let tmp = tempfile::tempdir().unwrap();
    let git = Arc::new(ConflictingGit {
        mid_merge: true,
        unresolved: vec!["src/lib.rs".into()],
        ..Default::default()
    });
    let (store, card, handle, mut rx, prompts) = scripted_resolve(
        git.clone(),
        tmp.path(),
        vec![ASKING.to_string(), "resolved".to_string()],
    );

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => matches!(
            c.state,
            CardState::PrReview(usine_core::PrReviewSub::AwaitingAnswer(_))
        )
        .then_some(()),
        _ => None,
    })
    .await;

    handle.send(ExecutorCommand::Answer {
        card_id: card.id,
        text: "Keep".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => matches!(
            c.state,
            CardState::PrReview(usine_core::PrReviewSub::ApplyingFixes)
        )
        .then_some(()),
        _ => None,
    })
    .await;

    // The state flips to `ApplyingFixes` a beat before the relaunched run
    // reaches the provider, so wait on the prompt itself.
    let second = {
        let mut second = None;
        for _ in 0..150 {
            let fixes: Vec<String> = prompts
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, _)| *m == RunMode::ApplyFixes)
                .map(|(_, t)| t.clone())
                .collect();
            if fixes.len() >= 2 {
                second = Some(fixes[1].clone());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        second.expect("the answer must launch a second fix run")
    };
    assert!(second.contains("cannot be merged") && second.contains("src/lib.rs"));
    assert!(second.contains("Keep the retry loop?") && second.contains("Keep"));
    // The Q&A is on the record for a later back-to-start.
    assert!(store
        .get_card(card.id)
        .unwrap()
        .qa_log
        .iter()
        .any(|l| l.contains("Keep the retry loop?")));
}

/// A run that claims success while leaving markers in the tree must not
/// publish them: `git add -A` would stage `<<<<<<<` straight onto the PR.
#[tokio::test]
async fn a_run_that_leaves_conflict_markers_fails_instead_of_committing_them() {
    let tmp = tempfile::tempdir().unwrap();
    let git = Arc::new(ConflictingGit {
        mid_merge: true,
        unresolved: vec!["src/lib.rs".into()],
        ..Default::default()
    });
    let (store, card, _handle, mut rx, _prompts) = scripted_resolve(
        git.clone(),
        tmp.path(),
        vec!["All conflicts resolved.".to_string()],
    );

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id && c.state.is_failed() => Some(()),
        _ => None,
    })
    .await;

    let CardState::Failed { message, .. } = store.get_card(card.id).unwrap().state else {
        panic!("expected a faulted card");
    };
    assert!(message.contains("src/lib.rs"), "got: {message}");
    assert!(!*git.committed.lock().unwrap(), "markers must not commit");
    assert!(!*git.pushed.lock().unwrap());
}

/// The gate must stay narrow: a resolution that actually resolved everything
/// commits (completing the merge) and pushes, exactly as before.
#[tokio::test]
async fn a_clean_resolution_still_commits_and_pushes() {
    let tmp = tempfile::tempdir().unwrap();
    let git = Arc::new(ConflictingGit {
        mid_merge: true,
        unresolved: Vec::new(),
        ..Default::default()
    });
    let (store, card, _handle, mut rx, _prompts) =
        scripted_resolve(git.clone(), tmp.path(), vec!["Resolved both sides.".into()]);

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            matches!(c.state, CardState::ReadyToMerge).then_some(())
        }
        _ => None,
    })
    .await;

    assert!(
        *git.committed.lock().unwrap(),
        "the merge must be completed"
    );
    assert!(*git.pushed.lock().unwrap(), "and published to the PR");
    assert!(matches!(
        store.get_card(card.id).unwrap().state,
        CardState::ReadyToMerge
    ));
}

// --- real git: a conflicted merge is an outcome, a broken one is an error ----

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repo on `dev` whose `feature` worktree and `dev` both edited `a.txt` — the
/// shape of every PR that stops merging because its base moved.
fn diverged_repo(tmp: &Path) -> (PathBuf, PathBuf) {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["branch", "-M", "dev"]);

    let wt = tmp.join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
            "dev",
        ],
    );
    std::fs::write(wt.join("a.txt"), "feature\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "feat"]);

    // `dev` moves underneath the branch, touching the same line.
    std::fs::write(repo.join("a.txt"), "dev\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "dev moves"]);
    (repo, wt)
}

/// The conflict lives on the *remote's* base branch, so the merge is only
/// correct if the fetch refreshed `origin/dev` first. `git fetch origin` with no
/// refspec is what does that — a bare `git fetch origin dev` would only write
/// `FETCH_HEAD`, leaving the merge to silently succeed against a stale ref.
#[tokio::test]
async fn fetching_the_remote_is_what_makes_the_base_merge_see_the_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = tmp.path().join("origin.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );

    let repo = tmp.path().join("repo");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["branch", "-M", "dev"]);
    git(&repo, &["push", "-qu", "origin", "dev"]);

    // The card's branch, in its own worktree, edits `a.txt`.
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
            "dev",
        ],
    );
    std::fs::write(wt.join("a.txt"), "feature\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "feat"]);

    // Meanwhile a *different* clone moves `dev` on the remote, touching the same
    // line. It has to be another clone: pushing from `repo` would update its own
    // `origin/dev` as a side effect, and nothing would be stale to fetch.
    let other = tmp.path().join("other");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "-b",
            "dev",
            origin.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    git(&other, &["config", "user.email", "o@o.dev"]);
    git(&other, &["config", "user.name", "o"]);
    git(&other, &["config", "commit.gpgsign", "false"]);
    std::fs::write(other.join("a.txt"), "dev\n").unwrap();
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "dev moves"]);
    git(&other, &["push", "-q", "origin", "dev"]);

    let gitops = RealGit;
    // Without the fetch, `origin/dev` is stale and the merge is a clean no-op —
    // the failure mode this ordering exists to prevent.
    assert_eq!(
        gitops.merge_ref(&wt, "origin/dev").await.unwrap(),
        MergeOutcome::Clean,
        "a stale origin/dev must look clean, proving the fetch below is load-bearing"
    );

    gitops.fetch(&wt, "origin").await.unwrap();
    assert_eq!(
        gitops.merge_ref(&wt, "origin/dev").await.unwrap(),
        MergeOutcome::Conflicted(vec!["a.txt".to_string()]),
    );
    assert!(std::fs::read_to_string(wt.join("a.txt"))
        .unwrap()
        .contains("<<<<<<<"));
}

#[tokio::test]
async fn merge_ref_reports_conflicts_as_an_outcome_and_breakage_as_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (_repo, wt) = diverged_repo(tmp.path());
    let gitops = RealGit;

    // A ref that doesn't exist leaves no unmerged paths, so it is a real failure
    // — not an empty conflict handed to an agent with nothing to resolve.
    gitops
        .merge_ref(&wt, "origin/nonexistent")
        .await
        .expect_err("merging an unknown ref must be an error");

    // The genuine conflict is an outcome, naming the file to resolve.
    let outcome = gitops.merge_ref(&wt, "dev").await.unwrap();
    assert_eq!(outcome, MergeOutcome::Conflicted(vec!["a.txt".to_string()]));
    // ...and the worktree is left mid-merge, which is what lets the agent (and
    // then `commit_all`) finish it.
    assert!(wt.join(".git").exists() || wt.join(".git").is_file());
    let conflicted = std::fs::read_to_string(wt.join("a.txt")).unwrap();
    assert!(
        conflicted.contains("<<<<<<<"),
        "conflict markers must be present"
    );

    // Resolving and committing — exactly what the agent + `commit_all` do —
    // concludes the merge, after which the same merge is a clean no-op.
    std::fs::write(wt.join("a.txt"), "resolved\n").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "resolve"]);
    assert_eq!(
        gitops.merge_ref(&wt, "dev").await.unwrap(),
        MergeOutcome::Clean
    );
}
