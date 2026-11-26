//! The Claude API-credits fallback, end to end through the executor.
//!
//! A card run cut off by the plan's usage limit (`AgentEvent::LimitReached`)
//! continues on the saved API key — same card, no Failed flicker, resuming
//! its own conversation — and every run until the reset bills the key. So do
//! PR reviews. A plan window reported nearly full (`AgentEvent::PlanUsage`)
//! moves the next runs onto the key before any is cut off. Without a key the
//! limit fails the run as before, with a hint about the fallback.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentEvent, AgentProvider, Card, CardConfig, CardState, ClaudeAuth, DesignSub,
    ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, PlanWindow, PlanWindowUsage,
    Project, ProjectConfig, Provider, ProviderFactory, ReviewStatus, RunConfig, RunHandle,
    SecretString, SimForge, SimGit, Store,
};

const PLAN: &str = "## Plan\n\nA plan long enough to clear the abandoned-plan floor: it names \
    the files to touch, the order to touch them in, and how the change is verified once it \
    lands, so the executor promotes it for approval rather than treating it as a bailed turn.";

/// Hits the plan's limit on a plan-billed run; finishes on a key-billed one.
/// Records each run's config.
///
/// With `near_limit`, a plan-billed run instead reports the session window
/// 97% used and finishes.
#[derive(Clone, Default)]
struct LimitedProvider {
    runs: Arc<Mutex<Vec<RunConfig>>>,
    near_limit: bool,
}

#[async_trait::async_trait]
impl AgentProvider for LimitedProvider {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    async fn start(&self, cfg: RunConfig) -> usine_core::Result<RunHandle> {
        let on_credits = cfg.claude_auth.is_api_key();
        self.runs.lock().unwrap().push(cfg);
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, _ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        if self.near_limit && !on_credits {
            let _ = evt_tx.unbounded_send(AgentEvent::PlanUsage {
                windows: vec![PlanWindowUsage {
                    window: PlanWindow::Session,
                    utilization: 0.97,
                    resets_at: Some(now_secs() + 3_600),
                }],
            });
        }
        let _ = evt_tx.unbounded_send(if on_credits || self.near_limit {
            AgentEvent::Done {
                result: PLAN.into(),
                cost_usd: 0.42,
                usage: usine_core::Usage::default(),
            }
        } else {
            AgentEvent::LimitReached {
                resets_at: None,
                message: "You've hit your session limit · resets 3pm".into(),
            }
        });
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

impl ProviderFactory for LimitedProvider {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(self.clone())
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
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

/// Start a plan run on a fresh card; return the state it settles in and the
/// runs the provider saw.
async fn run_card(with_key: bool) -> (CardState, Vec<RunConfig>) {
    run_cards(with_key, LimitedProvider::default(), 1).await
}

/// Start `n` plan runs one after the other on fresh cards; return the state
/// the last settles in and the runs the provider saw.
async fn run_cards(
    with_key: bool,
    provider: LimitedProvider,
    n: usize,
) -> (CardState, Vec<RunConfig>) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-api-credits"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let cards: Vec<Card> = (0..n)
        .map(|i| {
            Card::new(
                project.id,
                format!("c{i}"),
                "Do the thing.",
                CardConfig::default(),
            )
        })
        .collect();
    for card in &cards {
        store.upsert_card(card).unwrap();
    }

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(provider.clone()),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    if with_key {
        handle.send(ExecutorCommand::SetAnthropicKey {
            key: SecretString::new("sk-test"),
        });
        wait_for(&mut rx, |e| match &e.kind {
            ExecutorEventKind::BillingUpdated(b) if b.fallback_configured => Some(()),
            _ => None,
        })
        .await;
    }
    let mut state = None;
    for card in &cards {
        let card_id = card.id;
        handle.send(ExecutorCommand::Start { card_id });
        state = Some(
            wait_for(&mut rx, |e| match &e.kind {
                ExecutorEventKind::CardUpdated(c) if c.id == card_id => {
                    (!matches!(c.state, CardState::Designing(DesignSub::Running)))
                        .then(|| c.state.clone())
                }
                _ => None,
            })
            .await,
        );
    }
    let state = state.unwrap();
    let runs = provider.runs.lock().unwrap().clone();
    (state, runs)
}

#[tokio::test]
async fn a_run_cut_off_by_the_plan_limit_continues_on_api_credits() {
    let (state, runs) = run_card(true).await;
    assert!(
        !matches!(state, CardState::Failed { .. }),
        "the continued run finishes the plan, got {state:?}"
    );
    assert_eq!(runs.len(), 2, "one cut-off run, one continued run");
    assert_eq!(runs[0].claude_auth, ClaudeAuth::Subscription);
    assert_eq!(
        runs[1].claude_auth,
        ClaudeAuth::ApiKey(SecretString::new("sk-test"))
    );
    // A read-only plan run resumes its own conversation, told why it stopped.
    assert_eq!(runs[1].resume_session.as_deref(), Some("sess-1"));
    assert!(runs[1]
        .extra_prompt
        .as_deref()
        .is_some_and(|e| e.contains("interrupted by a usage limit")));
}

#[tokio::test]
async fn a_nearly_full_plan_window_moves_the_next_runs_to_credits() {
    let provider = LimitedProvider {
        near_limit: true,
        ..Default::default()
    };
    let (state, runs) = run_cards(true, provider, 2).await;
    assert!(!matches!(state, CardState::Failed { .. }), "{state:?}");
    assert_eq!(runs.len(), 2, "no run was cut off");
    // The run that reported 97% finished on the plan; the next started on
    // the key.
    assert_eq!(runs[0].claude_auth, ClaudeAuth::Subscription);
    assert!(runs[1].claude_auth.is_api_key());
}

#[tokio::test]
async fn a_pr_review_cut_off_by_the_plan_limit_resumes_on_api_credits() {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        review_contributors: vec!["octocat".into()],
        ..Default::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/usine-api-credits-review"), config);
    store.upsert_project(&project).unwrap();
    let provider = LimitedProvider::default();
    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store,
        providers: Arc::new(provider.clone()),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::SetAnthropicKey {
        key: SecretString::new("sk-test"),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::BillingUpdated(b) if b.fallback_configured => Some(()),
        _ => None,
    })
    .await;
    handle.send(ExecutorCommand::ScanReviews {
        project_id: project.id,
    });
    let review_id = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTasksUpdated { tasks, .. } if !tasks.is_empty() => {
            Some(tasks[0].id)
        }
        _ => None,
    })
    .await;
    handle.send(ExecutorCommand::StartReview {
        review_id,
        guidance: String::new(),
    });
    let status = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::ReviewTaskUpdated(t)
            if t.id == review_id && !matches!(t.status, ReviewStatus::Reviewing) =>
        {
            Some(t.status.clone())
        }
        _ => None,
    })
    .await;
    assert!(
        matches!(status, ReviewStatus::AwaitingValidation { .. }),
        "the continued review finishes, got {status:?}"
    );
    let runs = provider.runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 2, "one cut-off run, one continued run");
    assert_eq!(runs[0].claude_auth, ClaudeAuth::Subscription);
    assert!(runs[1].claude_auth.is_api_key());
    assert_eq!(runs[1].resume_session.as_deref(), Some("sess-1"));
}

#[tokio::test]
async fn without_a_key_the_limit_fails_the_run_with_a_hint() {
    let (state, runs) = run_card(false).await;
    match state {
        CardState::Failed { message, .. } => {
            assert!(message.contains("session limit"), "{message}");
            assert!(message.contains("Save an Anthropic API key"), "{message}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(runs.len(), 1);
}
