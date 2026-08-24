//! The per-project `auto_preview` toggle and its on-request sentinel channel:
//! with the toggle off, a write run starts NO app; the agent can request one
//! mid-run by creating `.usine-preview-request` in its worktree, which a
//! watcher bound to the run's lifetime turns into exactly one preview. The
//! sentinel itself must never reach the card's branch — its exclude is
//! registered at launch, before the run can touch it.
//!
//! The first two tests reuse `preview_reap.rs`'s shape (SimGit, a pre-seeded
//! temp worktree, a long-lived run script); the branch-hygiene test runs
//! against REAL git, where `git add -A` would actually sweep the sentinel.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, PreviewStatus, Project, ProjectConfig, RealGit, ReviewSub, SimFactory,
    SimForge, SimGit, Store,
};

const SENTINEL: &str = ".usine-preview-request";

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

/// A store with one project (long-lived `run_script`, `auto_preview` off) and
/// one card in `state` with a branch and a REAL worktree dir. `SimGit` is a
/// no-op, so the pre-seeded dir is what keeps the launch on a real path.
fn seed(state: CardState, worktree: &Path) -> (Store, uuid::Uuid) {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        run_script: Some("while true; do sleep 1; done".into()),
        auto_preview: false,
        ..ProjectConfig::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/usine-preview-request-p"), config);
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = state;
    card.branch = Some("usine/card-x".into());
    card.worktree_path = Some(worktree.to_path_buf());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    (store, card_id)
}

fn executor(store: &Store) -> (usine_core::ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

/// With `auto_preview` off and no sentinel, a full write run comes and goes
/// without a single preview event: nothing launches eagerly.
#[tokio::test]
async fn toggle_off_launches_no_preview() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        CardState::AwaitingReview(ReviewSub::ReadyForReview),
        wt.path(),
    );
    store.set_auto_review(card_id, false).unwrap();
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "polish it".into(),
    });

    // The run starts (card goes running) and parks back at ReadyForReview;
    // any preview activity in between is a failure.
    let mut saw_running = false;
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated { status, .. } if e.card_id == card_id => {
            panic!("no preview should ever start with auto_preview off, got {status:?}");
        }
        ExecutorEventKind::CardUpdated(c) if c.id == card_id => {
            if c.state.is_running() {
                saw_running = true;
                None
            } else if saw_running
                && matches!(
                    c.state,
                    CardState::AwaitingReview(ReviewSub::ReadyForReview)
                )
            {
                Some(())
            } else {
                None
            }
        }
        _ => None,
    })
    .await;
}

/// The sentinel channel: a request file in the worktree triggers exactly one
/// preview, and the watcher consumes the file.
#[tokio::test]
async fn sentinel_triggers_exactly_one_preview() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        CardState::AwaitingReview(ReviewSub::ReadyForReview),
        wt.path(),
    );
    store.set_auto_review(card_id, false).unwrap();
    let (handle, mut rx) = executor(&store);

    // Touch the sentinel before the run starts: the watcher's first tick is
    // immediate, so a pre-existing request triggers deterministically inside
    // the sim run's short life.
    std::fs::write(wt.path().join(SENTINEL), "").unwrap();
    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "polish it".into(),
    });

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated { status, .. }
            if e.card_id == card_id && *status == PreviewStatus::Running =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(
        !wt.path().join(SENTINEL).exists(),
        "the watcher should consume the sentinel"
    );
    // The park reaps the one preview; a second Running would mean the request
    // fired twice.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated { status, .. } if e.card_id == card_id => match status {
            PreviewStatus::Running => panic!("the sentinel must trigger exactly one preview"),
            PreviewStatus::Stopped => Some(()),
            _ => None,
        },
        _ => None,
    })
    .await;
}

// --- branch hygiene, against REAL git -----------------------------------

/// Point the worktrees root at a private tempdir for this test binary, so
/// card worktrees never land in the user's real data dir.
fn isolate_data_dir() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = tempfile::tempdir().expect("create data dir").keep();
        std::env::set_var("USINE_DATA_DIR", dir);
    });
}

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

fn git_stdout(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A never-consumed sentinel (no run script → no watcher) must not be swept
/// onto the branch by finalize's `git add -A` — the exclude registered at
/// launch covers it, alongside the other preview artifacts.
#[tokio::test]
async fn sentinel_never_reaches_the_branch() {
    isolate_data_dir();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("a.txt"), "a").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    git(&repo, &["branch", "-M", "dev"]);

    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        auto_preview: false,
        // No run script: no watcher, so nothing consumes the sentinel — the
        // exclude alone must keep it off the branch.
        run_script: None,
        ..ProjectConfig::default()
    };
    let project = Project::new("p", repo.clone(), config);
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, true).unwrap();
    store.set_auto_review(card_id, false).unwrap();
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RealGit),
    });

    handle.send(ExecutorCommand::Start { card_id });

    // As soon as the worktree exists, drop the sentinel plus a genuine change
    // into it — both sit there when finalize's `git add -A` runs.
    let wt = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card_id => c.worktree_path.clone(),
        _ => None,
    })
    .await;
    std::fs::write(wt.join(SENTINEL), "").unwrap();
    std::fs::write(wt.join("change.txt"), "the run's work").unwrap();

    let card = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == card_id
                && matches!(
                    c.state,
                    CardState::AwaitingReview(ReviewSub::ReadyForReview)
                ) =>
        {
            Some((**c).clone())
        }
        _ => None,
    })
    .await;

    // The real change was committed; the sentinel was not.
    let branch = card.branch.expect("card has a branch");
    let tree = git_stdout(&repo, &["ls-tree", "-r", "--name-only", &branch]);
    assert!(tree.contains("change.txt"), "the run's work is committed");
    assert!(
        !tree.contains(SENTINEL),
        "the sentinel must never land on the branch, got tree:\n{tree}"
    );
    // The repo-shared exclude carries all three preview artifacts.
    let exclude = git_stdout(&repo, &["rev-parse", "--path-format=absolute", "--git-path", "info/exclude"]);
    let exclude = std::fs::read_to_string(exclude.trim()).unwrap_or_default();
    for pattern in [".usine-preview.json", SENTINEL, ".wt-offset"] {
        assert!(
            exclude.lines().any(|l| l.trim() == pattern),
            "info/exclude should list {pattern}, got:\n{exclude}"
        );
    }
}
