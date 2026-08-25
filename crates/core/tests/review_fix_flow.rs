//! "Publish & fix": posting a maintainer review whose comments we then fix
//! ourselves, gated on the user reading the diff before anything is pushed.
//!
//! The promise ("I'm pushing a fix for this one myself") is on someone else's
//! PR the moment the review is submitted, so the invariants worth pinning are
//! about *ordering*: the push target is checked before the post, the fix run
//! commits but never pushes, and the push only happens on the user's explicit
//! command. Recording doubles make each of those observable.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, DraftComment, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, Forge, GitOps, LivePrState, MergeOutcome, Mergeable, PrInfo, PrPushTarget,
    PrSummary, Project, ProjectConfig, Result, ReviewComment, ReviewEvent, ReviewStatus,
    ReviewSummary, ReviewThread, SimFactory, SimForge, SimGit, Store,
};

/// Everything the doubles saw, in order — the test's whole assertion surface.
#[derive(Default)]
struct Log {
    /// Bodies of the inline comments submitted with the review.
    submitted_comments: Vec<String>,
    /// The review body (summary) submitted.
    submitted_body: String,
    submitted: bool,
    /// `(remote, refspec)` of every push.
    pushes: Vec<(String, String)>,
    /// Plain PR comments (the "pushed <sha>" follow-up, the retraction).
    pr_comments: Vec<String>,
    commits: usize,
    /// How many times the PR head was fetched into its local branch, and how
    /// many times the checkout was (re)built — both force-update the branch, so
    /// either one happening on a redo would wipe the fix commits.
    fetches: usize,
    worktree_adds: usize,
}

type Shared = Arc<Mutex<Log>>;

/// Git that records the outward-facing calls and models just enough history for
/// the gate: a HEAD sha that moves when a commit lands, and real directories so
/// the executor's "is the checkout still there?" guards mean something.
struct RecordingGit {
    log: Shared,
    head: Mutex<String>,
    push_fails: AtomicBool,
}

#[async_trait]
impl GitOps for RecordingGit {
    async fn worktree_add_existing(&self, _: &Path, _: &str, path: &Path) -> Result<()> {
        self.log.lock().unwrap().worktree_adds += 1;
        std::fs::create_dir_all(path)?;
        Ok(())
    }
    async fn remove_worktree(&self, _: &Path, path: &Path) -> Result<()> {
        let _ = std::fs::remove_dir_all(path);
        Ok(())
    }
    async fn commit_all(&self, _: &Path, _: &str) -> Result<bool> {
        self.log.lock().unwrap().commits += 1;
        *self.head.lock().unwrap() = "fixsha1".into();
        Ok(true)
    }
    async fn head_sha(&self, _: &Path) -> Result<String> {
        Ok(self.head.lock().unwrap().clone())
    }
    async fn push_refspec(&self, _: &Path, remote: &str, refspec: &str) -> Result<()> {
        if self.push_fails.load(Ordering::SeqCst) {
            return Err(usine_core::CoreError::other("non-fast-forward"));
        }
        self.log
            .lock()
            .unwrap()
            .pushes
            .push((remote.to_string(), refspec.to_string()));
        Ok(())
    }
    async fn remote_url(&self, _: &Path, _: &str) -> Result<String> {
        Ok("https://github.com/me/repo.git".into())
    }
    // Everything else behaves like the simulator (always succeeds, models nothing).
    async fn create_worktree(&self, r: &Path, b: &str, p: &Path, base: &str) -> Result<()> {
        SimGit.create_worktree(r, b, p, base).await
    }
    async fn worktree_add_detached(&self, r: &Path, p: &Path, c: &str) -> Result<()> {
        SimGit.worktree_add_detached(r, p, c).await
    }
    async fn fetch_pr(&self, r: &Path, n: u64, b: &str) -> Result<()> {
        self.log.lock().unwrap().fetches += 1;
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

/// Forge that records what was posted and answers the "may I push to this PR?"
/// question the way the test wants.
struct RecordingForge {
    log: Shared,
    target: PrPushTarget,
    /// When set, the PR is reported gone from the open listing and merged — the
    /// scan's reconciliation path.
    merged: AtomicBool,
}

#[async_trait]
impl Forge for RecordingForge {
    async fn submit_review(
        &self,
        _: &Path,
        _: u64,
        _: ReviewEvent,
        body: &str,
        comments: &[DraftComment],
    ) -> Result<()> {
        let mut log = self.log.lock().unwrap();
        log.submitted = true;
        log.submitted_body = body.to_string();
        log.submitted_comments = comments.iter().map(|c| c.body.clone()).collect();
        Ok(())
    }

    async fn pr_push_target(&self, _: &Path, _: u64) -> Result<Option<PrPushTarget>> {
        Ok(Some(self.target.clone()))
    }

    async fn comment_on_pr(&self, _: &Path, _: u64, body: &str) -> Result<()> {
        self.log.lock().unwrap().pr_comments.push(body.to_string());
        Ok(())
    }

    async fn list_review_prs(&self, repo: &Path, authors: &[String]) -> Result<Vec<PrSummary>> {
        if self.merged.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        SimForge.list_review_prs(repo, authors).await
    }

    async fn pr_live_state(&self, _: &Path, _: u64) -> Result<Option<LivePrState>> {
        Ok(self
            .merged
            .load(Ordering::SeqCst)
            .then_some(LivePrState::Merged))
    }

    // The rest is the simulator's canned behavior.
    async fn create_pr(
        &self,
        r: &Path,
        t: &str,
        b: &str,
        base: &str,
        head: &str,
        rev: Option<&str>,
        draft: bool,
    ) -> Result<PrInfo> {
        SimForge.create_pr(r, t, b, base, head, rev, draft).await
    }
    async fn fetch_comments(&self, r: &Path, n: u64) -> Result<Vec<ReviewComment>> {
        SimForge.fetch_comments(r, n).await
    }
    async fn list_reviewers(&self, r: &Path) -> Result<Vec<String>> {
        SimForge.list_reviewers(r).await
    }
    async fn list_submitted_reviews(&self, r: &Path, n: u64) -> Result<Vec<ReviewSummary>> {
        SimForge.list_submitted_reviews(r, n).await
    }
    async fn reply_to_comment(&self, r: &Path, n: u64, id: u64, b: &str) -> Result<()> {
        SimForge.reply_to_comment(r, n, id, b).await
    }
    async fn mark_ready(&self, r: &Path, n: u64) -> Result<()> {
        SimForge.mark_ready(r, n).await
    }
    async fn merge(&self, r: &Path, n: u64) -> Result<()> {
        SimForge.merge(r, n).await
    }
    async fn is_merged(&self, r: &Path, n: u64) -> Result<bool> {
        SimForge.is_merged(r, n).await
    }
    async fn merge_status(&self, r: &Path, n: u64) -> Result<Mergeable> {
        SimForge.merge_status(r, n).await
    }
    async fn delete_remote_branch(&self, r: &Path, b: &str) -> Result<()> {
        SimForge.delete_remote_branch(r, b).await
    }
    async fn resolve_threads(&self, r: &Path, n: u64, ids: &[u64]) -> Result<usize> {
        SimForge.resolve_threads(r, n, ids).await
    }
    async fn list_threads(&self, r: &Path, n: u64) -> Result<Vec<ReviewThread>> {
        SimForge.list_threads(r, n).await
    }
}

async fn wait_for<F, T>(rx: &mut UnboundedReceiver<ExecutorEvent>, mut f: F) -> T
where
    F: FnMut(&ExecutorEvent) -> Option<T>,
{
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(20), rx.next())
            .await
            .expect("timed out waiting for an executor event")
            .expect("event stream closed unexpectedly");
        if let Some(v) = f(&evt) {
            return v;
        }
    }
}

/// Wait for a review task to reach a status the predicate accepts.
async fn wait_status<F>(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    review_id: uuid::Uuid,
    mut f: F,
) -> ReviewStatus
where
    F: FnMut(&ReviewStatus) -> bool,
{
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTaskUpdated(t) if t.id == review_id && f(&t.status) => {
            Some(t.status.clone())
        }
        _ => None,
    })
    .await
}

struct Harness {
    store: Store,
    handle: usine_core::ExecutorHandle,
    rx: UnboundedReceiver<ExecutorEvent>,
    log: Shared,
    git: Arc<RecordingGit>,
    forge: Arc<RecordingForge>,
    _dir: tempfile::TempDir,
}

fn harness(target: PrPushTarget) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        dir.path().to_path_buf(),
        ProjectConfig {
            review_contributors: vec!["octocat".into()],
            ..Default::default()
        },
    );
    store.upsert_project(&project).unwrap();
    let log: Shared = Arc::new(Mutex::new(Log::default()));
    let git = Arc::new(RecordingGit {
        log: log.clone(),
        head: Mutex::new("basesha0".into()),
        push_fails: AtomicBool::new(false),
    });
    let forge = Arc::new(RecordingForge {
        log: log.clone(),
        target,
        merged: AtomicBool::new(false),
    });
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: forge.clone(),
        git: git.clone(),
    });
    handle.send(ExecutorCommand::ScanReviews {
        project_id: project.id,
    });
    Harness {
        store,
        handle,
        rx,
        log,
        git,
        forge,
        _dir: dir,
    }
}

fn same_repo_target() -> PrPushTarget {
    PrPushTarget {
        head_ref: "feat/cache".into(),
        cross_repo: false,
        head_repo: String::new(),
        maintainer_can_modify: false,
    }
}

/// Scan → start the review → the drafted comments, ready to publish.
async fn drafted(h: &mut Harness) -> (uuid::Uuid, Vec<DraftComment>, ReviewEvent) {
    let review_id = wait_for(&mut h.rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { tasks, .. } if !tasks.is_empty() => {
            Some(tasks[0].id)
        }
        _ => None,
    })
    .await;
    h.handle.send(ExecutorCommand::StartReview {
        review_id,
        guidance: String::new(),
    });
    let status = wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::AwaitingValidation { .. })
    })
    .await;
    let ReviewStatus::AwaitingValidation { drafts, event, .. } = status else {
        unreachable!()
    };
    (review_id, drafts, event)
}

#[tokio::test]
async fn publish_and_fix_pledges_then_gates_the_push() {
    let mut h = harness(same_repo_target());
    let (review_id, drafts, event) = drafted(&mut h).await;
    let n = drafts.len();
    assert!(n > 0, "the sim review drafts comments to fix");
    let pr_number = h.store.get_review_task(review_id).unwrap().pr_number;

    h.handle.send(ExecutorCommand::PublishReviewAndFix {
        review_id,
        drafts,
        event,
        body: "A few notes.".into(),
    });

    // The review goes out with the pledge on every comment and in the body.
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::Fixing { .. })
    })
    .await;
    {
        let log = h.log.lock().unwrap();
        assert!(log.submitted, "the review was submitted");
        assert_eq!(log.submitted_comments.len(), n);
        for body in &log.submitted_comments {
            assert!(
                body.contains("I'm pushing a fix for this one myself"),
                "every comment carries the pledge: {body}"
            );
        }
        assert!(log.submitted_body.starts_with("A few notes."));
        assert!(log.submitted_body.contains("I'm fixing the comments"));
    }

    // The agent commits in the checkout — and pushes nothing.
    let status = wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::FixReady { .. })
    })
    .await;
    match &status {
        ReviewStatus::FixReady {
            comments, base_sha, ..
        } => {
            assert_eq!(comments.len(), n, "the published comments ride along");
            assert_eq!(
                base_sha, "basesha0",
                "the gate diffs over the PR head we started from"
            );
        }
        other => panic!("expected FixReady, got {other:?}"),
    }
    {
        let log = h.log.lock().unwrap();
        assert_eq!(log.commits, 1);
        assert!(
            log.pushes.is_empty(),
            "nothing is pushed before the user approves"
        );
        assert!(log.pr_comments.is_empty());
    }
    assert!(
        h.store
            .get_review_task(review_id)
            .unwrap()
            .worktree_path
            .is_some(),
        "the checkout stays until the fix is pushed or discarded"
    );

    // The user approves: the fix lands on the PR's own head branch.
    h.handle.send(ExecutorCommand::PushReviewFix { review_id });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::Reviewed)
    })
    .await;
    let log = h.log.lock().unwrap();
    assert_eq!(
        log.pushes,
        vec![(
            "origin".to_string(),
            format!("usine-review/{pr_number}:feat/cache")
        )],
        "the fix is pushed to the PR's head, with no upstream tracking"
    );
    assert!(
        log.pr_comments
            .iter()
            .any(|c| c.contains("fixsha1") && c.contains("addressed")),
        "the author is told the fix landed: {:?}",
        log.pr_comments
    );
    let task = h.store.get_review_task(review_id).unwrap();
    assert!(task.worktree_path.is_none(), "the checkout is reaped");
}

#[tokio::test]
async fn a_fork_without_maintainer_edits_posts_nothing() {
    let mut h = harness(PrPushTarget {
        head_ref: "feat/cache".into(),
        cross_repo: true,
        head_repo: "octocat/repo".into(),
        maintainer_can_modify: false,
    });
    let (review_id, drafts, event) = drafted(&mut h).await;

    h.handle.send(ExecutorCommand::PublishReviewAndFix {
        review_id,
        drafts,
        event,
        body: "A few notes.".into(),
    });

    // The refusal is explained, and the promise is never made.
    let msg = wait_for(&mut h.rx, |e| match &e.kind {
        ExecutorEventKind::Toast { message, .. } if message.contains("maintainer edits") => {
            Some(message.clone())
        }
        _ => None,
    })
    .await;
    let author = h.store.get_review_task(review_id).unwrap().author;
    assert!(
        msg.contains(&author),
        "it names who has to enable it: {msg}"
    );
    let log = h.log.lock().unwrap();
    assert!(
        !log.submitted,
        "nothing is posted when the fix couldn't be pushed"
    );
    assert!(log.pushes.is_empty());
    assert!(matches!(
        h.store.get_review_task(review_id).unwrap().status,
        ReviewStatus::AwaitingValidation { .. }
    ));
}

#[tokio::test]
async fn a_rejected_push_keeps_the_fix_and_retries_in_place() {
    let mut h = harness(same_repo_target());
    let (review_id, drafts, event) = drafted(&mut h).await;
    h.git.push_fails.store(true, Ordering::SeqCst);

    h.handle.send(ExecutorCommand::PublishReviewAndFix {
        review_id,
        drafts,
        event,
        body: "A few notes.".into(),
    });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::FixReady { .. })
    })
    .await;

    h.handle.send(ExecutorCommand::PushReviewFix { review_id });
    let status = wait_status(&mut h.rx, review_id, |s| s.is_failed()).await;
    match &status {
        ReviewStatus::Failed { previous, message } => {
            assert!(
                matches!(**previous, ReviewStatus::FixReady { .. }),
                "the gate is preserved so the push can be retried"
            );
            assert!(
                message.contains("non-fast-forward"),
                "git's reason survives: {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(
        h.store
            .get_review_task(review_id)
            .unwrap()
            .worktree_path
            .is_some(),
        "the commits stay in the checkout"
    );

    // The obstacle clears; the same command lands the fix.
    h.git.push_fails.store(false, Ordering::SeqCst);
    h.handle.send(ExecutorCommand::PushReviewFix { review_id });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::Reviewed)
    })
    .await;
    assert_eq!(h.log.lock().unwrap().pushes.len(), 1);
}

#[tokio::test]
async fn discarding_the_fix_retracts_the_pledge() {
    let mut h = harness(same_repo_target());
    let (review_id, drafts, event) = drafted(&mut h).await;

    h.handle.send(ExecutorCommand::PublishReviewAndFix {
        review_id,
        drafts,
        event,
        body: "A few notes.".into(),
    });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::FixReady { .. })
    })
    .await;

    h.handle
        .send(ExecutorCommand::DiscardReviewFix { review_id });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::Reviewed)
    })
    .await;
    let log = h.log.lock().unwrap();
    assert!(log.pushes.is_empty(), "an abandoned fix is never pushed");
    assert!(
        log.pr_comments
            .iter()
            .any(|c| c.contains("no fix is coming")),
        "the author is told not to wait: {:?}",
        log.pr_comments
    );
    let task = h.store.get_review_task(review_id).unwrap();
    assert!(task.worktree_path.is_none(), "the checkout is reaped");
}

#[tokio::test]
async fn a_pr_merged_mid_fix_settles_as_reviewed() {
    let mut h = harness(same_repo_target());
    let (review_id, drafts, event) = drafted(&mut h).await;
    let project_id = h.store.get_review_task(review_id).unwrap().project_id;

    h.handle.send(ExecutorCommand::PublishReviewAndFix {
        review_id,
        drafts,
        event,
        body: "A few notes.".into(),
    });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::Fixing { .. })
    })
    .await;

    // The author merges while the fix is in flight. The review *was* published,
    // so "merged without review" would be a lie — only the fix is lost.
    h.forge.merged.store(true, Ordering::SeqCst);
    h.handle.send(ExecutorCommand::ScanReviews { project_id });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(
            s,
            ReviewStatus::Reviewed | ReviewStatus::MergedWithoutReview { .. }
        )
    })
    .await;
    let task = h.store.get_review_task(review_id).unwrap();
    assert!(
        matches!(task.status, ReviewStatus::Reviewed),
        "a published review settles as reviewed, got {:?}",
        task.status
    );
    assert!(task.worktree_path.is_none());
    assert!(h.log.lock().unwrap().pushes.is_empty());
}

#[tokio::test]
async fn redoing_the_fix_keeps_the_commits_and_never_refetches() {
    let mut h = harness(same_repo_target());
    let (review_id, drafts, event) = drafted(&mut h).await;
    let pr_number = h.store.get_review_task(review_id).unwrap().pr_number;

    h.handle.send(ExecutorCommand::PublishReviewAndFix {
        review_id,
        drafts,
        event,
        body: "A few notes.".into(),
    });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::FixReady { .. })
    })
    .await;
    let checkout = h.store.get_review_task(review_id).unwrap().worktree_path;
    let (fetches, adds) = {
        let log = h.log.lock().unwrap();
        (log.fetches, log.worktree_adds)
    };

    // The user sends it back with feedback. The whole point of the redo is that
    // it runs *in the same checkout*: re-fetching the PR head force-updates the
    // local branch, which would throw the fix commit away — and the published
    // review already promises that fix.
    h.handle.send(ExecutorCommand::ReviseReviewFix {
        review_id,
        note: "keep the helper private".into(),
    });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::Fixing { .. })
    })
    .await;
    let status = wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::FixReady { .. })
    })
    .await;

    {
        let log = h.log.lock().unwrap();
        assert_eq!(
            log.fetches, fetches,
            "the redo never re-fetches the PR head"
        );
        assert_eq!(
            log.worktree_adds, adds,
            "the redo reuses the checkout rather than rebuilding it"
        );
        assert_eq!(log.commits, 2, "the redo commits on top of the first pass");
        assert!(log.pushes.is_empty(), "a redo still pushes nothing");
    }
    match &status {
        ReviewStatus::FixReady { base_sha, .. } => assert_eq!(
            base_sha, "basesha0",
            "the gate still diffs over the PR head the first pass started from"
        ),
        other => panic!("expected FixReady, got {other:?}"),
    }
    assert_eq!(
        h.store.get_review_task(review_id).unwrap().worktree_path,
        checkout,
        "the same checkout carries the fix across the redo"
    );

    // And the redone fix pushes like any other.
    h.handle.send(ExecutorCommand::PushReviewFix { review_id });
    wait_status(&mut h.rx, review_id, |s| {
        matches!(s, ReviewStatus::Reviewed)
    })
    .await;
    assert_eq!(
        h.log.lock().unwrap().pushes,
        vec![(
            "origin".to_string(),
            format!("usine-review/{pr_number}:feat/cache")
        )]
    );
}
