//! Screenshots pasted into the PR description reach the PR as hosted images:
//! `create_pr` swaps each `usine-image:<id>` placeholder for the URL the forge
//! hosted that card attachment at — and opens no PR when it can't.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, DraftComment, ExecutorCommand, ExecutorConfig,
    ExecutorEvent, ExecutorEventKind, Forge, Mergeable, PrImage, PrInfo, PrSummary, Project,
    ProjectConfig, ReviewComment, ReviewEvent, ReviewScope, ReviewSub, ReviewSummary, ReviewThread,
    Severity, SimFactory, SimForge, SimGit, Store,
};

/// The sim forge, recording each PR body it is asked to open, and hosting
/// images only when `hosts` (otherwise the trait's default refusal applies).
struct RecordingForge {
    hosts: bool,
    bodies: Arc<Mutex<Vec<String>>>,
    hosted: Arc<Mutex<Vec<PrImage>>>,
}

#[async_trait]
impl Forge for RecordingForge {
    async fn create_pr(
        &self,
        r: &Path,
        t: &str,
        body: &str,
        b: &str,
        h: &str,
        rv: Option<&str>,
        d: bool,
    ) -> usine_core::Result<PrInfo> {
        self.bodies.lock().unwrap().push(body.to_string());
        SimForge.create_pr(r, t, body, b, h, rv, d).await
    }
    async fn host_pr_images(
        &self,
        r: &Path,
        head: &str,
        images: &[PrImage],
    ) -> usine_core::Result<Vec<String>> {
        if !self.hosts {
            return Err(usine_core::CoreError::forge(
                "this forge can't embed images in a PR description yet",
            ));
        }
        self.hosted.lock().unwrap().extend_from_slice(images);
        SimForge.host_pr_images(r, head, images).await
    }
    async fn fetch_comments(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewComment>> {
        Ok(vec![])
    }
    async fn list_submitted_reviews(
        &self,
        _: &Path,
        _: u64,
    ) -> usine_core::Result<Vec<ReviewSummary>> {
        Ok(vec![])
    }
    async fn list_threads(&self, _: &Path, _: u64) -> usine_core::Result<Vec<ReviewThread>> {
        Ok(vec![])
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
    async fn reply_to_comment(&self, r: &Path, n: u64, c: u64, b: &str) -> usine_core::Result<()> {
        SimForge.reply_to_comment(r, n, c, b).await
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
    async fn resolve_threads(&self, r: &Path, n: u64, c: &[u64]) -> usine_core::Result<usize> {
        SimForge.resolve_threads(r, n, c).await
    }
}

struct Setup {
    store: Store,
    card_id: uuid::Uuid,
    bodies: Arc<Mutex<Vec<String>>>,
    hosted: Arc<Mutex<Vec<PrImage>>>,
    rx: UnboundedReceiver<ExecutorEvent>,
    _handle: usine_core::ExecutorHandle,
    _dir: tempfile::TempDir,
}

/// Seed a `ReadyForPr` card carrying `attachments` (file name → bytes, written
/// to a temp dir), and ask the executor to open a PR with `body`.
fn create_pr(hosts: bool, attachments: &[(&str, &[u8])], body: &str) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let project = Project::new(
        "p",
        PathBuf::from("/tmp/usine-pr-images"),
        ProjectConfig::default(),
    );
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    card.state = CardState::AwaitingReview(ReviewSub::ReadyForPr);
    card.branch = Some("feat/thing".into());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    let paths: Vec<PathBuf> = attachments
        .iter()
        .map(|(name, bytes)| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        })
        .collect();
    store.set_attachments(card_id, &paths).unwrap();

    let bodies = Arc::new(Mutex::new(Vec::new()));
    let hosted = Arc::new(Mutex::new(Vec::new()));
    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(RecordingForge {
            hosts,
            bodies: Arc::clone(&bodies),
            hosted: Arc::clone(&hosted),
        }),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::CreatePr {
        card_id,
        branch: "feat/thing".into(),
        title: "t".into(),
        body: body.into(),
        reviewer: None,
        draft: false,
    });
    Setup {
        store,
        card_id,
        bodies,
        hosted,
        rx,
        _handle: handle,
        _dir: dir,
    }
}

/// The first toast of `severity` for the card.
async fn toast(s: &mut Setup, severity: Severity) -> String {
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(15), s.rx.next())
            .await
            .expect("timed out waiting for a toast")
            .expect("event stream closed unexpectedly");
        if let ExecutorEventKind::Toast {
            severity: sev,
            message,
        } = &evt.kind
        {
            if evt.card_id == s.card_id && *sev == severity {
                return message.clone();
            }
        }
    }
}

#[tokio::test]
async fn a_pasted_image_is_hosted_and_embedded_in_the_pr_body() {
    let mut s = create_pr(
        true,
        &[
            ("0a1b2c3d-pasted-1.png", b"png!"),
            ("ffffffff-notes.txt", b"n"),
        ],
        "Before.\n\n![pasted-1](usine-image:0a1b2c3d)\n\nAgain: ![x](usine-image:0a1b2c3d)",
    );
    toast(&mut s, Severity::Success).await;

    let bodies = s.bodies.lock().unwrap().clone();
    let url = "https://sim.usine/pr-images/feat/thing/0a1b2c3d-pasted-1.png";
    assert_eq!(
        bodies,
        vec![format!(
            "Before.\n\n![pasted-1]({url})\n\nAgain: ![x]({url})"
        )]
    );
    // Only the referenced attachment, once, with its bytes.
    let hosted = s.hosted.lock().unwrap().clone();
    assert_eq!(hosted.len(), 1);
    assert_eq!(hosted[0].name, "0a1b2c3d-pasted-1.png");
    assert_eq!(hosted[0].bytes, b"png!");
    assert!(s.store.get_card(s.card_id).unwrap().pr.is_some());
}

#[tokio::test]
async fn a_body_without_placeholders_never_touches_hosting() {
    // A forge that can't host still opens a placeholder-free PR.
    let mut s = create_pr(false, &[("0a1b2c3d-pasted-1.png", b"png!")], "Plain.");
    toast(&mut s, Severity::Success).await;
    assert_eq!(*s.bodies.lock().unwrap(), vec!["Plain.".to_string()]);
}

#[tokio::test]
async fn a_removed_attachment_fails_before_the_pr_is_opened() {
    let mut s = create_pr(true, &[], "![pasted-1](usine-image:0a1b2c3d)");
    let err = toast(&mut s, Severity::Error).await;
    assert!(err.contains("0a1b2c3d") && err.contains("removed"), "{err}");
    assert_no_pr(&s);
}

#[tokio::test]
async fn a_forge_that_cannot_host_fails_before_the_pr_is_opened() {
    let mut s = create_pr(
        false,
        &[("0a1b2c3d-pasted-1.png", b"png!")],
        "![pasted-1](usine-image:0a1b2c3d)",
    );
    let err = toast(&mut s, Severity::Error).await;
    assert!(err.contains("can't embed images"), "{err}");
    assert_no_pr(&s);
}

fn assert_no_pr(s: &Setup) {
    assert!(s.bodies.lock().unwrap().is_empty(), "no PR may be opened");
    let card = s.store.get_card(s.card_id).unwrap();
    assert!(matches!(
        card.state,
        CardState::AwaitingReview(ReviewSub::ReadyForPr)
    ));
    assert!(card.pr.is_none());
}
