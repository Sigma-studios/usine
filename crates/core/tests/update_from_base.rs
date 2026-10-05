//! "Update from base": merge `origin/<base>` into a card's branch and ALWAYS
//! run an agent that adapts the card's work to what landed — before the PR
//! (back to `ReadyForPr`, which re-runs validation) and after it (the push
//! re-runs CI). Driven against REAL git wherever the branch's history is the
//! point (what gets merged, pushed, or rolled back on Cancel), and against a
//! scripted double where the point is the pre-commit gate.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentEvent, AgentProvider, Card, CardConfig, CardState, CheckStatus, CoreError,
    DraftComment, ExchangeKind, ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind,
    ExecutorHandle, Forge, GitOps, MergeOutcome, Mergeable, PrInfo, PrReviewSub, PrSummary,
    Project, ProjectConfig, Provider, ProviderFactory, RealGit, ReviewComment, ReviewEvent,
    ReviewScope, ReviewSub, ReviewSummary, RunConfig, RunHandle, RunMode, RunSub, Severity,
    SimForge, Store, UpstreamChanges,
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

/// A bare `origin`, the project clone on `main`, and the card's worktree on
/// [`BRANCH`] with one commit touching `shared.rs` (its last line).
struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    origin: PathBuf,
    repo: PathBuf,
    wt: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        isolate_data_dir();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let origin = root.join("origin.git");
        git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        let repo = root.join("repo");
        git(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                repo.to_str().unwrap(),
            ],
        );
        identity(&repo);
        std::fs::write(repo.join("shared.rs"), SHARED).unwrap();
        std::fs::write(repo.join("other.rs"), "o\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "base"]);
        git(&repo, &["branch", "-M", "main"]);
        git(&repo, &["push", "-qu", "origin", "main"]);

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
        std::fs::write(
            wt.join("shared.rs"),
            SHARED.replace("line 5", "line 5 (card)"),
        )
        .unwrap();
        git(&wt, &["commit", "-qam", "card work"]);
        Fixture {
            _tmp: tmp,
            root,
            origin,
            repo,
            wt,
        }
    }

    /// Land commits on `origin/main` from another clone, the way a teammate
    /// would: "other" (other.rs), then "rename helper" (shared.rs line 1, or
    /// its last line when `conflicting`, which the card also edited).
    fn land_upstream(&self, conflicting: bool) {
        let other = self.root.join("other");
        git(
            &self.root,
            &[
                "clone",
                "-q",
                "-b",
                "main",
                self.origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        identity(&other);
        std::fs::write(other.join("other.rs"), "o\nupstream\n").unwrap();
        git(&other, &["commit", "-qam", "other"]);
        let shared = if conflicting {
            SHARED.replace("line 5", "line 5 (upstream)")
        } else {
            SHARED.replace("line 1", "line 1 (renamed)")
        };
        std::fs::write(other.join("shared.rs"), shared).unwrap();
        git(&other, &["commit", "-qam", "rename helper"]);
        git(&other, &["push", "-q", "origin", "main"]);
    }

    fn head(&self) -> String {
        git_out(&self.wt, &["rev-parse", "HEAD"])
    }

    fn remote_branch(&self) -> String {
        git_out(
            &self.origin,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{BRANCH}"),
            ],
        )
    }

    fn merge_in_progress(&self) -> bool {
        Command::new("git")
            .current_dir(&self.wt)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .output()
            .unwrap()
            .status
            .success()
    }
}

/// A forge that disables the comment poll (its first tick would race the
/// assertions on the state the update itself writes) and reports `status`
/// for mergeability; everything else defers to the simulator.
struct QuietForge {
    status: Mergeable,
}

#[async_trait]
impl Forge for QuietForge {
    async fn merge(&self, _: &Path, _: u64) -> usine_core::Result<()> {
        Err(CoreError::forge("merging is not under test"))
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
    async fn fetch_comments(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewComment>> {
        Err(CoreError::forge("comment poll disabled in this test"))
    }
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

type Prompts = Arc<Mutex<Vec<(RunMode, String)>>>;

/// A one-shot provider that ends each run with the next scripted message
/// (`"done"` once the script runs out), recording every prompt — or, with
/// `hold`, one that keeps running until cancelled.
#[derive(Clone)]
struct Scripted {
    results: Arc<Mutex<Vec<String>>>,
    prompts: Prompts,
    starts: Arc<AtomicUsize>,
    hold: bool,
}

impl Scripted {
    fn new(results: &[&str]) -> Self {
        Scripted {
            results: Arc::new(Mutex::new(results.iter().map(|s| s.to_string()).collect())),
            prompts: Arc::default(),
            starts: Arc::default(),
            hold: false,
        }
    }
    fn until_cancelled() -> Self {
        Scripted {
            hold: true,
            ..Scripted::new(&[])
        }
    }
    fn fix_prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| *m == RunMode::ApplyFixes)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

#[async_trait]
impl AgentProvider for Scripted {
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
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, mut ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        if self.hold {
            tokio::spawn(async move {
                let _ = ctl_rx.next().await;
                drop(evt_tx);
            });
        } else {
            let result = {
                let mut rs = self.results.lock().unwrap();
                if rs.is_empty() {
                    "done".to_string()
                } else {
                    rs.remove(0)
                }
            };
            let _ = evt_tx.unbounded_send(AgentEvent::Done {
                result,
                cost_usd: 0.0,
                usage: usine_core::Usage::default(),
            });
        }
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

impl ProviderFactory for Scripted {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(self.clone())
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

async fn wait_for_state(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
    want: impl Fn(&CardState) -> bool,
) {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card_id && want(&c.state) => Some(()),
        _ => None,
    })
    .await
}

async fn poll_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn pr() -> PrInfo {
    PrInfo {
        number: 7,
        url: "https://github.com/example/repo/pull/7".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: None,
        reviewer_recorded: false,
    }
}

/// Seed a card in `state` on the fixture's branch and worktree, and spawn an
/// executor over it.
fn spawn(
    fx_repo: &Path,
    wt: &Path,
    state: CardState,
    with_pr: bool,
    configure: impl FnOnce(&mut ProjectConfig),
    providers: Arc<dyn ProviderFactory>,
    git: Arc<dyn GitOps>,
) -> (
    Store,
    Card,
    ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
) {
    let store = Store::open_in_memory().unwrap();
    let mut config = ProjectConfig {
        base_branch: "main".into(),
        ci_checks: Some(false),
        ..ProjectConfig::default()
    };
    configure(&mut config);
    let project = Project::new("p", fx_repo.to_path_buf(), config);
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "Thing", "Do the thing.", CardConfig::default());
    card.state = state;
    card.branch = Some(BRANCH.into());
    card.worktree_path = Some(wt.to_path_buf());
    if with_pr {
        card.pr = Some(pr());
    }
    store.upsert_card(&card).unwrap();
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers,
        forge: Arc::new(QuietForge {
            status: Mergeable::Conflicting,
        }),
        git,
    });
    (store, card, handle, rx)
}

fn update(handle: &ExecutorHandle, card_id: uuid::Uuid, note: Option<&str>) {
    handle.send(ExecutorCommand::UpdateFromBase {
        card_id,
        note: note.map(str::to_string),
    });
}

fn ready_for_pr() -> CardState {
    CardState::AwaitingReview(ReviewSub::ReadyForPr)
}

// --- before the PR -----------------------------------------------------------

/// The brief carries what landed (with the overlapping file first) and the
/// note; the card re-enters the validation gate, and nothing is pushed — a
/// pre-PR branch stays local.
#[tokio::test(flavor = "multi_thread")]
async fn a_clean_update_before_the_pr_briefs_the_agent_and_revalidates() {
    let fx = Fixture::new();
    fx.land_upstream(false);
    let agent = Scripted::new(&["Adapted the call site to the renamed helper."]);
    let (store, card, handle, mut rx) = spawn(
        &fx.repo,
        &fx.wt,
        ready_for_pr(),
        false,
        |c| c.validate_script = Some("true".into()),
        Arc::new(agent.clone()),
        Arc::new(RealGit),
    );
    update(&handle, card.id, Some("watch the helper rename"));

    wait_for_state(&mut rx, card.id, |s| {
        matches!(s, CardState::AwaitingReview(ReviewSub::Validating { .. }))
    })
    .await;
    wait_for_state(&mut rx, card.id, |s| *s == ready_for_pr()).await;

    let prompt = agent.fix_prompts().pop().expect("an update run");
    assert!(prompt.contains("rename helper") && prompt.contains("- other\n"));
    let shared = prompt
        .find("- shared.rs (also changed by this branch)")
        .expect("the overlap is flagged");
    let other = prompt.find("- other.rs\n").expect("the rest is listed");
    assert!(shared < other, "the overlap comes first");
    assert!(prompt.contains("watch the helper rename"));
    assert!(
        fx.remote_branch().is_empty(),
        "nothing is pushed before a PR"
    );
    assert!(!fx.merge_in_progress());

    let log = store.get_answers(card.id).unwrap();
    let last = log.exchanges.last().expect("a chat entry");
    assert_eq!(last.kind, ExchangeKind::Change);
    assert_eq!(last.question, "watch the helper rename");
    assert_eq!(store.get_update_origin(card.id).unwrap(), None);
    assert_eq!(store.get_fix_extra(card.id).unwrap(), None);
}

/// Without a note, the recap still lands in the Agent Chat log — as an entry
/// of its own, titled after the base.
#[tokio::test(flavor = "multi_thread")]
async fn an_update_without_a_note_logs_its_own_chat_entry() {
    let fx = Fixture::new();
    fx.land_upstream(false);
    let agent = Scripted::new(&["Adapted the call site."]);
    let (store, card, handle, mut rx) = spawn(
        &fx.repo,
        &fx.wt,
        ready_for_pr(),
        false,
        |_| {},
        Arc::new(agent.clone()),
        Arc::new(RealGit),
    );
    update(&handle, card.id, None);
    wait_for_state(&mut rx, card.id, |s| {
        matches!(s, CardState::Updating { .. })
    })
    .await;
    wait_for_state(&mut rx, card.id, |s| *s == ready_for_pr()).await;

    let log = store.get_answers(card.id).unwrap();
    assert!(log.exchanges.iter().all(|e| e.kind != ExchangeKind::Change));
    let last = log.exchanges.last().expect("a chat entry");
    assert_eq!(last.kind, ExchangeKind::Update);
    assert_eq!(last.question, "Update from main");
    assert_eq!(last.answer, "Adapted the call site.");
    assert!(!agent.fix_prompts()[0].contains("What to look out for"));
}

/// Nothing landed upstream: a toast, and nothing else happens.
#[tokio::test(flavor = "multi_thread")]
async fn already_up_to_date_is_a_toast_and_no_run() {
    let fx = Fixture::new();
    let agent = Scripted::new(&[]);
    let (store, card, handle, mut rx) = spawn(
        &fx.repo,
        &fx.wt,
        ready_for_pr(),
        false,
        |_| {},
        Arc::new(agent.clone()),
        Arc::new(RealGit),
    );
    let head = fx.head();
    update(&handle, card.id, Some("anything"));
    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Info,
            message,
        } if e.card_id == card.id => Some(message.clone()),
        ExecutorEventKind::CardUpdated(c) if c.id == card.id && c.state != ready_for_pr() => {
            panic!("an up-to-date branch must not move the card: {:?}", c.state)
        }
        _ => None,
    })
    .await;
    assert!(msg.contains("Already up to date"), "got: {msg}");
    assert_eq!(agent.starts.load(Ordering::SeqCst), 0);
    assert_eq!(store.get_card(card.id).unwrap().state, ready_for_pr());
    assert_eq!(store.get_update_origin(card.id).unwrap(), None);
    assert_eq!(fx.head(), head);
}

// --- after the PR ------------------------------------------------------------

/// "Nothing to adapt" is an allowed outcome: the merge commit itself is pushed
/// (re-running CI), and the card returns to the merge gate.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_to_adapt_after_the_pr_still_pushes_the_merge() {
    let fx = Fixture::new();
    git(&fx.wt, &["push", "-qu", "origin", BRANCH]);
    fx.land_upstream(false);
    let agent = Scripted::new(&[
        "Nothing to adapt because the rename doesn't reach this code. \
                                 Checked every call site.",
    ]);
    let (store, card, handle, mut rx) = spawn(
        &fx.repo,
        &fx.wt,
        CardState::ReadyToMerge,
        true,
        |c| c.ci_checks = Some(true),
        Arc::new(agent.clone()),
        Arc::new(RealGit),
    );
    store
        .mutate_card(card.id, |c| {
            c.checks = CheckStatus::Passing;
            Ok(())
        })
        .unwrap();
    update(&handle, card.id, None);

    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Success,
            message,
        } if e.card_id == card.id => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(msg.contains("nothing to adapt"), "got: {msg}");
    assert!(
        msg.contains("the rename doesn't reach this code."),
        "got: {msg}"
    );
    assert!(
        !msg.contains("Checked every call site"),
        "only the first sentence: {msg}"
    );
    wait_for_state(&mut rx, card.id, |s| *s == CardState::ReadyToMerge).await;

    let head = fx.head();
    assert_eq!(fx.remote_branch(), head, "the merge commit is published");
    assert!(
        git_out(&fx.wt, &["log", "-1", "--format=%P"]).contains(' '),
        "HEAD is the merge commit"
    );
    let after = store.get_card(card.id).unwrap();
    assert_eq!(after.checks, CheckStatus::Pending);
    assert_eq!(after.mergeable, Mergeable::Unknown);
    let last = store.get_answers(card.id).unwrap().exchanges.pop().unwrap();
    assert_eq!(last.kind, ExchangeKind::Update);
}

// --- the pre-commit gate (scripted git) ----------------------------------------

/// Git whose base merge always stops on a conflict in `src/lib.rs`, with one
/// upstream commit; `unresolved` is what the worktree holds after the agent.
#[derive(Default)]
struct ConflictingGit {
    unresolved: Vec<String>,
    committed: Arc<Mutex<bool>>,
    pushed: Arc<Mutex<bool>>,
}

#[async_trait]
impl GitOps for ConflictingGit {
    async fn upstream_changes(&self, _: &Path, _: &str) -> usine_core::Result<UpstreamChanges> {
        Ok(UpstreamChanges {
            subjects: vec!["rename helper".into()],
            files: vec!["src/lib.rs".into()],
            overlap: vec!["src/lib.rs".into()],
        })
    }
    async fn merge_ref(&self, _: &Path, _: &str) -> usine_core::Result<MergeOutcome> {
        Ok(MergeOutcome::Conflicted(vec!["src/lib.rs".into()]))
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

/// Regression: a conflict run started from `PrReview(Idle)` used to skip the
/// pre-commit gate (it was keyed off the `ApplyingFixes` sub-state), so
/// leftover markers were committed and pushed to the PR. Through either entry
/// point, the gate now faults the run instead.
#[tokio::test(flavor = "multi_thread")]
async fn leftover_markers_from_the_pr_gate_fault_instead_of_publishing() {
    for via_resolve in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let git = Arc::new(ConflictingGit {
            unresolved: vec!["src/lib.rs".into()],
            ..Default::default()
        });
        let (store, card, handle, mut rx) = spawn(
            Path::new("/tmp/usine-update-from-base"),
            tmp.path(),
            CardState::PrReview(PrReviewSub::Idle),
            true,
            |_| {},
            Arc::new(Scripted::new(&["All resolved."])),
            git.clone(),
        );
        if via_resolve {
            handle.send(ExecutorCommand::ResolveConflicts { card_id: card.id });
        } else {
            update(&handle, card.id, None);
        }
        wait_for_state(&mut rx, card.id, CardState::is_failed).await;

        let CardState::Failed { previous, message } = store.get_card(card.id).unwrap().state else {
            unreachable!()
        };
        assert!(
            matches!(*previous, CardState::Updating { .. }),
            "via_resolve={via_resolve}: {previous:?}"
        );
        assert!(message.contains("src/lib.rs"), "got: {message}");
        assert!(!*git.committed.lock().unwrap(), "markers must not commit");
        assert!(!*git.pushed.lock().unwrap(), "nor be pushed");
    }
}

const ASKING: &str = "Adapted src/a.rs. src/lib.rs needs your call.\n\n\
```usine-questions\n\
[{\"question\":\"Keep the old helper name?\",\"options\":[\"Keep\",\"Rename\"]}]\n\
```\n";

/// A question parks the update with nothing committed; the answer restarts
/// the run with the same brief, and the card returns to the PR gate.
#[tokio::test(flavor = "multi_thread")]
async fn a_question_parks_the_update_and_the_answer_finishes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let git = Arc::new(ConflictingGit::default());
    let agent = Scripted::new(&[ASKING, "adapted"]);
    let (store, card, handle, mut rx) = spawn(
        Path::new("/tmp/usine-update-from-base"),
        tmp.path(),
        CardState::PrReview(PrReviewSub::Idle),
        true,
        |_| {},
        Arc::new(agent.clone()),
        git.clone(),
    );
    update(&handle, card.id, None);
    wait_for_state(&mut rx, card.id, |s| {
        matches!(
            s,
            CardState::Updating {
                sub: RunSub::Intervention(_),
                ..
            }
        )
    })
    .await;
    let iv = store
        .get_card(card.id)
        .unwrap()
        .state
        .intervention()
        .cloned()
        .unwrap();
    assert_eq!(iv.question, "Keep the old helper name?");
    assert!(
        !*git.committed.lock().unwrap(),
        "nothing committed while asking"
    );
    assert!(!*git.pushed.lock().unwrap());

    handle.send(ExecutorCommand::Answer {
        card_id: card.id,
        text: "Rename".into(),
    });
    wait_for_state(&mut rx, card.id, |s| {
        *s == CardState::PrReview(PrReviewSub::Idle)
    })
    .await;

    let prompts = agent.fix_prompts();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[1].contains("## Update from the base branch"));
    assert!(prompts[1].contains("Keep the old helper name?") && prompts[1].contains("Rename"));
    assert!(*git.committed.lock().unwrap() && *git.pushed.lock().unwrap());
}

// --- cancel rolls the merge back ------------------------------------------------

/// Start an update against a holding agent and wait until it is really running.
async fn start_held_update(
    fx: &Fixture,
    state: CardState,
) -> (
    Store,
    Card,
    ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
) {
    let agent = Scripted::until_cancelled();
    let (store, card, handle, mut rx) = spawn(
        &fx.repo,
        &fx.wt,
        state,
        false,
        |_| {},
        Arc::new(agent.clone()),
        Arc::new(RealGit),
    );
    update(&handle, card.id, Some("note"));
    wait_for_state(&mut rx, card.id, |s| s.is_running()).await;
    poll_until("the update run to start", || {
        agent.starts.load(Ordering::SeqCst) > 0
    })
    .await;
    (store, card, handle, rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_clean_update_rewinds_the_merge_commit() {
    let fx = Fixture::new();
    fx.land_upstream(false);
    let pre = fx.head();
    let (store, card, handle, mut rx) = start_held_update(&fx, ready_for_pr()).await;
    assert_ne!(fx.head(), pre, "the clean merge was committed");

    handle.send(ExecutorCommand::Cancel { card_id: card.id });
    wait_for_state(&mut rx, card.id, |s| *s == ready_for_pr()).await;
    poll_until("HEAD to rewind", || fx.head() == pre).await;
    assert!(!fx.merge_in_progress());
    assert_eq!(store.get_fix_extra(card.id).unwrap(), None);
    poll_until("the origin to clear", || {
        store.get_update_origin(card.id).unwrap().is_none()
    })
    .await;
    assert_eq!(store.get_pending_change(card.id).unwrap(), None);
    assert!(fx.remote_branch().is_empty(), "the remote is untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_conflicted_update_aborts_the_merge() {
    let fx = Fixture::new();
    fx.land_upstream(true);
    let pre = fx.head();
    let (_store, card, handle, mut rx) = start_held_update(&fx, ready_for_pr()).await;
    assert!(fx.merge_in_progress());

    handle.send(ExecutorCommand::Cancel { card_id: card.id });
    wait_for_state(&mut rx, card.id, |s| *s == ready_for_pr()).await;
    poll_until("the merge to be aborted", || !fx.merge_in_progress()).await;
    assert_eq!(fx.head(), pre);
    assert!(git_out(&fx.wt, &["status", "--porcelain"]).is_empty());
}

/// Once the merge commit is on the remote, rewinding it would rewrite shared
/// history: Cancel keeps it, says so, and never force-pushes.
#[tokio::test(flavor = "multi_thread")]
async fn a_published_merge_is_kept_on_cancel() {
    let fx = Fixture::new();
    fx.land_upstream(false);
    let (_store, card, handle, mut rx) = start_held_update(&fx, ready_for_pr()).await;
    let merged = fx.head();
    git(
        &fx.wt,
        &["push", "-q", "origin", &format!("HEAD:refs/heads/{BRANCH}")],
    );
    git(&fx.wt, &["fetch", "-q", "origin"]);

    handle.send(ExecutorCommand::Cancel { card_id: card.id });
    let msg = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Toast {
            severity: Severity::Warning,
            message,
        } if e.card_id == card.id => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(msg.contains("Kept the merge"), "got: {msg}");
    assert_eq!(fx.head(), merged);
    assert_eq!(fx.remote_branch(), merged, "nothing was force-pushed");
}

/// Abandoning a faulted update (Cancel from `Failed`) rolls the merge back
/// and returns the card to the gate it was updated from.
#[tokio::test(flavor = "multi_thread")]
async fn abandoning_a_faulted_update_rolls_it_back() {
    let fx = Fixture::new();
    fx.land_upstream(true);
    let pre = fx.head();
    // The agent claims success but leaves the markers: the gate faults it.
    let (store, card, handle, mut rx) = spawn(
        &fx.repo,
        &fx.wt,
        CardState::PrReview(PrReviewSub::Idle),
        false,
        |_| {},
        Arc::new(Scripted::new(&["All resolved."])),
        Arc::new(RealGit),
    );
    update(&handle, card.id, None);
    // The fault and the release of the action's claim race each other; a
    // Cancel sent while the claim is held would be dropped.
    let (mut failed, mut released) = (false, false);
    while !(failed && released) {
        wait_for(&mut rx, |e| match &e.kind {
            ExecutorEventKind::CardUpdated(c) if c.id == card.id && c.state.is_failed() => {
                failed = true;
                Some(())
            }
            ExecutorEventKind::CardBusy { busy: false } if e.card_id == card.id => {
                released = true;
                Some(())
            }
            _ => None,
        })
        .await;
    }
    assert!(fx.merge_in_progress());

    handle.send(ExecutorCommand::Cancel { card_id: card.id });
    wait_for_state(&mut rx, card.id, |s| {
        *s == CardState::PrReview(PrReviewSub::Idle)
    })
    .await;
    poll_until("the merge to be rolled back", || !fx.merge_in_progress()).await;
    assert_eq!(fx.head(), pre);
    poll_until("the origin to clear", || {
        store.get_update_origin(card.id).unwrap().is_none()
    })
    .await;
}
