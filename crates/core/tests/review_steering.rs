//! Proves a PR review can be steered: the free-form instruction the user types
//! before starting reaches the review agent's prompt, is recorded on the task,
//! and is reused by a retry that sends it back.
//!
//! Without the recording step, "Retry" from the board — which has no box of its
//! own — would silently drop the steering and re-run an unsteered review. A
//! spying provider captures the exact prompt each run receives.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, Project, ProjectConfig, Provider, ProviderFactory, Result, ReviewStatus,
    RunConfig, RunHandle, RunMode, SimFactory, SimForge, SimGit, Store,
};

/// Every prompt handed to a provider, tagged with the run mode that asked for it.
type Prompts = Arc<Mutex<Vec<(RunMode, String)>>>;

/// Wraps the simulator, recording each run's full prompt before delegating.
struct SpyProvider {
    inner: Arc<dyn AgentProvider>,
    prompts: Prompts,
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
        self.prompts
            .lock()
            .unwrap()
            .push((cfg.mode, cfg.full_prompt()));
        self.inner.start(cfg).await
    }
}

struct SpyFactory(Prompts);

impl ProviderFactory for SpyFactory {
    fn make(&self, provider: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(SpyProvider {
            inner: SimFactory.make(provider),
            prompts: self.0.clone(),
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

/// The prompts of every review run launched so far.
fn review_prompts(prompts: &Prompts) -> Vec<String> {
    prompts
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| *m == RunMode::Review)
        .map(|(_, p)| p.clone())
        .collect()
}

const STEER: &str = "Focus on the auth middleware and its error paths.";

#[tokio::test]
async fn the_users_steering_reaches_the_review_prompt_and_survives_a_retry() {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        review_contributors: vec!["octocat".into()],
        ..Default::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/review-steering"), config);
    store.upsert_project(&project).unwrap();
    let project_id = project.id;

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(prompts.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    handle.send(ExecutorCommand::ScanReviews { project_id });
    let review_id = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { tasks, .. } if !tasks.is_empty() => {
            Some(tasks[0].id)
        }
        _ => None,
    })
    .await;

    handle.send(ExecutorCommand::StartReview {
        review_id,
        guidance: format!("  {STEER}  "),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTaskUpdated(t) if t.id == review_id => match &t.status {
            ReviewStatus::AwaitingValidation { .. } => Some(()),
            _ => None,
        },
        _ => None,
    })
    .await;

    let first = review_prompts(&prompts);
    assert_eq!(first.len(), 1, "one review run so far");
    assert!(
        first[0].contains(STEER),
        "the steering must reach the agent:\n{}",
        first[0]
    );
    // It steers the pass without becoming the whole brief: the PR's identity and
    // the standing review instruction are still there.
    assert!(first[0].contains("pull request #"));
    assert!(first[0].contains("usine-review"));

    // Recorded on the task, trimmed, so a caller with no box of its own (the
    // board's Review/Retry buttons) can send it straight back.
    let stored = store.get_review_task(review_id).unwrap().guidance;
    assert_eq!(stored, STEER, "stored trimmed, ready to be sent back");

    // What "Retry" does: hand the stored steering back. It must land in the new
    // run's prompt just like the first one.
    store
        .mutate_review_task(review_id, |t| {
            t.status = ReviewStatus::ToReview;
            Ok(())
        })
        .unwrap();
    handle.send(ExecutorCommand::StartReview {
        review_id,
        guidance: stored,
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTaskUpdated(t) if t.id == review_id => match &t.status {
            ReviewStatus::AwaitingValidation { .. } => Some(()),
            _ => None,
        },
        _ => None,
    })
    .await;

    let all = review_prompts(&prompts);
    assert_eq!(all.len(), 2, "the retry launched a second review run");
    assert!(
        all[1].contains(STEER),
        "the retry must keep steering the same way:\n{}",
        all[1]
    );
}

#[tokio::test]
async fn an_unsteered_review_says_nothing_about_steering() {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        review_contributors: vec!["octocat".into()],
        ..Default::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/review-unsteered"), config);
    store.upsert_project(&project).unwrap();
    let project_id = project.id;

    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SpyFactory(prompts.clone())),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    handle.send(ExecutorCommand::ScanReviews { project_id });
    let review_id = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { tasks, .. } if !tasks.is_empty() => {
            Some(tasks[0].id)
        }
        _ => None,
    })
    .await;

    // Whitespace-only is the same as nothing: no empty "the user asked you to
    // steer this review:" block for the agent to read meaning into.
    handle.send(ExecutorCommand::StartReview {
        review_id,
        guidance: "   \n ".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTaskUpdated(t) if t.id == review_id => match &t.status {
            ReviewStatus::AwaitingValidation { .. } => Some(()),
            _ => None,
        },
        _ => None,
    })
    .await;

    let p = review_prompts(&prompts);
    assert_eq!(p.len(), 1);
    assert!(
        !p[0].contains("steer this review"),
        "no steering block without steering:\n{}",
        p[0]
    );
    assert!(store
        .get_review_task(review_id)
        .unwrap()
        .guidance
        .is_empty());
}
