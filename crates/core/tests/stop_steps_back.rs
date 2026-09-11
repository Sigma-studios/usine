//! Stop on a "Request changes" run steps back to the review gate it was asked
//! from — against REAL git, since `SimGit`'s discard is a no-op: the committed
//! implementation survives on the branch, and only the stopped run's
//! uncommitted edits are thrown away.
//!
//! Before this, Stop parked the card in the starting block with its worktree
//! and branch still set, and the next Start re-cut the branch from base —
//! wiping the implementation that had already been committed.

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentEvent, AgentProvider, Card, CardConfig, CardState, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, Project, ProjectConfig, Provider,
    ProviderFactory, RealGit, Result, ReviewSub, RunConfig, RunHandle, SimForge, Store,
};

/// Keep anything the executor writes under the data dir out of the
/// developer's own Usine data.
fn isolate_data_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        std::env::set_var("USINE_DATA_DIR", dir.path());
    });
}

/// Run `git <args>` in `dir`, asserting success.
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

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// An agent that starts and keeps working until it's told to stop — its event
/// stream ends on the cancel, as a real CLI's does when its process is killed.
struct UntilCancelled;

#[async_trait::async_trait]
impl AgentProvider for UntilCancelled {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn interactive(&self) -> bool {
        false
    }
    async fn start(&self, _cfg: RunConfig) -> Result<RunHandle> {
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, mut ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        tokio::spawn(async move {
            let _ = ctl_rx.next().await;
            drop(evt_tx);
        });
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

struct UntilCancelledFactory;

impl ProviderFactory for UntilCancelledFactory {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(UntilCancelled)
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

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_change_run_keeps_the_committed_work() {
    isolate_data_dir();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "dev"]);
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "init"]);

    // The card's implementation, already committed on its own branch.
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "usine/thing",
            wt.to_str().unwrap(),
        ],
    );
    std::fs::write(wt.join("feature.txt"), "done").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "implement"]);
    let implemented = git_out(&wt, &["rev-parse", "HEAD"]);

    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", repo.clone(), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "Thing", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
    card.branch = Some("usine/thing".into());
    card.worktree_path = Some(wt.clone());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(UntilCancelledFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RealGit),
    });
    exec.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "make it blue".into(),
    });
    // The run has really started once `launch` supersedes the Agent Chat log.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { .. } if e.card_id == card_id => Some(()),
        _ => None,
    })
    .await;
    // The run is mid-write: an edit to the committed file and a brand-new one.
    std::fs::write(wt.join("feature.txt"), "half-blue").unwrap();
    std::fs::write(wt.join("scratch.txt"), "wip").unwrap();

    exec.send(ExecutorCommand::Cancel { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == card_id
                && c.state == CardState::AwaitingReview(ReviewSub::ReadyForReview) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    // The discard waits for the run to die, then resets — poll for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    while wt.join("scratch.txt").exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(
        !wt.join("scratch.txt").exists(),
        "the stopped run's new file is discarded"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("feature.txt")).unwrap(),
        "done",
        "the stopped run's edit is discarded, the committed work kept"
    );
    assert_eq!(git_out(&wt, &["rev-parse", "HEAD"]), implemented);
    assert_eq!(git_out(&repo, &["rev-parse", "usine/thing"]), implemented);
    let card = store.get_card(card_id).unwrap();
    assert_eq!(
        card.state,
        CardState::AwaitingReview(ReviewSub::ReadyForReview)
    );
    assert_eq!(card.branch.as_deref(), Some("usine/thing"));
    assert_eq!(card.worktree_path.as_deref(), Some(wt.as_path()));
}
