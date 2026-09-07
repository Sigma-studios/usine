//! The design phase's freshness guarantee: a read-only plan/investigate/Q&A run
//! reads a throwaway DETACHED worktree cut at a freshly fetched `origin/<base>`,
//! never the user's main checkout (which sits wherever their last pull left it).
//!
//! The first test runs against REAL git — `SimGit`'s worktree/fetch ops are
//! no-ops, so only a real repo can prove the cut point. The rest cover the
//! degradation paths: a failed fetch warns but still runs (and stays quiet for a
//! local-only repo, whose fetch always fails), and a failed worktree add fails
//! the run instead of quietly falling back to the main checkout.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, CoreError, DesignSub,
    ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, GitOps, MergeOutcome,
    Project, ProjectConfig, Provider, ProviderFactory, RealGit, Result, RunConfig, RunHandle,
    RunMode, Severity, SimFactory, SimGit, Store,
};

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;

/// Keep every worktree this file creates inside a throwaway data dir, so a real
/// `git worktree add` can never land in the developer's own Usine data.
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

/// What a run's working directory looked like the moment the provider started:
/// `(mode, dir, HEAD sha, detached?)`. Captured in `start` because the design
/// tree is torn down as soon as the run ends.
type Runs = Arc<Mutex<Vec<(RunMode, PathBuf, String, bool)>>>;

struct SpyProvider {
    inner: Arc<dyn AgentProvider>,
    runs: Runs,
}

#[async_trait::async_trait]
impl AgentProvider for SpyProvider {
    fn provider(&self) -> Provider {
        self.inner.provider()
    }
    fn interactive(&self) -> bool {
        self.inner.interactive()
    }
    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        let dir = cfg.project_dir.clone();
        let head = git_out(&dir, &["rev-parse", "HEAD"]);
        // `symbolic-ref HEAD` fails (empty output) exactly when HEAD is detached.
        let detached = git_out(&dir, &["symbolic-ref", "-q", "HEAD"]).is_empty();
        self.runs
            .lock()
            .unwrap()
            .push((cfg.mode, dir, head, detached));
        self.inner.start(cfg).await
    }
}

struct SpyFactory {
    runs: Runs,
}

impl ProviderFactory for SpyFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(SpyProvider {
            inner: SimFactory.make(provider),
            runs: self.runs.clone(),
        })
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
    pred: impl Fn(&CardState) -> bool,
) -> Card {
    wait_for(rx, |evt| match &evt.kind {
        ExecutorEventKind::CardUpdated(card) if pred(&card.state) => Some((**card).clone()),
        _ => None,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_run_reads_a_fresh_origin_cut_not_the_main_checkout() {
    isolate_data_dir();
    let tmp = tempfile::tempdir().unwrap();

    // An `upstream` whose `dev` has moved on since the user last pulled: the
    // clone's working tree AND its `origin/dev` ref both sit at the old commit.
    let upstream = tmp.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "dev"]);
    git(&upstream, &["config", "user.email", "t@t.dev"]);
    git(&upstream, &["config", "user.name", "t"]);
    git(&upstream, &["config", "commit.gpgsign", "false"]);
    std::fs::write(upstream.join("a.txt"), "a").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-qm", "old"]);

    let repo = tmp.path().join("repo");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            upstream.to_str().unwrap(),
            repo.to_str().unwrap(),
        ],
    );
    git(&repo, &["config", "user.email", "t@t.dev"]);
    git(&repo, &["config", "user.name", "t"]);
    let stale = git_out(&repo, &["rev-parse", "HEAD"]);

    std::fs::write(upstream.join("b.txt"), "b").unwrap();
    git(&upstream, &["add", "-A"]);
    git(&upstream, &["commit", "-qm", "new"]);
    let fresh = git_out(&upstream, &["rev-parse", "HEAD"]);
    assert_ne!(stale, fresh);
    // The user's own uncommitted work, which the design run must not see.
    std::fs::write(repo.join("wip.txt"), "wip").unwrap();

    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", repo.clone(), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(
        project.id,
        "Plan it",
        "What should we do?",
        CardConfig::default(),
    );
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let runs: Runs = Arc::new(Mutex::new(Vec::new()));
    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory { runs: runs.clone() }),
        forge: Arc::new(usine_core::SimForge),
        git: Arc::new(RealGit),
    });

    exec.send(ExecutorCommand::Start { card_id });
    // The sim plan run asks a mid-run question — a convenient point to inspect
    // the live scratch tree before it's torn down.
    let card = wait_for_state(&mut rx, |s| {
        matches!(s, CardState::Designing(DesignSub::Intervention(_)))
    })
    .await;

    let (mode, dir, head, detached) = runs.lock().unwrap()[0].clone();
    assert_eq!(mode, RunMode::Plan);
    assert_ne!(dir, repo, "never the user's main checkout");
    assert_eq!(
        dir.file_name().unwrap().to_string_lossy(),
        format!("{card_id}-design")
    );
    assert_eq!(
        head, fresh,
        "cut at the FRESHLY FETCHED origin/dev, not the stale local dev"
    );
    assert!(detached, "the scratch tree claims no branch");
    assert!(
        !dir.join("wip.txt").exists(),
        "the user's WIP stays out of it"
    );

    // The main checkout is untouched: same commit, same branch, WIP intact.
    assert_eq!(git_out(&repo, &["rev-parse", "HEAD"]), stale);
    assert_eq!(
        git_out(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "dev"
    );
    assert!(repo.join("wip.txt").exists());
    // Nothing is persisted on the card — the tree is throwaway.
    assert!(card.worktree_path.is_none());
    assert!(card.branch.is_none());
    assert!(dir.exists(), "the tree is live while the run is");

    // Answer the question so the run finishes normally — the scratch tree must
    // not survive it.
    exec.send(ExecutorCommand::Answer {
        card_id,
        text: "Simplicity".into(),
    });
    wait_for_state(&mut rx, |s| {
        matches!(s, CardState::Designing(DesignSub::AwaitingApproval { .. }))
    })
    .await;
    // The actor tears the tree down just after emitting the parked state, so
    // give it a moment to land.
    for _ in 0..100 {
        if !dir.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !dir.exists(),
        "the design worktree is torn down with the run"
    );
    assert!(!git_out(&repo, &["worktree", "list"]).contains("-design"));
}

/// A provider that runs the simulator but reports itself ONE-SHOT, like the real
/// CLIs: a mid-run question then tears the run down (and drops its runs-map
/// entry) instead of staying attached.
struct OneShotFactory;

struct OneShotProvider(Arc<dyn AgentProvider>);

#[async_trait::async_trait]
impl AgentProvider for OneShotProvider {
    fn provider(&self) -> Provider {
        self.0.provider()
    }
    fn interactive(&self) -> bool {
        false
    }
    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        self.0.start(cfg).await
    }
}

impl ProviderFactory for OneShotFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(OneShotProvider(SimFactory.make(provider)))
    }
}

/// Cancelling a design run parked on a question has to sweep the scratch tree:
/// the one-shot run already dropped its runs-map entry when it tore down, so no
/// actor is left to do it — and the tree deliberately survives that teardown so
/// the user's answer can resume in the same cwd.
#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_parked_design_run_sweeps_its_scratch_tree() {
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

    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", repo.clone(), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "Plan it", "What now?", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(OneShotFactory),
        forge: Arc::new(usine_core::SimForge),
        git: Arc::new(RealGit),
    });

    exec.send(ExecutorCommand::Start { card_id });
    wait_for_state(&mut rx, |s| {
        matches!(s, CardState::Designing(DesignSub::Intervention(_)))
    })
    .await;
    // The tree survives the one-shot teardown (an answer would resume in it).
    let scratch = design_dir(&repo, &format!("{card_id}-design"));
    assert!(
        scratch.exists(),
        "the parked run keeps its tree for a resume"
    );

    exec.send(ExecutorCommand::Cancel { card_id });
    wait_for_state(&mut rx, |s| matches!(s, CardState::StartingBlock)).await;
    for _ in 0..100 {
        if !scratch.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        !scratch.exists(),
        "cancelling the design phase sweeps its scratch tree"
    );
    assert!(!git_out(&repo, &["worktree", "list"]).contains("-design"));
}

/// The scratch tree's path, read off the only place it's observable from
/// outside: `git worktree list`.
fn design_dir(repo: &Path, needle: &str) -> PathBuf {
    let list = git_out(repo, &["worktree", "list", "--porcelain"]);
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if path.ends_with(needle) {
                return PathBuf::from(path);
            }
        }
    }
    panic!("no design worktree registered:\n{list}");
}

/// A `GitOps` that delegates to `SimGit` except for the one call it is armed to
/// fail. Only the trait's required methods need spelling out.
struct FlakyGit {
    fail_fetch: bool,
    fail_detached_add: bool,
    /// Whether the repo has an `origin` remote at all — a fetch failure only
    /// toasts when it does (see `a_local_only_project_does_not_toast`).
    has_origin: bool,
}

#[async_trait::async_trait]
impl GitOps for FlakyGit {
    async fn create_worktree(&self, r: &Path, b: &str, p: &Path, c: &str) -> Result<()> {
        SimGit.create_worktree(r, b, p, c).await
    }
    async fn remove_worktree(&self, r: &Path, p: &Path) -> Result<()> {
        SimGit.remove_worktree(r, p).await
    }
    async fn worktree_add_existing(&self, r: &Path, b: &str, p: &Path) -> Result<()> {
        SimGit.worktree_add_existing(r, b, p).await
    }
    async fn worktree_add_detached(&self, r: &Path, p: &Path, c: &str) -> Result<()> {
        if self.fail_detached_add {
            return Err(CoreError::other("simulated worktree add failure"));
        }
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
    async fn remote_url(&self, _d: &Path, _remote: &str) -> Result<String> {
        if self.has_origin {
            Ok("https://example.invalid/p.git".into())
        } else {
            // What `git remote get-url origin` does in a repo with no remote.
            Err(CoreError::other("No such remote 'origin'"))
        }
    }
    async fn fetch(&self, d: &Path, remote: &str) -> Result<()> {
        if self.fail_fetch {
            return Err(CoreError::other("simulated network failure"));
        }
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

/// A provider spy that just records that it was asked to start.
struct CountingFactory {
    started: Arc<AtomicBool>,
}

struct CountingProvider {
    inner: Arc<dyn AgentProvider>,
    started: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl AgentProvider for CountingProvider {
    fn provider(&self) -> Provider {
        self.inner.provider()
    }
    fn interactive(&self) -> bool {
        self.inner.interactive()
    }
    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        self.started.store(true, Ordering::SeqCst);
        self.inner.start(cfg).await
    }
}

impl ProviderFactory for CountingFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(CountingProvider {
            inner: SimFactory.make(provider),
            started: self.started.clone(),
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_fetch_toasts_but_the_run_still_starts() {
    isolate_data_dir();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/p-fetch"), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "Plan it", "go", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(CountingFactory {
            started: started.clone(),
        }),
        forge: Arc::new(usine_core::SimForge),
        git: Arc::new(FlakyGit {
            fail_fetch: true,
            fail_detached_add: false,
            has_origin: true,
        }),
    });

    exec.send(ExecutorCommand::Start { card_id });
    let msg = wait_for(&mut rx, |evt| match &evt.kind {
        ExecutorEventKind::Toast { severity, message } if *severity == Severity::Warning => {
            Some(message.clone())
        }
        _ => None,
    })
    .await;
    // The reducer drops `card_id`, so the text has to name the card itself.
    assert!(msg.contains("Plan it"), "the toast names the card: {msg}");
    assert!(msg.contains("origin"), "and says what went stale: {msg}");

    // Degradation, not failure: the run happens anyway, off the last-fetched ref.
    wait_for_state(&mut rx, |s| {
        matches!(s, CardState::Designing(DesignSub::Intervention(_)))
    })
    .await;
    assert!(started.load(Ordering::SeqCst), "the run still started");
}

/// A project with no `origin` at all fails every fetch by construction — that's
/// its steady state, not a stale cut. Toasting there would leave a persisting
/// warning to dismiss on every single run, with text that isn't even true.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_only_project_does_not_toast_on_its_always_failing_fetch() {
    isolate_data_dir();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/p-local"), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "Plan it", "go", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(CountingFactory {
            started: started.clone(),
        }),
        forge: Arc::new(usine_core::SimForge),
        git: Arc::new(FlakyGit {
            fail_fetch: true,
            fail_detached_add: false,
            has_origin: false,
        }),
    });

    exec.send(ExecutorCommand::Start { card_id });
    // Run to the same point the toasting test reaches, collecting every event.
    let mut toasts = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for the run");
        let Ok(Some(evt)) = tokio::time::timeout(Duration::from_secs(10), rx.next()).await else {
            panic!("event stream closed");
        };
        match &evt.kind {
            ExecutorEventKind::Toast { severity, message } => {
                toasts.push((*severity, message.clone()))
            }
            ExecutorEventKind::CardUpdated(c)
                if matches!(c.state, CardState::Designing(DesignSub::Intervention(_))) =>
            {
                break
            }
            _ => {}
        }
    }
    assert!(started.load(Ordering::SeqCst), "the run still started");
    assert!(
        !toasts.iter().any(|(sev, _)| *sev == Severity::Warning),
        "no fetch warning for a repo with no origin: {toasts:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncreatable_design_worktree_fails_the_run_rather_than_reading_the_main_checkout() {
    isolate_data_dir();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/p-wt"), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "Plan it", "go", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let started = Arc::new(AtomicBool::new(false));
    let (exec, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(CountingFactory {
            started: started.clone(),
        }),
        forge: Arc::new(usine_core::SimForge),
        git: Arc::new(FlakyGit {
            fail_fetch: false,
            fail_detached_add: true,
            has_origin: true,
        }),
    });

    exec.send(ExecutorCommand::Start { card_id });
    let card = wait_for_state(&mut rx, |s| s.is_failed()).await;
    assert!(
        !started.load(Ordering::SeqCst),
        "no run may read the user's main checkout as a fallback"
    );
    match &card.state {
        CardState::Failed { message, previous } => {
            assert!(message.contains("design worktree"), "{message}");
            // Parked from the running state, so Retry puts it right back.
            assert!(previous.is_running());
        }
        s => panic!("expected Failed, got {s:?}"),
    }
    let err = wait_for(&mut rx, |evt| match &evt.kind {
        ExecutorEventKind::Toast { severity, message } if *severity == Severity::Error => {
            Some(message.clone())
        }
        _ => None,
    })
    .await;
    assert!(err.contains("design worktree"), "{err}");
}
