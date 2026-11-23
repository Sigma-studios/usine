//! The "nothing to do" escape hatch.
//!
//! A card can need no code change: the ask is already implemented, or it makes
//! no sense for the codebase. A plan or implement agent says so by ending its
//! reply with a ` ```usine-no-work ` block, and the card parks on
//! `CardState::NothingToDo` with the agent's explanation — not on `Failed`
//! with a generic "no files changed", and not on a < 200-char "no plan" fault.
//! From there the user marks it done (cleaning up the empty worktree), or
//! overrules it ("proceed anyway"), which re-runs the same phase and whose
//! Stop steps back to the verdict.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AgentProvider, Card, CardConfig, CardState, DesignSub, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, ExecutorHandle, GitOps, MergeOutcome,
    NoWorkKind, NoWorkPhase, Project, ProjectConfig, Provider, ProviderFactory, Result, RunConfig,
    RunHandle, RunMode, RunSub, SimForge, SimGit, Store,
};

const VERDICT: &str = "This is already implemented: `src/menu.rs:42` adds the entry.\n\n\
```usine-no-work\n\
{\"v\":1,\"kind\":\"already_done\",\"explanation\":\"src/menu.rs:42 already adds the Mark done entry.\"}\n\
```";

/// Git like the simulator, with knobs: whether a commit lands, and whether the
/// worktree holds uncommitted (untracked) work. Records deleted branches.
struct ScriptGit {
    commits: bool,
    dirty: bool,
    deleted: Mutex<Vec<String>>,
}

impl ScriptGit {
    fn new(commits: bool, dirty: bool) -> Arc<Self> {
        Arc::new(Self {
            commits,
            dirty,
            deleted: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl GitOps for ScriptGit {
    async fn commit_all(&self, _: &Path, _: &str) -> Result<bool> {
        Ok(self.commits)
    }
    async fn branch_has_commits(&self, _: &Path, _: &str) -> Result<bool> {
        Ok(false)
    }
    async fn untracked_files(&self, _: &Path) -> Result<Vec<PathBuf>> {
        Ok(if self.dirty {
            vec![PathBuf::from("scratch.txt")]
        } else {
            Vec::new()
        })
    }
    // The worktree dir really exists, so cleanup can tell "clean" from "gone".
    async fn create_worktree(&self, r: &Path, b: &str, p: &Path, base: &str) -> Result<()> {
        std::fs::create_dir_all(p)?;
        SimGit.create_worktree(r, b, p, base).await
    }
    async fn remove_worktree(&self, r: &Path, p: &Path) -> Result<()> {
        let _ = std::fs::remove_dir_all(p);
        SimGit.remove_worktree(r, p).await
    }
    async fn worktree_add_existing(&self, r: &Path, b: &str, p: &Path) -> Result<()> {
        SimGit.worktree_add_existing(r, b, p).await
    }
    async fn worktree_add_detached(&self, r: &Path, p: &Path, c: &str) -> Result<()> {
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
        self.deleted.lock().unwrap().push(b.to_string());
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

type Prompts = Arc<Mutex<Vec<(RunMode, String)>>>;

/// A one-shot provider that ends each run with the next scripted final
/// message (falling back to a plain "done").
struct ScriptedProvider {
    results: Arc<Mutex<Vec<String>>>,
    prompts: Prompts,
}

#[async_trait]
impl AgentProvider for ScriptedProvider {
    fn provider(&self) -> Provider {
        Provider::Claude
    }
    fn interactive(&self) -> bool {
        false
    }
    async fn start(&self, cfg: RunConfig) -> Result<RunHandle> {
        self.prompts
            .lock()
            .unwrap()
            .push((cfg.mode, cfg.full_prompt()));
        let result = {
            let mut rs = self.results.lock().unwrap();
            if rs.is_empty() {
                "done".to_string()
            } else {
                rs.remove(0)
            }
        };
        let (evt_tx, evt_rx) = futures::channel::mpsc::unbounded();
        let (ctl_tx, _ctl_rx) = futures::channel::mpsc::unbounded();
        let _ = evt_tx.unbounded_send(usine_core::AgentEvent::Started {
            session_id: "sess-1".into(),
        });
        let _ = evt_tx.unbounded_send(usine_core::AgentEvent::Done {
            result,
            cost_usd: 0.0,
            usage: usine_core::Usage::default(),
        });
        drop(evt_tx);
        Ok(RunHandle {
            events: evt_rx.boxed(),
            control: ctl_tx,
        })
    }
}

struct ScriptedFactory {
    results: Arc<Mutex<Vec<String>>>,
    prompts: Prompts,
}

impl ProviderFactory for ScriptedFactory {
    fn make(&self, _: Provider) -> Arc<dyn AgentProvider> {
        Arc::new(ScriptedProvider {
            results: self.results.clone(),
            prompts: self.prompts.clone(),
        })
    }
}

struct Rig {
    store: Store,
    card_id: uuid::Uuid,
    handle: ExecutorHandle,
    rx: UnboundedReceiver<ExecutorEvent>,
    prompts: Prompts,
}

fn rig(git: Arc<ScriptGit>, skip_plan: bool, results: Vec<&str>) -> Rig {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-no-work"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let card = Card::new(
        project.id,
        "Add Mark done",
        "Add a Mark done entry to the card menu.",
        CardConfig::default(),
    );
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    store.set_skip_plan(card_id, skip_plan).unwrap();
    let prompts: Prompts = Arc::new(Mutex::new(Vec::new()));
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(ScriptedFactory {
            results: Arc::new(Mutex::new(
                results.into_iter().map(str::to_string).collect(),
            )),
            prompts: prompts.clone(),
        }),
        forge: Arc::new(SimForge),
        git,
    });
    Rig {
        store,
        card_id,
        handle,
        rx,
        prompts,
    }
}

/// Wait for the card to reach a state `f` accepts, panicking on `bad`.
async fn wait_state<F>(rig: &mut Rig, mut f: F) -> CardState
where
    F: FnMut(&CardState) -> bool,
{
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(15), rig.rx.next())
            .await
            .expect("timed out waiting for an executor event")
            .expect("event stream closed unexpectedly");
        if let ExecutorEventKind::CardUpdated(c) = &evt.kind {
            if evt.card_id == rig.card_id && f(&c.state) {
                return c.state.clone();
            }
        }
    }
}

fn is_verdict(s: &CardState, phase: NoWorkPhase) -> bool {
    matches!(s, CardState::NothingToDo { phase: p, .. } if *p == phase)
}

fn settled(s: &CardState) -> bool {
    !s.is_running() && !matches!(s, CardState::StartingBlock)
}

#[tokio::test]
async fn an_empty_implement_run_with_a_verdict_parks_on_nothing_to_do() {
    let mut rig = rig(ScriptGit::new(false, false), true, vec![VERDICT]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    let state = wait_state(&mut rig, settled).await;
    match state {
        CardState::NothingToDo {
            phase,
            kind,
            explanation,
        } => {
            assert_eq!(phase, NoWorkPhase::Implement);
            assert_eq!(kind, NoWorkKind::AlreadyDone);
            assert!(explanation.contains("src/menu.rs:42"), "{explanation}");
        }
        other => panic!("expected NothingToDo, got {other:?}"),
    }
    // The implement prompt offered the escape hatch.
    assert!(rig.prompts.lock().unwrap()[0].1.contains("usine-no-work"));
    // The transcript line lands just after the state change.
    let mut transcript = Vec::new();
    for _ in 0..100 {
        transcript = rig.store.load_transcript(rig.card_id).unwrap();
        if transcript.iter().any(|l| l.contains("nothing to do")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        transcript
            .iter()
            .any(|l| l.contains("nothing to do — already done")),
        "{transcript:?}"
    );
}

#[tokio::test]
async fn an_empty_implement_run_without_a_verdict_still_fails_but_quotes_the_agent() {
    let mut rig = rig(
        ScriptGit::new(false, false),
        true,
        vec!["The menu already has that entry. I left everything as is."],
    );
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    let state = wait_state(&mut rig, settled).await;
    let CardState::Failed { message, .. } = state else {
        panic!("expected Failed, got {state:?}");
    };
    assert!(message.contains("without changing any files"), "{message}");
    assert!(
        message.contains("The menu already has that entry."),
        "the agent's words must reach the user: {message}"
    );
}

#[tokio::test]
async fn a_short_plan_with_a_verdict_parks_instead_of_failing_the_floor() {
    let mut rig = rig(ScriptGit::new(false, false), false, vec![VERDICT]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    let state = wait_state(&mut rig, settled).await;
    assert!(
        is_verdict(&state, NoWorkPhase::Plan),
        "expected a plan-phase verdict, got {state:?}"
    );
    let prompts = rig.prompts.lock().unwrap();
    assert_eq!(prompts[0].0, RunMode::Plan);
    assert!(prompts[0].1.contains("usine-no-work"));
}

#[tokio::test]
async fn proceed_anyway_reruns_the_plan_with_the_note() {
    let long_plan = format!("## Plan\n\n{}", "Step: do the thing properly. ".repeat(20));
    let mut rig = rig(
        ScriptGit::new(false, false),
        false,
        vec![VERDICT, long_plan.as_str()],
    );
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| is_verdict(s, NoWorkPhase::Plan)).await;
    rig.handle.send(ExecutorCommand::ProceedAnyway {
        card_id: rig.card_id,
        note: "I want it in the context menu too".into(),
    });
    let state = wait_state(&mut rig, |s| {
        matches!(s, CardState::Designing(DesignSub::AwaitingApproval { .. }))
    })
    .await;
    assert!(matches!(
        state,
        CardState::Designing(DesignSub::AwaitingApproval { .. })
    ));
    let prompts = rig.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[1].0, RunMode::Plan);
    assert!(prompts[1].1.contains("I want it in the context menu too"));
    assert!(prompts[1].1.contains("src/menu.rs:42 already adds"));
    drop(prompts);
    // The note is kept for a later Back to start.
    let card = rig.store.get_card(rig.card_id).unwrap();
    assert!(card.qa_log.iter().any(|e| e.contains("context menu too")));
}

#[tokio::test]
async fn proceed_anyway_reruns_the_implementation_and_stop_returns_to_the_verdict() {
    let mut rig = rig(ScriptGit::new(false, false), true, vec![VERDICT, VERDICT]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| is_verdict(s, NoWorkPhase::Implement)).await;
    rig.handle.send(ExecutorCommand::ProceedAnyway {
        card_id: rig.card_id,
        note: String::new(),
    });
    wait_state(&mut rig, |s| {
        matches!(s, CardState::Implementing(RunSub::Running))
    })
    .await;
    let card = rig.store.get_card(rig.card_id).unwrap();
    assert!(
        usine_core::stop_can_return_to(&card.state, card.entered_from.as_ref().unwrap()),
        "Stop must step back to the verdict, entered_from = {:?}",
        card.entered_from
    );
    // The agent repeats its verdict: the card parks on it again, not Failed.
    let state = wait_state(&mut rig, settled).await;
    assert!(is_verdict(&state, NoWorkPhase::Implement), "{state:?}");
    let prompts = rig.prompts.lock().unwrap();
    assert_eq!(prompts[1].0, RunMode::Implement);
    assert!(prompts[1].1.contains("no further detail given"));
}

#[tokio::test]
async fn mark_done_removes_an_empty_worktree() {
    let git = ScriptGit::new(false, false);
    let mut rig = rig(git.clone(), true, vec![VERDICT]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| is_verdict(s, NoWorkPhase::Implement)).await;
    let branch = rig.store.get_card(rig.card_id).unwrap().branch.unwrap();
    // Cutting the worktree pre-cleans a same-named branch; only count what
    // Mark done deletes.
    git.deleted.lock().unwrap().clear();
    rig.handle.send(ExecutorCommand::MarkDone {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| matches!(s, CardState::Done)).await;
    // Cleanup lands right after the Done transition.
    for _ in 0..100 {
        if rig.store.get_card(rig.card_id).unwrap().branch.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let card = rig.store.get_card(rig.card_id).unwrap();
    assert_eq!(card.branch, None);
    assert_eq!(card.worktree_path, None);
    assert!(git.deleted.lock().unwrap().contains(&branch));
}

#[tokio::test]
async fn mark_done_keeps_a_dirty_worktree() {
    let git = ScriptGit::new(false, true);
    let mut rig = rig(git.clone(), true, vec![VERDICT]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| is_verdict(s, NoWorkPhase::Implement)).await;
    let branch = rig.store.get_card(rig.card_id).unwrap().branch.unwrap();
    // Cutting the worktree pre-cleans a same-named branch; only count what
    // Mark done deletes.
    git.deleted.lock().unwrap().clear();
    rig.handle.send(ExecutorCommand::MarkDone {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| matches!(s, CardState::Done)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let card = rig.store.get_card(rig.card_id).unwrap();
    assert_eq!(card.branch.as_deref(), Some(branch.as_str()));
    assert!(card.worktree_path.is_some());
    assert!(!git.deleted.lock().unwrap().contains(&branch));
}

#[tokio::test]
async fn a_committed_run_that_also_emits_a_verdict_still_advances() {
    let reply = format!("Implemented the entry.\n\n{VERDICT}");
    let mut rig = rig(ScriptGit::new(true, false), true, vec![reply.as_str()]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    let state = wait_state(&mut rig, |s| {
        matches!(
            s,
            CardState::AwaitingReview(_) | CardState::NothingToDo { .. }
        )
    })
    .await;
    assert!(
        matches!(state, CardState::AwaitingReview(_)),
        "the diff wins over the verdict: {state:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let transcript = rig.store.load_transcript(rig.card_id).unwrap();
    assert!(
        !transcript.iter().any(|l| l.contains("usine-no-work")),
        "the block must not leak into the recap: {transcript:?}"
    );
}

#[tokio::test]
async fn mark_done_after_an_empty_implement_failure_removes_the_worktree() {
    let git = ScriptGit::new(false, false);
    let mut rig = rig(git.clone(), true, vec!["Nothing needed changing."]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    let state = wait_state(&mut rig, settled).await;
    assert!(matches!(state, CardState::Failed { .. }), "{state:?}");
    let branch = rig.store.get_card(rig.card_id).unwrap().branch.unwrap();
    git.deleted.lock().unwrap().clear();
    rig.handle.send(ExecutorCommand::MarkDone {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| matches!(s, CardState::Done)).await;
    for _ in 0..100 {
        if rig.store.get_card(rig.card_id).unwrap().branch.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let card = rig.store.get_card(rig.card_id).unwrap();
    assert_eq!(card.branch, None);
    assert_eq!(card.worktree_path, None);
    assert!(git.deleted.lock().unwrap().contains(&branch));
}

#[tokio::test]
async fn mark_done_drops_the_branch_of_a_worktree_removed_by_hand() {
    // Dirty per git, but the dir is gone: nothing can hold edits any more.
    let git = ScriptGit::new(false, true);
    let mut rig = rig(git.clone(), true, vec![VERDICT]);
    rig.handle.send(ExecutorCommand::Start {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| is_verdict(s, NoWorkPhase::Implement)).await;
    let card = rig.store.get_card(rig.card_id).unwrap();
    let branch = card.branch.unwrap();
    std::fs::remove_dir_all(card.worktree_path.unwrap()).unwrap();
    git.deleted.lock().unwrap().clear();
    rig.handle.send(ExecutorCommand::MarkDone {
        card_id: rig.card_id,
    });
    wait_state(&mut rig, |s| matches!(s, CardState::Done)).await;
    for _ in 0..100 {
        if rig.store.get_card(rig.card_id).unwrap().branch.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let card = rig.store.get_card(rig.card_id).unwrap();
    assert_eq!(card.branch, None);
    assert!(git.deleted.lock().unwrap().contains(&branch));
}
