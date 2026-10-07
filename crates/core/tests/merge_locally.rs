//! "Merge without a PR": squash a `ReadyForPr` card's branch into ONE commit
//! on the base branch — pushed to `origin/<base>` when the repo has an origin,
//! else applied to the local `<base>` (fast-forwarding its checkout, or moving
//! the ref by compare-and-swap when nothing has it checked out). Driven against
//! REAL git: what lands where, and that every refusal leaves both the base and
//! the card untouched.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Once};
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, ExecutorHandle, PrReviewSub, Project, ProjectConfig, RealGit, ReviewSub,
    Severity, SimFactory, SimForge, Store,
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

fn identity(dir: &Path) {
    git(dir, &["config", "user.email", "t@t.dev"]);
    git(dir, &["config", "user.name", "t"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
}

const BRANCH: &str = "usine/thing";
const SHARED: &str = "line 1\nline 2\nline 3\nline 4\nline 5\n";

/// The project repo on `main` — cloned from a bare `origin`, or a plain
/// `git init` with no remote — and the card's worktree on [`BRANCH`]. With
/// `card_work`, the branch carries two commits (`shared.rs`'s last line, then
/// a new `card.rs`), so a squash is observable as one commit replacing two.
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    origin: Option<PathBuf>,
    repo: PathBuf,
    wt: PathBuf,
}

impl Fixture {
    fn with_origin() -> Self {
        Self::new(true, true)
    }

    fn local() -> Self {
        Self::new(false, true)
    }

    fn new(origin: bool, card_work: bool) -> Self {
        isolate_data_dir();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo = root.join("repo");
        let origin = origin.then(|| {
            let origin = root.join("origin.git");
            git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
            git(
                &root,
                &[
                    "clone",
                    "-q",
                    origin.to_str().unwrap(),
                    repo.to_str().unwrap(),
                ],
            );
            origin
        });
        if origin.is_none() {
            git(&root, &["init", "-q", repo.to_str().unwrap()]);
        }
        identity(&repo);
        std::fs::write(repo.join("shared.rs"), SHARED).unwrap();
        std::fs::write(repo.join("other.rs"), "o\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        git(&repo, &["branch", "-M", "main"]);
        if origin.is_some() {
            git(&repo, &["push", "-qu", "origin", "main"]);
        }

        let wt = root.join("wt");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                BRANCH,
                wt.to_str().unwrap(),
                "main",
            ],
        );
        if card_work {
            std::fs::write(
                wt.join("shared.rs"),
                SHARED.replace("line 5", "line 5 (card)"),
            )
            .unwrap();
            git(&wt, &["commit", "-qam", "card work"]);
            std::fs::write(wt.join("card.rs"), "c\n").unwrap();
            git(&wt, &["add", "-A"]);
            git(&wt, &["commit", "-qm", "more card work"]);
        }
        Fixture {
            _tmp: tmp,
            root,
            origin,
            repo,
            wt,
        }
    }

    fn origin(&self) -> &Path {
        self.origin.as_deref().expect("an origin fixture")
    }

    fn origin_main(&self) -> String {
        git_out(self.origin(), &["rev-parse", "refs/heads/main"])
    }

    fn local_main(&self) -> String {
        git_out(&self.repo, &["rev-parse", "refs/heads/main"])
    }

    fn branch_exists(&self) -> bool {
        !git_out(
            &self.repo,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{BRANCH}"),
            ],
        )
        .is_empty()
    }

    fn scratch_left(&self) -> bool {
        git_out(&self.repo, &["worktree", "list"]).contains("-merge")
    }

    /// Commit to `main` in the project checkout itself (a local-only change),
    /// conflicting with the card's last-line edit or not.
    fn commit_on_local_main(&self, conflicting: bool) {
        let shared = if conflicting {
            SHARED.replace("line 5", "line 5 (main)")
        } else {
            SHARED.replace("line 1", "line 1 (main)")
        };
        std::fs::write(self.repo.join("shared.rs"), shared).unwrap();
        git(&self.repo, &["commit", "-qam", "main moved"]);
    }

    /// Land a commit on `origin/main` from another clone, the way a teammate
    /// would, conflicting with the card's last-line edit.
    fn land_conflict_on_origin(&self) {
        let other = self.root.join("other");
        git(
            &self.root,
            &[
                "clone",
                "-q",
                "-b",
                "main",
                self.origin().to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        identity(&other);
        std::fs::write(
            other.join("shared.rs"),
            SHARED.replace("line 5", "line 5 (upstream)"),
        )
        .unwrap();
        git(&other, &["commit", "-qam", "upstream"]);
        git(&other, &["push", "-q", "origin", "main"]);
    }
}

fn ready_for_pr() -> CardState {
    CardState::AwaitingReview(ReviewSub::ReadyForPr)
}

/// Seed a card in `state` on the fixture's branch and worktree, and spawn an
/// executor over it.
fn spawn(
    fx: &Fixture,
    state: CardState,
) -> (
    Store,
    Card,
    ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
) {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        base_branch: "main".into(),
        ci_checks: Some(false),
        ..ProjectConfig::default()
    };
    let project = Project::new("p", fx.repo.clone(), config);
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "Thing", "Do the thing.", CardConfig::default());
    card.state = state;
    card.branch = Some(BRANCH.into());
    card.worktree_path = Some(fx.wt.clone());
    store.upsert_card(&card).unwrap();
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(RealGit),
    });
    (store, card, handle, rx)
}

fn merge(handle: &ExecutorHandle, card_id: uuid::Uuid, delete_branch: bool) {
    handle.send(ExecutorCommand::MergeLocally {
        card_id,
        title: "Ship the thing".into(),
        body: "Why it matters.".into(),
        delete_branch,
    });
}

/// The first toast for the card — every outcome of a merge ends in one.
async fn toast(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
) -> (Severity, String) {
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(15), rx.next())
            .await
            .expect("timed out waiting for a toast")
            .expect("event stream closed unexpectedly");
        if let ExecutorEventKind::Toast { severity, message } = evt.kind {
            if evt.card_id == card_id {
                return (severity, message);
            }
        }
    }
}

/// The squash commit on `tip`: one commit whose parent is `parent`, with the
/// form's title + description as its message and the branch's tree.
fn assert_squashed(dir: &Path, tip: &str, parent: &str) {
    assert_eq!(git_out(dir, &["rev-parse", &format!("{tip}^")]), parent);
    assert_eq!(
        git_out(dir, &["log", "-1", "--format=%B", tip]),
        "Ship the thing\n\nWhy it matters."
    );
    assert_eq!(
        git_out(dir, &["show", &format!("{tip}:shared.rs")]),
        SHARED.replace("line 5", "line 5 (card)").trim_end()
    );
    assert_eq!(git_out(dir, &["show", &format!("{tip}:card.rs")]), "c");
}

// --- with an origin ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn squashes_onto_origin_and_finishes_the_card() {
    let fx = Fixture::with_origin();
    let before = fx.origin_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Success, "got: {msg}");
    assert!(msg.contains("origin/main"), "got: {msg}");

    assert_squashed(fx.origin(), "refs/heads/main", &before);
    assert_eq!(fx.local_main(), before, "the local main is left alone");
    let card = store.get_card(card.id).unwrap();
    assert_eq!(card.state, CardState::Done);
    assert!(card.pr.is_none());
    assert!(card.worktree_path.is_none());
    assert!(!fx.wt.exists());
    assert!(!fx.branch_exists());
    assert!(!fx.scratch_left());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflict_with_origin_changes_nothing() {
    let fx = Fixture::with_origin();
    fx.land_conflict_on_origin();
    let before = fx.origin_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Warning, "got: {msg}");
    assert!(msg.contains("Update from base"), "got: {msg}");
    assert_eq!(fx.origin_main(), before);
    assert_eq!(store.get_card(card.id).unwrap().state, ready_for_pr());
    assert!(fx.branch_exists());
    assert!(!fx.scratch_left());
}

/// With an origin, the merge goes there — never silently to the local base.
#[tokio::test(flavor = "multi_thread")]
async fn an_origin_without_the_base_is_refused() {
    let fx = Fixture::with_origin();
    git(fx.origin(), &["branch", "-m", "main", "trunk"]);
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    // A fetch doesn't prune, so drop the stale tracking ref by hand.
    git(&fx.repo, &["update-ref", "-d", "refs/remotes/origin/main"]);
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Error, "got: {msg}");
    assert!(msg.contains("origin has no `main`"), "got: {msg}");
    assert_eq!(fx.local_main(), before);
    assert_eq!(store.get_card(card.id).unwrap().state, ready_for_pr());
}

// --- without an origin ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn fast_forwards_the_checked_out_local_base() {
    let fx = Fixture::local();
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Success, "got: {msg}");
    assert!(msg.contains("into main"), "got: {msg}");
    assert_squashed(&fx.repo, "main", &before);
    // The checkout itself moved with the branch.
    assert_eq!(git_out(&fx.repo, &["rev-parse", "HEAD"]), fx.local_main());
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("card.rs")).unwrap(),
        "c\n"
    );
    assert_eq!(git_out(&fx.repo, &["status", "--porcelain"]), "");
    assert_eq!(store.get_card(card.id).unwrap().state, CardState::Done);
    assert!(!fx.branch_exists());
    assert!(!fx.scratch_left());
}

#[tokio::test(flavor = "multi_thread")]
async fn moves_a_base_checked_out_nowhere() {
    let fx = Fixture::local();
    git(&fx.repo, &["checkout", "-q", "-b", "elsewhere"]);
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Success, "got: {msg}");
    assert_squashed(&fx.repo, "main", &before);
    assert_eq!(git_out(&fx.repo, &["rev-parse", "HEAD"]), before);
    assert!(
        !fx.repo.join("card.rs").exists(),
        "the checkout is untouched"
    );
    assert_eq!(store.get_card(card.id).unwrap().state, CardState::Done);
}

/// Git refuses a fast-forward that would overwrite the checkout's uncommitted
/// edits; the refusal leaves everything where it was.
#[tokio::test(flavor = "multi_thread")]
async fn an_overlapping_uncommitted_change_in_the_checkout_is_refused() {
    let fx = Fixture::local();
    let dirty = SHARED.replace("line 3", "line 3 (wip)");
    std::fs::write(fx.repo.join("shared.rs"), &dirty).unwrap();
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Error, "got: {msg}");
    assert!(msg.contains("couldn't fast-forward"), "got: {msg}");
    assert_eq!(fx.local_main(), before);
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("shared.rs")).unwrap(),
        dirty
    );
    assert_eq!(store.get_card(card.id).unwrap().state, ready_for_pr());
    assert!(fx.wt.exists());
    assert!(!fx.scratch_left());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_conflict_changes_nothing() {
    let fx = Fixture::local();
    fx.commit_on_local_main(true);
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Warning, "got: {msg}");
    assert_eq!(fx.local_main(), before);
    assert_eq!(git_out(&fx.repo, &["status", "--porcelain"]), "");
    assert_eq!(store.get_card(card.id).unwrap().state, ready_for_pr());
    assert!(!fx.scratch_left());
}

/// Without an origin, "Update from base" — the conflict toast's way out —
/// merges the local base instead of fetching.
#[tokio::test(flavor = "multi_thread")]
async fn update_from_base_merges_the_local_base_without_an_origin() {
    let fx = Fixture::local();
    fx.commit_on_local_main(false);
    let (_store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    handle.send(ExecutorCommand::UpdateFromBase {
        card_id: card.id,
        note: None,
    });
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(30), rx.next())
            .await
            .expect("timed out waiting for the update")
            .expect("event stream closed unexpectedly");
        match evt.kind {
            ExecutorEventKind::Toast {
                severity: Severity::Error,
                message,
            } if evt.card_id == card.id => panic!("update failed: {message}"),
            ExecutorEventKind::CardUpdated(c)
                if c.id == card.id && matches!(c.state, CardState::Updating { .. }) =>
            {
                break
            }
            _ => {}
        }
    }
    let is_ancestor = Command::new("git")
        .current_dir(&fx.repo)
        .args(["merge-base", "--is-ancestor", "main", BRANCH])
        .status()
        .unwrap()
        .success();
    assert!(
        is_ancestor,
        "the local main was merged into the card's branch"
    );
}

// --- refusals and no-ops -------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_panel_is_refused() {
    let fx = Fixture::local();
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, CardState::PrReview(PrReviewSub::Idle));
    merge(&handle, card.id, true);

    let (severity, _) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Error);
    assert_eq!(fx.local_main(), before);
    assert_eq!(
        store.get_card(card.id).unwrap().state,
        CardState::PrReview(PrReviewSub::Idle)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dirty_card_worktree_is_refused() {
    let fx = Fixture::local();
    std::fs::write(fx.wt.join("card.rs"), "uncommitted\n").unwrap();
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Error, "got: {msg}");
    assert!(msg.contains("uncommitted"), "got: {msg}");
    assert_eq!(fx.local_main(), before);
    assert_eq!(store.get_card(card.id).unwrap().state, ready_for_pr());
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_to_merge_is_a_toast() {
    let fx = Fixture::new(false, false);
    let before = fx.local_main();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, true);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Info, "got: {msg}");
    assert!(msg.contains("Nothing to merge"), "got: {msg}");
    assert_eq!(fx.local_main(), before);
    assert_eq!(store.get_card(card.id).unwrap().state, ready_for_pr());
}

#[tokio::test(flavor = "multi_thread")]
async fn keeping_the_branch_still_removes_the_worktree() {
    let fx = Fixture::local();
    let (store, card, handle, mut rx) = spawn(&fx, ready_for_pr());
    merge(&handle, card.id, false);

    let (severity, msg) = toast(&mut rx, card.id).await;
    assert_eq!(severity, Severity::Success, "got: {msg}");
    assert!(fx.branch_exists());
    assert!(!fx.wt.exists());
    assert!(store.get_card(card.id).unwrap().worktree_path.is_none());
}
