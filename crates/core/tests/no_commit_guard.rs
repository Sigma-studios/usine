//! A write run that changes no files must not sail through to review.
//!
//! `finalize_run` commits the card's worktree, but an implement/fix run can
//! finish having written nothing — the agent answered in prose, decided no
//! change was needed, or reverted its own edits. `git add -A` then stages
//! nothing and `git commit` reports "nothing to commit". Without a guard the
//! card would advance to the review gate on an empty branch: it looks done, but
//! the diff is empty and any PR opened from it would be empty too (exactly how a
//! "ready for PR" card ended up with no changes in the diff viewer).
//!
//! The guard demotes such a run to `Failed` (a running state's escape hatch) so
//! the user sees it needs attention and can Retry, instead of a silent dead-end.
//!
//! The exception is a retried implement run whose earlier attempt already
//! committed: it finds the work finished and (as `resume_extra` instructs)
//! changes nothing. Demoting it would re-arm the same retry forever, so when
//! the branch already carries commits beyond the base the run advances instead.
//!
//! Fix runs never demote at all: their branch is never empty (the empty-PR
//! disaster the guard prevents can't happen) and every fix transition lands at
//! a gate that re-checks the result, so a no-op fix run advances with a warning
//! — dropping the stashed "Fix applied" QA lines so no false claim survives.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, GitOps, MergeOutcome, Project, ProjectConfig, Result, ReviewSub, SimFactory,
    SimForge, SimGit, Store,
};

/// Git that behaves like the simulator, except a commit never lands — the tree
/// is always clean, so `commit_all` reports "nothing committed". `prior_work`
/// is what `branch_has_commits` reports: whether an earlier attempt already
/// put this card's commits on the branch.
struct NoCommitGit {
    prior_work: bool,
}

#[async_trait]
impl GitOps for NoCommitGit {
    async fn commit_all(&self, _: &Path, _: &str) -> Result<bool> {
        Ok(false)
    }
    async fn branch_has_commits(&self, _: &Path, _: &str) -> Result<bool> {
        Ok(self.prior_work)
    }
    // Everything else defers to the simulator's always-succeeds behavior.
    async fn create_worktree(&self, r: &Path, b: &str, p: &Path, base: &str) -> Result<()> {
        SimGit.create_worktree(r, b, p, base).await
    }
    async fn remove_worktree(&self, r: &Path, p: &Path) -> Result<()> {
        SimGit.remove_worktree(r, p).await
    }
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
    async fn push(&self, d: &Path, b: &str) -> Result<()> {
        SimGit.push(d, b).await
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

#[tokio::test]
async fn an_implement_run_that_commits_nothing_is_failed_not_advanced() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-no-commit-guard"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(NoCommitGit { prior_work: false }),
    });

    handle.send(ExecutorCommand::Start { card_id });

    // The run finishes, commits nothing, and lands in Failed — never at the gate.
    let message = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => match &c.state {
            CardState::Failed { message, .. } => Some(message.clone()),
            CardState::AwaitingReview(_) => {
                panic!("an empty run reached the review gate instead of Failed")
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(
        message.contains("without changing any files"),
        "the failure explains why: {message}"
    );

    // The final stored state is Failed, and it preserves the implement run state
    // so Retry relaunches the right phase rather than stranding the card.
    let final_state = store.get_card(card_id).unwrap().state;
    match final_state {
        CardState::Failed { previous, .. } => assert!(
            matches!(*previous, CardState::Implementing(_)),
            "Failed preserves the implement run for Retry, got {previous:?}"
        ),
        other => panic!("card should be Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_retried_run_whose_branch_already_has_the_work_advances() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-no-commit-guard-prior"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(NoCommitGit { prior_work: true }),
    });

    handle.send(ExecutorCommand::Start { card_id });

    // The run commits nothing, but the branch already carries an earlier
    // attempt's commits — the card reaches the review gate instead of looping
    // through Failed → Retry → Failed on the same finished work.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => match &c.state {
            CardState::AwaitingReview(_) => Some(()),
            CardState::Failed { message, .. } if message.contains("without changing any files") => {
                panic!("a run with prior work on the branch was demoted by the guard")
            }
            _ => None,
        },
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn a_no_op_fix_run_advances_without_claiming_fixes() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-no-commit-guard-fix"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(NoCommitGit { prior_work: true }),
    });

    // Implement (advances via the prior-work rescue), then the self-review pass
    // lands the card at the fix picker.
    handle.send(ExecutorCommand::Start { card_id });
    let verdicts = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => match &c.state {
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) => {
                Some(verdicts.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;

    // Pick a finding; the fix run commits nothing. The card must still reach
    // ReadyForPr — its branch has the work and the diff there is the re-check —
    // instead of looping through Failed → Retry → Failed.
    let verdicts: Vec<_> = verdicts
        .into_iter()
        .map(|mut v| {
            v.selected = true;
            v
        })
        .collect();
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id,
        verdicts,
        note: String::new(),
        prompt: None,
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if e.card_id == card_id => match &c.state {
            CardState::AwaitingReview(ReviewSub::ReadyForPr) => Some(()),
            CardState::Failed { message, .. } if message.contains("without changing any files") => {
                panic!("a no-op fix run was demoted by the guard")
            }
            _ => None,
        },
        _ => None,
    })
    .await;

    // The stashed "Fix applied" lines were dropped, not logged: the run changed
    // nothing, so a durable claim that it fixed anything would be false.
    let qa = store.get_card(card_id).unwrap().qa_log;
    assert!(
        !qa.iter().any(|l| l.contains("Fix applied")),
        "a no-op fix run must not claim it applied fixes: {qa:?}"
    );
}
