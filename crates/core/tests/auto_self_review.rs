//! Proves the pre-PR self-review starts itself: a finished implementation flows
//! straight into the review pass and parks at the fix picker without any
//! `SelfReview` command — the user's first interaction is picking which review
//! comments to fix. Cancelling the auto-started pass parks the card back at the
//! manual `ReadyForReview` gate, the escape hatch with all the buttons.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig, ExecutorEvent,
    ExecutorEventKind, Project, ProjectConfig, ReviewSub, SimFactory, SimForge, SimGit, Store,
};

/// Await events until `f` returns `Some`, failing the test on timeout.
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

/// A store + executor over the simulated backends holding one plan-skipping card.
fn setup(
    dir: &str,
) -> (
    Store,
    usine_core::ExecutorHandle,
    UnboundedReceiver<ExecutorEvent>,
    uuid::Uuid,
) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from(dir), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let card = Card::new(project.id, "c", "Do the thing.", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    // Straight to implementing — the plan phase would block on a sim question.
    store.set_skip_plan(card_id, true).unwrap();

    let (handle, rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    (store, handle, rx, card_id)
}

#[tokio::test]
async fn a_finished_implementation_reviews_itself_to_the_fix_picker() {
    let (_store, handle, mut rx, card_id) = setup("/tmp/auto-self-review");

    // One command only: no `SelfReview` is ever sent.
    handle.send(ExecutorCommand::Start { card_id });

    // The card passes through the (now transient) gate into the review pass…
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::AwaitingReview(ReviewSub::Reviewing)) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // …and parks at the picker with the sim's findings in hand.
    let verdicts = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::SelectingFixes { verdicts }) => {
                Some(verdicts.clone())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(verdicts.len(), 2, "the sim self-review finds two things");
}

#[tokio::test]
async fn cancelling_the_auto_review_parks_at_the_manual_gate() {
    let (store, handle, mut rx, card_id) = setup("/tmp/auto-self-review-cancel");

    handle.send(ExecutorCommand::Start { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if matches!(c.state, CardState::AwaitingReview(ReviewSub::Reviewing)) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // Cancel the pass: the card must rest at `ReadyForReview` (where the manual
    // Self-review / Skip review / Request changes buttons live), not re-review.
    handle.send(ExecutorCommand::Cancel { card_id });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c) => match &c.state {
            CardState::AwaitingReview(ReviewSub::ReadyForReview) => Some(()),
            CardState::AwaitingReview(ReviewSub::SelectingFixes { .. }) => {
                panic!("a cancelled review must not still deliver its findings")
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert!(matches!(
        store.get_card(card_id).unwrap().state,
        CardState::AwaitingReview(ReviewSub::ReadyForReview)
    ));
}
