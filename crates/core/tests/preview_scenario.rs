//! A card's scenario script (`.usine-scenario.sh`): stored on the card,
//! restored into the worktree, replayed after the setup script and before the
//! app on every fresh preview start — never on a relaunch, never without a
//! setup script — and a failing one fails the preview (telling an in-run agent
//! through the preview-info file).
//!
//! The preview is a real `sh` process in a real temp worktree — only the
//! agents, git, and the forge are simulated.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use std::sync::Mutex;

use futures::channel::mpsc::UnboundedSender;
use usine_core::{
    spawn_executor, AgentEvent, AgentProvider, Card, CardConfig, CardState, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, PreviewStatus, Project, ProjectConfig,
    Provider, ProviderFactory, ReviewSub, RunConfig, RunHandle, RunSub, SimFactory, SimForge,
    SimGit, Store,
};

const SCENARIO: &str = ".usine-scenario.sh";
const INFO: &str = ".usine-preview.json";
/// Writes `setup-done`; the scenario script checks for it to prove ordering.
const SETUP: &str = "touch setup-done";
/// Long-lived app that marks that it started.
const APP: &str = "touch app-started; while true; do sleep 1; done";

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

/// Wait for the card's preview to settle on `Running` or `Failed`.
async fn wait_settled(
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    card_id: uuid::Uuid,
) -> PreviewStatus {
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated { status, .. }
            if e.card_id == card_id
                && matches!(status, PreviewStatus::Running | PreviewStatus::Failed(_)) =>
        {
            Some(status.clone())
        }
        _ => None,
    })
    .await
}

fn seed(
    config: ProjectConfig,
    state: CardState,
    worktree: &Path,
    script: Option<&str>,
) -> (Store, uuid::Uuid) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/usine-preview-scenario-p"), config);
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = state;
    card.branch = Some("usine/card-x".into());
    card.worktree_path = Some(worktree.to_path_buf());
    card.scenario_script = script.map(str::to_string);
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    (store, card_id)
}

fn parked() -> CardState {
    CardState::AwaitingReview(ReviewSub::ReadyForReview)
}

fn with_setup() -> ProjectConfig {
    ProjectConfig {
        worktree_setup_script: Some(SETUP.into()),
        run_script: Some(APP.into()),
        ..ProjectConfig::default()
    }
}

fn executor(store: &Store) -> (usine_core::ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

fn count_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// The stored script is restored into a worktree that lost it, runs after
/// setup, and the app comes up. A relaunch (app only, DB kept) does not run
/// it again.
#[tokio::test]
async fn a_stored_script_is_restored_and_replayed_once_per_fresh_start() {
    let wt = tempfile::tempdir().unwrap();
    let script = "test -f setup-done || exit 9\necho ran >> scenario-runs\n";
    let (store, card_id) = seed(with_setup(), parked(), wt.path(), Some(script));
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::StartPreview { card_id });
    assert_eq!(wait_settled(&mut rx, card_id).await, PreviewStatus::Running);
    assert_eq!(
        std::fs::read_to_string(wt.path().join(SCENARIO)).unwrap(),
        script,
        "restored from the card"
    );
    assert_eq!(count_lines(&wt.path().join("scenario-runs")), 1);
    assert!(wt.path().join("app-started").exists());

    handle.send(ExecutorCommand::RelaunchPreview { card_id });
    assert_eq!(wait_settled(&mut rx, card_id).await, PreviewStatus::Running);
    assert_eq!(
        count_lines(&wt.path().join("scenario-runs")),
        1,
        "a relaunch keeps the data and must not replay the script"
    );
    handle.send(ExecutorCommand::StopPreview { card_id });
}

/// A worktree file wins over the stored copy and is saved onto the card when
/// the preview starts.
#[tokio::test]
async fn a_hand_edited_file_is_saved_at_preview_start() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(with_setup(), parked(), wt.path(), Some("echo old\n"));
    std::fs::write(wt.path().join(SCENARIO), "echo new\n").unwrap();
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::StartPreview { card_id });
    assert_eq!(wait_settled(&mut rx, card_id).await, PreviewStatus::Running);
    assert_eq!(
        store.get_card(card_id).unwrap().scenario_script.as_deref(),
        Some("echo new\n")
    );
    handle.send(ExecutorCommand::StopPreview { card_id });
}

/// A failing script fails the preview with its output and the remedy, and
/// the app never starts.
#[tokio::test]
async fn a_failing_script_fails_the_preview() {
    let wt = tempfile::tempdir().unwrap();
    let script = "echo 'inserting licensee'\necho 'unique constraint failed' >&2\nexit 1\n";
    let (store, card_id) = seed(with_setup(), parked(), wt.path(), Some(script));
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::StartPreview { card_id });
    let PreviewStatus::Failed(reason) = wait_settled(&mut rx, card_id).await else {
        panic!("the preview should fail");
    };
    for needle in [
        "scenario script",
        "exit status: 1",
        "unique constraint failed",
        "Fix or remove the card's scenario script",
    ] {
        assert!(reason.contains(needle), "missing {needle:?}: {reason}");
    }
    assert!(
        !wt.path().join("app-started").exists(),
        "the app must not start"
    );
}

/// Without a setup script there is no isolated environment to seed: the
/// script is skipped (with a warning) and the app still starts.
#[tokio::test]
async fn no_setup_script_skips_the_scenario() {
    let wt = tempfile::tempdir().unwrap();
    let config = ProjectConfig {
        run_script: Some(APP.into()),
        ..ProjectConfig::default()
    };
    let (store, card_id) = seed(config, parked(), wt.path(), Some("touch scenario-ran\n"));
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::StartPreview { card_id });
    assert_eq!(wait_settled(&mut rx, card_id).await, PreviewStatus::Running);
    assert!(!wt.path().join("scenario-ran").exists());
    assert!(wt.path().join("app-started").exists());
    let saved = store.load_transcript(card_id).unwrap().join("\n");
    assert!(saved.contains("scenario script skipped"), "{saved}");
    handle.send(ExecutorCommand::StopPreview { card_id });
}

/// An in-run start that fails tells the agent through the info file, so it
/// can fix the script instead of waiting for URLs that never come.
#[tokio::test]
async fn an_in_run_failure_is_written_to_the_info_file() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(
        with_setup(),
        CardState::Implementing(RunSub::Running),
        wt.path(),
        Some("echo 'no such table' >&2; exit 2\n"),
    );
    std::fs::write(
        wt.path().join(SCENARIO),
        "echo 'no such table' >&2; exit 2\n",
    )
    .unwrap();
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::EnsurePreview { card_id });
    assert!(matches!(
        wait_settled(&mut rx, card_id).await,
        PreviewStatus::Failed(_)
    ));
    // The file is written right after the Failed status goes out.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let info = loop {
        if let Ok(s) = std::fs::read_to_string(wt.path().join(INFO)) {
            break s;
        }
        assert!(std::time::Instant::now() < deadline, "no info file written");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let v: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(v["status"], "failed");
    assert!(
        v["error"].as_str().unwrap().contains("no such table"),
        "{info}"
    );
}

/// "Remove" drops both the stored copy and the worktree file.
#[tokio::test]
async fn clear_scenario_removes_field_and_file() {
    let wt = tempfile::tempdir().unwrap();
    let (store, card_id) = seed(with_setup(), parked(), wt.path(), Some("echo x\n"));
    std::fs::write(wt.path().join(SCENARIO), "echo x\n").unwrap();
    let (handle, mut rx) = executor(&store);

    handle.send(ExecutorCommand::ClearScenario { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) if c.id == card_id && c.scenario_script.is_none() => {
            Some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(store.get_card(card_id).unwrap().scenario_script, None);
    assert!(!wt.path().join(SCENARIO).exists());
}

/// An agent whose run stays open until the test ends it, so the in-run
/// preview machinery (eager start, request watcher) can be driven without
/// racing a simulated run's clock.
#[derive(Clone, Default)]
struct HeldRun(Arc<Mutex<Option<UnboundedSender<AgentEvent>>>>);

#[async_trait::async_trait]
impl AgentProvider for HeldRun {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn interactive(&self) -> bool {
        false
    }
    async fn start(&self, _cfg: RunConfig) -> usine_core::Result<RunHandle> {
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, _ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        *self.0.lock().unwrap() = Some(evt_tx);
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

impl ProviderFactory for HeldRun {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(self.clone())
    }
}

/// With `auto_preview` on, an eager start that fails on the scenario script
/// can be restarted by the agent once it fixed the cause: the request
/// watcher runs whatever the toggle.
#[tokio::test]
async fn a_failed_eager_start_can_be_re_requested() {
    let wt = tempfile::tempdir().unwrap();
    let config = ProjectConfig {
        auto_preview: true,
        ..with_setup()
    };
    let (store, card_id) = seed(
        config,
        parked(),
        wt.path(),
        Some("test -f fixed || exit 1\n"),
    );
    store.set_auto_review(card_id, false).unwrap();
    let agent = HeldRun::default();
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(agent.clone()),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    handle.send(ExecutorCommand::ReviseImplementation {
        card_id,
        feedback: "polish it".into(),
    });
    assert!(matches!(
        wait_settled(&mut rx, card_id).await,
        PreviewStatus::Failed(_)
    ));

    // The agent "fixes" its script and asks for a restart.
    std::fs::write(wt.path().join("fixed"), "").unwrap();
    std::fs::write(wt.path().join(".usine-preview-request"), "").unwrap();
    assert_eq!(wait_settled(&mut rx, card_id).await, PreviewStatus::Running);
    let info = std::fs::read_to_string(wt.path().join(INFO)).unwrap();
    assert!(
        info.contains("\"running\""),
        "the failed file was replaced: {info}"
    );

    // End the run; the park reaps the app.
    let tx = agent.0.lock().unwrap().take().expect("the run started");
    let _ = tx.unbounded_send(AgentEvent::Done {
        result: "done".into(),
        cost_usd: 0.0,
        usage: usine_core::Usage::default(),
    });
    drop(tx);
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::PreviewUpdated {
            status: PreviewStatus::Stopped,
            ..
        } if e.card_id == card_id => Some(()),
        _ => None,
    })
    .await;
}
