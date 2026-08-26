//! End-to-end guards on branch adoption (`AdoptBranch`), against REAL git —
//! SimGit is a no-op, so only a real repo can prove the cut, the dirty-copy,
//! and the retire actually behave.
//!
//! The invariants under test:
//! - adoption cuts a `usine/` branch at the source tip and lands the card
//!   running in the self-review pipeline;
//! - an empty adoption (nothing beyond base, nothing dirty to include) is
//!   refused before any card or branch exists;
//! - "include uncommitted changes" COPIES — the commit lands only on the
//!   card's branch and the user's checkout stays dirty and untouched;
//! - retire-original deletes the local branch only when nothing has it
//!   checked out, and the adopted commits stay reachable either way;
//! - a remote-only ref is adoptable (and there is nothing local to retire);
//! - deleting an adopted card reaps the card's artifacts but never the
//!   (non-retired) original branch.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardState, DirtyAction, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, Project, ProjectConfig, RealGit, ReviewSub, Severity, SimFactory, SimForge,
    Store,
};

/// Point the worktrees root (and everything else path-derived) at a private
/// tempdir for this whole test binary, so adopted cards' worktrees never land
/// in the user's real data dir. Once per process: the env var is global, and
/// the per-project/per-card layout below it keeps concurrent tests apart.
fn isolate_data_dir() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = tempfile::tempdir().expect("create data dir").keep();
        std::env::set_var("USINE_DATA_DIR", dir);
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

/// A repo checked out on `dev`, with a `feature` branch one commit ahead that
/// is NOT checked out anywhere — the shape of a hand-made branch awaiting
/// adoption after its author switched back to base.
fn repo_with_feature(tmp: &Path) -> PathBuf {
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
    git(&repo, &["checkout", "-qb", "feature"]);
    std::fs::write(repo.join("feat.txt"), "f").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "feat: the hand-made work"]);
    git(&repo, &["checkout", "-q", "dev"]);
    repo
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

/// Spawn the real executor (RealGit + sim provider/forge) over a fresh store
/// holding one project rooted at `repo`.
fn executor_for(
    repo: &Path,
) -> (
    Store,
    uuid::Uuid,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
) {
    isolate_data_dir();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", repo.to_path_buf(), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RealGit),
    });
    (store, project.id, handle, rx)
}

fn adopt_cmd(
    project_id: uuid::Uuid,
    source_ref: &str,
    dirty_action: DirtyAction,
) -> ExecutorCommand {
    ExecutorCommand::AdoptBranch {
        project_id,
        source_ref: source_ref.into(),
        title: "Adopted work".into(),
        description: "Hand-made branch: adds the feature file.".into(),
        retire_original: false,
        dirty_action,
    }
}

/// Wait until `card_id` (any card when `None`) reaches a state matching `pred`,
/// returning the card.
async fn wait_for_state<F>(rx: &mut UnboundedReceiver<ExecutorEvent>, mut pred: F) -> Card
where
    F: FnMut(&Card) -> bool,
{
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if pred(c) => Some((**c).clone()),
        _ => None,
    })
    .await
}

/// The full happy path: adopt → the card lands reviewing in self-review → the
/// sim's verdicts park it at the fix picker → an empty "apply fixes" advances
/// it to Ready-for-PR. The original branch survives (retire was off).
#[tokio::test]
async fn adopt_runs_the_self_review_pipeline() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    let (store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(adopt_cmd(project_id, "feature", DirtyAction::Ignore));

    // The sim review emits two findings, parking the card at the fix picker.
    let card = wait_for_state(&mut rx, |c| {
        matches!(
            c.state,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        )
    })
    .await;
    let branch = card.branch.clone().expect("adopted card has a branch");
    assert!(branch.starts_with("usine/"), "card branch is usine-owned");
    assert!(local_branch_exists(&repo, &branch));
    assert!(
        local_branch_exists(&repo, "feature"),
        "retire was off — the original branch survives"
    );
    // The card branch carries the adopted commit.
    let log = git_stdout(&repo, &["log", "--oneline", &branch]);
    assert!(log.contains("feat: the hand-made work"));
    let wt = card
        .worktree_path
        .clone()
        .expect("adopted card has a worktree");
    assert!(wt.join("feat.txt").exists());

    // Empty fix selection → straight to Ready-for-PR (through the no-op gate).
    handle.send(ExecutorCommand::ApplySelfFixes {
        card_id: card.id,
        verdicts: Vec::new(),
        note: String::new(),
        prompt: None,
    });
    wait_for_state(&mut rx, |c| {
        matches!(c.state, CardState::AwaitingReview(ReviewSub::ReadyForPr))
    })
    .await;
    assert_eq!(store.list_cards().unwrap().len(), 1);
}

/// A branch sitting exactly on base with nothing dirty to include is refused
/// up front: no card, no `usine/` branch, just an error toast.
#[tokio::test]
async fn adopting_an_empty_branch_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    git(&repo, &["branch", "noop", "dev"]);
    let (store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(adopt_cmd(project_id, "noop", DirtyAction::Ignore));

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(
        msg.contains("nothing to adopt"),
        "refusal names the problem, got: {msg}"
    );
    assert!(store.list_cards().unwrap().is_empty(), "no card created");
    let branches = git_stdout(&repo, &["branch", "--list", "usine/*"]);
    assert!(branches.trim().is_empty(), "no usine branch created");
}

/// "Include uncommitted changes" copies: the tracked edit and the untracked
/// file are committed on the CARD's branch, while the user's checkout keeps
/// its dirty state, and the original branch gains no commit.
#[tokio::test]
async fn dirty_include_copies_and_leaves_the_checkout_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    // Check `feature` out in a linked worktree and dirty it: a tracked edit
    // plus an untracked file.
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "feature"],
    );
    std::fs::write(wt.join("feat.txt"), "edited").unwrap();
    std::fs::write(wt.join("notes.txt"), "untracked").unwrap();
    let (_store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(adopt_cmd(project_id, "feature", DirtyAction::Include));

    let card = wait_for_state(&mut rx, |c| {
        matches!(
            c.state,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        )
    })
    .await;

    // The card's worktree holds the snapshot, committed.
    let card_wt = card.worktree_path.clone().unwrap();
    assert_eq!(
        std::fs::read_to_string(card_wt.join("feat.txt")).unwrap(),
        "edited"
    );
    assert_eq!(
        std::fs::read_to_string(card_wt.join("notes.txt")).unwrap(),
        "untracked"
    );
    let branch = card.branch.clone().unwrap();
    let log = git_stdout(&repo, &["log", "--oneline", &branch]);
    assert!(log.contains("Adopt uncommitted changes from feature"));

    // The user's checkout is exactly as they left it: dirty, both files there.
    assert_eq!(
        std::fs::read_to_string(wt.join("feat.txt")).unwrap(),
        "edited"
    );
    assert!(wt.join("notes.txt").exists());
    let status = git_stdout(&wt, &["status", "--porcelain"]);
    assert!(
        status.contains("feat.txt") && status.contains("notes.txt"),
        "checkout must stay dirty, got status: {status}"
    );
    // And `feature` itself gained no commit.
    let feature_log = git_stdout(&repo, &["log", "--oneline", "feature"]);
    assert!(!feature_log.contains("Adopt uncommitted changes"));
}

/// The empty-adoption guard counts against `origin/<base>`, not the user's
/// (possibly stale) local base: a branch fully merged into the remote base has
/// nothing left to adopt, even when the local base hasn't been pulled and
/// still shows it as ahead.
#[tokio::test]
async fn a_branch_merged_into_the_remote_base_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    // `feature` becomes origin's `dev` tip — merged remotely — while the local
    // `dev` stays a commit behind, as after someone else pressed the merge
    // button and the user never pulled.
    let origin = tmp.path().join("origin.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "-q", "origin", "feature:dev", "feature"]);
    let (store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(adopt_cmd(project_id, "feature", DirtyAction::Ignore));

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(
        msg.contains("nothing to adopt"),
        "a remotely merged branch must be refused, got: {msg}"
    );
    assert!(store.list_cards().unwrap().is_empty(), "no card created");
}

/// The dirty snapshot survives the awkward cases byte-for-byte: an accented
/// untracked filename (C-quoted by `core.quotePath` unless listed with `-z`),
/// non-UTF-8 file content in the tracked patch, and untracked symlinks —
/// valid ones stay symlinks (never materialized copies), dangling ones don't
/// abort the adoption.
#[tokio::test]
async fn dirty_include_survives_symlinks_and_non_utf8() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "feature"],
    );
    // Tracked non-UTF-8 (Latin-1) content, edited in the checkout.
    std::fs::write(wt.join("latin1.txt"), b"caf\xe9\n").unwrap();
    git(&wt, &["add", "latin1.txt"]);
    git(&wt, &["commit", "-qm", "latin1 file"]);
    std::fs::write(wt.join("latin1.txt"), b"caf\xe9 edited\n").unwrap();
    // Untracked accented filename, valid symlink, dangling symlink.
    std::fs::write(wt.join("café.txt"), "accent").unwrap();
    std::os::unix::fs::symlink("feat.txt", wt.join("link.txt")).unwrap();
    std::os::unix::fs::symlink("missing.txt", wt.join("dangling.txt")).unwrap();
    let (_store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(adopt_cmd(project_id, "feature", DirtyAction::Include));

    let card = wait_for_state(&mut rx, |c| {
        matches!(
            c.state,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        )
    })
    .await;
    let card_wt = card.worktree_path.clone().unwrap();
    assert_eq!(
        std::fs::read(card_wt.join("latin1.txt")).unwrap(),
        b"caf\xe9 edited\n",
        "non-UTF-8 patch content must survive byte-for-byte"
    );
    assert_eq!(
        std::fs::read_to_string(card_wt.join("café.txt")).unwrap(),
        "accent"
    );
    let link = card_wt.join("link.txt");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "a valid symlink stays a symlink"
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap(),
        PathBuf::from("feat.txt")
    );
    assert_eq!(
        std::fs::read_link(card_wt.join("dangling.txt")).unwrap(),
        PathBuf::from("missing.txt"),
        "a dangling symlink is carried over, not an abort"
    );
}

/// Retiring deletes a branch nothing has checked out, and the adopted commits
/// stay reachable through the card's own branch.
#[tokio::test]
async fn retire_deletes_a_free_branch_and_keeps_the_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    let (_store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(ExecutorCommand::AdoptBranch {
        project_id,
        source_ref: "feature".into(),
        title: "Adopted work".into(),
        description: "desc".into(),
        retire_original: true,
        dirty_action: DirtyAction::Ignore,
    });

    let card = wait_for_state(&mut rx, |c| {
        matches!(
            c.state,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        )
    })
    .await;
    assert!(
        !local_branch_exists(&repo, "feature"),
        "the free original branch is retired"
    );
    let log = git_stdout(&repo, &["log", "--oneline", &card.branch.clone().unwrap()]);
    assert!(
        log.contains("feat: the hand-made work"),
        "the commits live on through the card branch"
    );
}

/// Retiring skips (with a warning) a branch that is checked out somewhere —
/// deleting would fail, and switching the user's checkout is off-limits.
#[tokio::test]
async fn retire_skips_a_checked_out_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    let wt = tmp.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "feature"],
    );
    let (_store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(ExecutorCommand::AdoptBranch {
        project_id,
        source_ref: "feature".into(),
        title: "Adopted work".into(),
        description: "desc".into(),
        retire_original: true,
        dirty_action: DirtyAction::Ignore,
    });

    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Warning,
            message,
        } if message.contains("left in place") => Some(()),
        _ => None,
    })
    .await;
    assert!(local_branch_exists(&repo, "feature"));
}

/// A branch that only exists on the remote is adoptable — the cut point is the
/// remote-tracking ref — and retire has nothing local to delete.
#[tokio::test]
async fn adopts_a_remote_only_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    // Publish `feature` to a bare origin, then drop the local branch: the work
    // now exists only as `origin/feature`.
    let origin = tmp.path().join("origin.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", origin.to_str().unwrap()],
    );
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["push", "-q", "origin", "dev", "feature"]);
    git(&repo, &["branch", "-qD", "feature"]);
    git(&repo, &["fetch", "-q", "origin"]);
    let (_store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(ExecutorCommand::AdoptBranch {
        project_id,
        source_ref: "origin/feature".into(),
        title: "Adopted remote work".into(),
        description: "desc".into(),
        retire_original: true,
        dirty_action: DirtyAction::Ignore,
    });

    let card = wait_for_state(&mut rx, |c| {
        matches!(
            c.state,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        )
    })
    .await;
    let log = git_stdout(&repo, &["log", "--oneline", &card.branch.clone().unwrap()]);
    assert!(log.contains("feat: the hand-made work"));
    assert!(card.worktree_path.unwrap().join("feat.txt").exists());
}

/// Deleting an adopted card reaps the card's own branch and worktree but never
/// touches the (non-retired) original branch.
#[tokio::test]
async fn deleting_an_adopted_card_leaves_the_original_branch_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = repo_with_feature(tmp.path());
    let (store, project_id, handle, mut rx) = executor_for(&repo);

    handle.send(adopt_cmd(project_id, "feature", DirtyAction::Ignore));
    let card = wait_for_state(&mut rx, |c| {
        matches!(
            c.state,
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. })
        )
    })
    .await;

    handle.send(ExecutorCommand::DeleteCard { card_id: card.id });
    wait_for(&mut rx, |e| {
        matches!(&e.kind, ExecutorEventKind::CardRemoved if e.card_id == card.id).then_some(())
    })
    .await;

    assert!(store.get_card(card.id).is_err());
    assert!(!card.worktree_path.clone().unwrap().exists());
    assert!(!local_branch_exists(&repo, &card.branch.clone().unwrap()));
    assert!(
        local_branch_exists(&repo, "feature"),
        "the original branch is not Usine's to delete"
    );
}
