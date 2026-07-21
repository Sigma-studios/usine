//! A duplicate `ApprovePlan` must be dropped, not raced.
//!
//! `approve_plan` reads the card's state, builds the worktree, *then* applies
//! the transition — and the dispatcher spawns it. Before the per-card in-flight
//! claim, two of them overlapped: both passed the `AwaitingApproval` guard, both
//! ran `ensure_worktree` (so the loser tore down and recreated the worktree the
//! winner's agent was already launched into), and the loser then failed with
//! "illegal transition: Implementing(Running) cannot accept ApprovePlan".
//!
//! A duplicate is easy to produce: the Approve button can't hide until the
//! resulting `CardUpdated` lands, which is after the worktree work.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, DesignSub, ExecutorCommand, ExecutorConfig,
    ExecutorEventKind, GitOps, MergeOutcome, Project, ProjectConfig, Result, Severity, SimFactory,
    SimForge, SimGit, Store,
};

/// Simulator git that counts worktree creations and makes each one slow, so the
/// duplicate command is dispatched while the first is still inside the window
/// this test is about.
struct SlowGit {
    created: Arc<AtomicUsize>,
    removed: Arc<AtomicUsize>,
}

#[async_trait]
impl GitOps for SlowGit {
    async fn create_worktree(&self, r: &Path, b: &str, p: &Path, base: &str) -> Result<()> {
        self.created.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        SimGit.create_worktree(r, b, p, base).await
    }
    async fn remove_worktree(&self, r: &Path, p: &Path) -> Result<()> {
        self.removed.fetch_add(1, Ordering::SeqCst);
        SimGit.remove_worktree(r, p).await
    }
    // Everything else defers to the simulator's always-succeeds behavior.
    async fn worktree_add_existing(&self, r: &Path, b: &str, p: &Path) -> Result<()> {
        SimGit.worktree_add_existing(r, b, p).await
    }
    async fn worktree_add_detached(&self, r: &Path, p: &Path, c: &str) -> Result<()> {
        SimGit.worktree_add_detached(r, p, c).await
    }
    async fn fetch_pr(&self, r: &Path, n: u64, b: &str) -> Result<()> {
        SimGit.fetch_pr(r, n, b).await
    }
    async fn reset_mixed(&self, d: &Path, g: &str) -> Result<()> {
        SimGit.reset_mixed(d, g).await
    }
    async fn rename_branch(&self, d: &Path, o: &str, n: &str) -> Result<()> {
        SimGit.rename_branch(d, o, n).await
    }
    async fn delete_branch(&self, r: &Path, b: &str) -> Result<()> {
        SimGit.delete_branch(r, b).await
    }
    async fn fetch(&self, d: &Path, remote: &str) -> Result<()> {
        SimGit.fetch(d, remote).await
    }
    async fn merge_ref(&self, d: &Path, g: &str) -> Result<MergeOutcome> {
        SimGit.merge_ref(d, g).await
    }
    async fn commit_all(&self, d: &Path, m: &str) -> Result<bool> {
        SimGit.commit_all(d, m).await
    }
    async fn push(&self, d: &Path, b: &str) -> Result<()> {
        SimGit.push(d, b).await
    }
}

#[tokio::test]
async fn duplicate_approve_plan_is_dropped() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-approve-race"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();

    // Park the card exactly where the Approve button lives, with a plan that has
    // no outstanding questions (the other reason approval is refused).
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::Designing(DesignSub::AwaitingApproval {
        plan: "## Plan\n\n1. Do the thing.".into(),
    });
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let created = Arc::new(AtomicUsize::new(0));
    let removed = Arc::new(AtomicUsize::new(0));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SlowGit {
            created: created.clone(),
            removed: removed.clone(),
        }),
    });

    // The double click.
    handle.send(ExecutorCommand::ApprovePlan { card_id });
    handle.send(ExecutorCommand::ApprovePlan { card_id });

    // Collect everything the executor says for long enough to cover both the
    // (slow) worktree build and any late failure from the duplicate.
    let errors = Arc::new(Mutex::new(Vec::<String>::new()));
    // An ordered log of the busy flips and the approval's own transition, to
    // check the UI is never told "idle" while the card is still mid-approve.
    let trace = Arc::new(Mutex::new(Vec::<String>::new()));
    let collected = errors.clone();
    let logged = trace.clone();
    let drain = tokio::spawn(async move {
        while let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(2), rx.next()).await {
            match &evt.kind {
                ExecutorEventKind::Toast {
                    severity: Severity::Error,
                    message,
                } => collected.lock().unwrap().push(message.clone()),
                ExecutorEventKind::CardBusy { busy } => {
                    logged.lock().unwrap().push(format!("busy={busy}"))
                }
                ExecutorEventKind::CardUpdated(c)
                    if matches!(c.state, CardState::Implementing(_)) =>
                {
                    logged.lock().unwrap().push("implementing".into())
                }
                _ => {}
            }
        }
    });
    let _ = tokio::time::timeout(Duration::from_secs(10), drain).await;

    // Exactly one busy cycle: the dropped duplicate never claims, so it can't
    // flip the card back to idle while the winner is still working.
    let trace = trace.lock().unwrap();
    assert_eq!(
        trace.iter().filter(|e| *e == "busy=true").count(),
        1,
        "only the winning approve marks the card busy, got: {trace:?}"
    );
    assert_eq!(
        trace.iter().filter(|e| *e == "busy=false").count(),
        1,
        "the claim is released exactly once, got: {trace:?}"
    );
    // Ordering: busy is released only *after* the card has advanced, so the UI
    // never has a frame where the card looks both idle and un-approved. (The
    // run actor may emit further `Implementing` updates in between, hence
    // anchoring on the first one rather than an exact sequence.)
    let pos = |e: &str| trace.iter().position(|x| x == e);
    assert!(
        pos("busy=true") < pos("implementing") && pos("implementing") < pos("busy=false"),
        "busy must span the whole approve, got: {trace:?}"
    );

    let errors = errors.lock().unwrap();
    assert!(
        errors.is_empty(),
        "the duplicate approve must be dropped silently, got: {errors:?}"
    );
    assert_eq!(
        created.load(Ordering::SeqCst),
        1,
        "only the winning approve may build the worktree"
    );
    assert_eq!(
        removed.load(Ordering::SeqCst),
        0,
        "the duplicate must not tear down the live worktree"
    );

    // And the card really did advance — the drop must not swallow the approval.
    // Implementing *or* past it: the simulated implement run can finish inside
    // the drain window above.
    let state = store.get_card(card_id).unwrap().state;
    assert!(
        matches!(
            state,
            CardState::Implementing(_) | CardState::AwaitingReview(_)
        ),
        "the winning approve still advances the card past the plan gate, got {state:?}"
    );
}
