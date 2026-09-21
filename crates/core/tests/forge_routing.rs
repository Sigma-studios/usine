//! Projects on different code hosts share one board: each project's PR
//! operations go through the forge its kind resolves to (see
//! `ForgeRegistry`), a forge that can't be built fails its calls with the
//! reason instead of silently falling back to another host, and a declined
//! review comment is answered through `Forge::decline_comment` — which on
//! Azure DevOps also closes the thread, so a comment policy can't block the
//! merge on it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor_with_forges, Card, CardConfig, CardState, CoreError, DraftComment,
    ExecutorCommand, ExecutorConfig, ExecutorEvent, ExecutorEventKind, FixVerdict, Forge,
    ForgeFactory, ForgeKind, ForgeRegistry, Mergeable, PrInfo, PrReviewSub, PrSummary, Project,
    ProjectConfig, ReviewComment, ReviewEvent, ReviewScope, ReviewSummary, ReviewThread, Severity,
    SimFactory, SimForge, SimGit, Store,
};
use uuid::Uuid;

/// A forge that names itself in its reviewer list and records how declined
/// comments reach it.
struct Named {
    name: &'static str,
    declined: Mutex<Vec<(u64, String)>>,
    plain_replies: Mutex<usize>,
}

impl Named {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Named {
            name,
            declined: Mutex::new(Vec::new()),
            plain_replies: Mutex::new(0),
        })
    }
}

#[async_trait]
impl Forge for Named {
    async fn list_reviewers(&self, _: &Path) -> usine_core::Result<Vec<String>> {
        Ok(vec![format!("{}-reviewer", self.name)])
    }
    async fn decline_comment(
        &self,
        _: &Path,
        _: u64,
        id: u64,
        body: &str,
    ) -> usine_core::Result<()> {
        self.declined.lock().unwrap().push((id, body.to_string()));
        Ok(())
    }
    async fn reply_to_comment(&self, _: &Path, _: u64, _: u64, _: &str) -> usine_core::Result<()> {
        *self.plain_replies.lock().unwrap() += 1;
        Ok(())
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
    async fn fetch_comments(&self, r: &Path, n: u64) -> usine_core::Result<Vec<ReviewComment>> {
        SimForge.fetch_comments(r, n).await
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
    async fn list_submitted_reviews(
        &self,
        r: &Path,
        n: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        SimForge.list_submitted_reviews(r, n).await
    }
    async fn mark_ready(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.mark_ready(r, n).await
    }
    async fn merge(&self, r: &Path, n: u64) -> usine_core::Result<()> {
        SimForge.merge(r, n).await
    }
    async fn is_merged(&self, r: &Path, n: u64) -> usine_core::Result<bool> {
        SimForge.is_merged(r, n).await
    }
    async fn merge_status(&self, r: &Path, n: u64) -> usine_core::Result<Mergeable> {
        SimForge.merge_status(r, n).await
    }
    async fn delete_remote_branch(&self, r: &Path, b: &str) -> usine_core::Result<()> {
        SimForge.delete_remote_branch(r, b).await
    }
    async fn resolve_threads(&self, r: &Path, n: u64, ids: &[u64]) -> usine_core::Result<usize> {
        SimForge.resolve_threads(r, n, ids).await
    }
    async fn list_threads(&self, r: &Path, n: u64) -> usine_core::Result<Vec<ReviewThread>> {
        SimForge.list_threads(r, n).await
    }
}

/// Hands out one fixed forge, or fails like an Azure project whose `origin`
/// isn't an Azure remote.
struct Factory(Option<Arc<Named>>);

impl ForgeFactory for Factory {
    fn for_repo(&self, _: &Path) -> usine_core::Result<Arc<dyn Forge>> {
        match &self.0 {
            Some(f) => Ok(Arc::clone(f) as Arc<dyn Forge>),
            None => Err(CoreError::forge(
                "`origin` isn't an Azure DevOps repository",
            )),
        }
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

fn project(store: &Store, name: &str, forge: Option<ForgeKind>) -> Project {
    let project = Project::new(
        name,
        PathBuf::from(format!("/tmp/usine-forge-routing-{name}")),
        ProjectConfig {
            pinned_forge: forge,
            ..ProjectConfig::default()
        },
    );
    store.upsert_project(&project).unwrap();
    project
}

fn spawn(
    store: &Store,
    registry: ForgeRegistry,
) -> (usine_core::ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor_with_forges(
        ExecutorConfig {
            store: store.clone(),
            providers: Arc::new(SimFactory),
            forge: registry.default_forge(),
            git: Arc::new(SimGit),
        },
        registry,
    )
}

async fn reviewers_of(
    handle: &usine_core::ExecutorHandle,
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    project_id: Uuid,
) -> Vec<String> {
    handle.send(ExecutorCommand::ListReviewers { project_id });
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::Reviewers {
            project_id: p,
            logins,
        } if *p == project_id => Some(logins.clone()),
        _ => None,
    })
    .await
}

#[tokio::test]
async fn each_project_goes_through_the_forge_of_its_kind() {
    let store = Store::open_in_memory().unwrap();
    let github = project(&store, "gh", None);
    let azure = project(&store, "az", Some(ForgeKind::AzureDevOps));
    let registry = ForgeRegistry::new(Named::new("github")).with(
        ForgeKind::AzureDevOps,
        Arc::new(Factory(Some(Named::new("azure")))),
    );
    let (handle, mut rx) = spawn(&store, registry);

    assert_eq!(
        reviewers_of(&handle, &mut rx, github.id).await,
        vec!["github-reviewer"]
    );
    assert_eq!(
        reviewers_of(&handle, &mut rx, azure.id).await,
        vec!["azure-reviewer"]
    );
}

#[tokio::test]
async fn a_forge_that_cant_be_built_fails_loudly_instead_of_using_another_host() {
    let store = Store::open_in_memory().unwrap();
    let azure = project(&store, "az-broken", Some(ForgeKind::AzureDevOps));
    let registry = ForgeRegistry::new(Named::new("github"))
        .with(ForgeKind::AzureDevOps, Arc::new(Factory(None)));
    let (handle, mut rx) = spawn(&store, registry);

    handle.send(ExecutorCommand::ListReviewers {
        project_id: azure.id,
    });
    let message = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::Reviewers { .. } => panic!("fell back to another forge"),
        ExecutorEventKind::Toast {
            severity: Severity::Error,
            message,
        } => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(
        message.contains("Azure DevOps") && message.contains("isn't an Azure DevOps repository"),
        "{message}"
    );
}

fn verdict(id: u64, selected: bool) -> FixVerdict {
    FixVerdict {
        comment: ReviewComment {
            id,
            author: "rita@x.com".into(),
            body: format!("comment {id}"),
            path: String::new(),
            line: None,
            review_body_of: None,
        },
        worth_fixing: selected,
        severity: "low".into(),
        rationale: "because".into(),
        selected,
        reply: if selected {
            String::new()
        } else {
            "Out of scope for this PR.".into()
        },
        instruction: String::new(),
    }
}

#[tokio::test]
async fn a_declined_comment_is_answered_through_decline_comment() {
    let store = Store::open_in_memory().unwrap();
    let project = project(&store, "az-decline", Some(ForgeKind::AzureDevOps));
    std::fs::create_dir_all(&project.path).unwrap();
    let wt = format!("{}-wt", project.path.display());
    std::fs::create_dir_all(&wt).unwrap();
    let mut card = Card::new(project.id, "c", "Do it.", CardConfig::default());
    card.state = CardState::PrReview(PrReviewSub::SelectingFixes {
        verdicts: vec![verdict(4097, false)],
    });
    card.branch = Some("feat/x".into());
    card.worktree_path = Some(PathBuf::from(&wt));
    card.pr = Some(PrInfo {
        number: 22,
        url: "https://dev.azure.com/o/p/_git/r/pullrequest/22".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: Some("rita@x.com".into()),
        reviewer_recorded: true,
    });
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    let azure = Named::new("azure");
    let registry = ForgeRegistry::new(Named::new("github")).with(
        ForgeKind::AzureDevOps,
        Arc::new(Factory(Some(Arc::clone(&azure)))),
    );
    let (handle, mut rx) = spawn(&store, registry);
    handle.send(ExecutorCommand::ApplyFixes {
        card_id,
        verdicts: vec![verdict(4097, false)],
        note: String::new(),
        prompt: None,
    });
    // Nothing checked: the picker answers the declined comment and moves on.
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if e.card_id == card_id && matches!(c.state, CardState::ReadyToMerge) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    assert_eq!(
        *azure.declined.lock().unwrap(),
        vec![(4097, "Out of scope for this PR.".to_string())]
    );
    assert_eq!(*azure.plain_replies.lock().unwrap(), 0);
}
