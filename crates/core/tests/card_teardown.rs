//! Guards that a card's git artifacts are reaped when the card leaves the board
//! by a path *other* than merge — deleting it, or sending it back to the start.
//!
//! Both used to leak: `DeleteCard` only dropped the DB record (no worktree
//! removal, no branch delete, no run cancel), and `back_to_start` nulled the
//! worktree path even when removal failed, orphaning it. These drive the real
//! executor against real git (SimGit is a no-op, so it can't prove removal) and
//! assert the worktree and branch are actually gone afterward.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, Project, ProjectConfig, RealGit, ReviewSub, SimFactory, SimForge, Store,
};

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

/// A repo on `dev` with a `feature` branch checked out in its own worktree — the
/// state a card is in after it has implemented and committed its work.
fn repo_with_worktree(tmp: &Path) -> (PathBuf, PathBuf) {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("a.txt"), "a").unwrap();
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
    std::fs::write(wt.join("feat.txt"), "f").unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-qm", "feat"]);
    (repo, wt)
}

fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .current_dir(repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .expect("run git")
        .success()
}

/// Await events until `f` returns `Some`, failing the test on timeout.
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

/// Seed a post-implement card whose branch + worktree point at `repo`/`wt`.
fn card_on_feature(store: &Store, project_id: uuid::Uuid, wt: &Path) -> Card {
    let mut card = Card::new(project_id, "t", "d", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
    card.branch = Some("feature".into());
    card.worktree_path = Some(wt.to_path_buf());
    store.upsert_card(&card).unwrap();
    card
}

/// Deleting a card removes its worktree and branch, not just its DB record.
#[tokio::test]
async fn deleting_a_card_reaps_its_worktree_and_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, wt) = repo_with_worktree(tmp.path());
    assert!(wt.exists() && local_branch_exists(&repo, "feature"));

    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", repo.clone(), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = card_on_feature(&store, project.id, &wt);

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RealGit),
    });
    handle.send(ExecutorCommand::DeleteCard { card_id: card.id });

    // CardRemoved is emitted only after teardown + the DB delete.
    wait_for(&mut rx, |e| {
        matches!(&e.kind, ExecutorEventKind::CardRemoved if e.card_id == card.id).then_some(())
    })
    .await;

    assert!(store.get_card(card.id).is_err(), "record should be gone");
    assert!(!wt.exists(), "worktree dir should be removed");
    assert!(
        !local_branch_exists(&repo, "feature"),
        "branch should be deleted"
    );
}

/// A do-over removes the discarded attempt's worktree + branch and forgets them.
#[tokio::test]
async fn back_to_start_reaps_and_forgets_the_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, wt) = repo_with_worktree(tmp.path());

    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", repo.clone(), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = card_on_feature(&store, project.id, &wt);

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RealGit),
    });
    handle.send(ExecutorCommand::BackToStart { card_id: card.id });

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card.id => {
            matches!(c.state, CardState::StartingBlock).then_some(())
        }
        _ => None,
    })
    .await;

    let card = store.get_card(card.id).unwrap();
    assert!(matches!(card.state, CardState::StartingBlock));
    // Removal succeeded, so the card forgets the artifacts (a fresh start rebuilds).
    assert!(
        card.worktree_path.is_none(),
        "worktree path should be cleared"
    );
    assert!(card.branch.is_none(), "branch should be cleared");
    assert!(!wt.exists(), "worktree dir should be removed");
    assert!(
        !local_branch_exists(&repo, "feature"),
        "branch should be deleted"
    );
}
