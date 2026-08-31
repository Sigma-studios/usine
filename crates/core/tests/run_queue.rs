//! The global run-concurrency cap over the simulated backends: with the cap
//! saturated a second card's launch parks in the FIFO queue (its card stays in
//! the running state, badge-only), dequeues when a slot frees or the cap is
//! raised, is purged by cancel, and never blocks the exempt Agent Chat
//! questions.
//!
//! Deterministic slot-holding: a sim plan run is interactive and parks on its
//! mid-plan question with the actor alive — the slot stays held until the
//! question is answered and the plan lands.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedReceiver;
use futures::StreamExt;
use usine_core::{
    spawn_executor, AppSettings, Card, CardConfig, CardState, DesignSub, ExecutorCommand,
    ExecutorConfig, ExecutorEvent, ExecutorEventKind, ExecutorHandle, Project, ProjectConfig,
    QueuedTarget, ReviewSub, SimFactory, SimForge, SimGit, Store,
};
use uuid::Uuid;

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

fn spawn_with(store: &Store) -> (ExecutorHandle, UnboundedReceiver<ExecutorEvent>) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

/// A store whose persisted settings cap concurrent runs at `cap`.
fn store_with_cap(cap: u32) -> Store {
    let store = Store::open_in_memory().unwrap();
    let settings = AppSettings {
        max_concurrent_runs: cap,
        ..AppSettings::default()
    };
    store.save_settings(&settings).unwrap();
    store
}

fn seed_card(store: &Store, project_id: Uuid, title: &str) -> Uuid {
    let card = Card::new(project_id, title, "d", CardConfig::default());
    let card_id = card.id;
    store.upsert_card(&card).unwrap();
    card_id
}

fn seed_project(store: &Store, dir: &str) -> Uuid {
    let project = Project::new("p", PathBuf::from(dir), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    project.id
}

/// Start card `a` and drive its sim plan run to the mid-plan question, where
/// the interactive run parks holding its concurrency slot.
async fn park_holding_slot(
    handle: &ExecutorHandle,
    rx: &mut UnboundedReceiver<ExecutorEvent>,
    a: Uuid,
) {
    handle.send(ExecutorCommand::Start { card_id: a });
    wait_for(rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == a && matches!(c.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn a_second_card_queues_at_the_cap_and_dequeues_when_a_slot_frees() {
    let store = store_with_cap(1);
    let project_id = seed_project(&store, "/tmp/run-queue-basic");
    let a = seed_card(&store, project_id, "a");
    let b = seed_card(&store, project_id, "b");
    let (handle, mut rx) = spawn_with(&store);

    park_holding_slot(&handle, &mut rx, a).await;

    // The second start parks in the queue: the UI hears about it, and the card
    // stays in the running state its command put it in — with no run behind it.
    handle.send(ExecutorCommand::Start { card_id: b });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries } if entries == &[QueuedTarget::Card(b)] => {
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(matches!(
        store.get_card(b).unwrap().state,
        CardState::Designing(DesignSub::Running)
    ));

    // Answering a's question lets its plan run finish (AwaitingApproval); the
    // freed slot pumps b, whose own plan run then parks on its question.
    handle.send(ExecutorCommand::Answer {
        card_id: a,
        text: "Simplicity".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == a
                && matches!(
                    c.state,
                    CardState::Designing(DesignSub::AwaitingApproval { .. })
                ) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == b && matches!(c.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    // The queue drained.
    assert!(matches!(
        store.get_card(b).unwrap().state,
        CardState::Designing(DesignSub::Intervention(_))
    ));
}

#[tokio::test]
async fn the_queue_drains_in_fifo_order() {
    let store = store_with_cap(1);
    let project_id = seed_project(&store, "/tmp/run-queue-fifo");
    let a = seed_card(&store, project_id, "a");
    let b = seed_card(&store, project_id, "b");
    let c = seed_card(&store, project_id, "c");
    let (handle, mut rx) = spawn_with(&store);

    park_holding_slot(&handle, &mut rx, a).await;
    handle.send(ExecutorCommand::Start { card_id: b });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries } if entries == &[QueuedTarget::Card(b)] => {
            Some(())
        }
        _ => None,
    })
    .await;
    handle.send(ExecutorCommand::Start { card_id: c });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries }
            if entries == &[QueuedTarget::Card(b), QueuedTarget::Card(c)] =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // The freed slot goes to b — first in line — and c keeps its place at the
    // head of the queue (the pump claims the slot at pop time, so nothing can
    // bounce a dequeued entry back behind a later arrival).
    handle.send(ExecutorCommand::Answer {
        card_id: a,
        text: "Simplicity".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(card)
            if card.id == b
                && matches!(card.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(matches!(
        store.get_card(c).unwrap().state,
        CardState::Designing(DesignSub::Running)
    ));

    // And the next freed slot goes to c.
    handle.send(ExecutorCommand::Answer {
        card_id: b,
        text: "Simplicity".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(card)
            if card.id == c
                && matches!(card.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
}

#[tokio::test]
async fn cancelling_a_queued_card_purges_it_and_it_never_launches() {
    let store = store_with_cap(1);
    let project_id = seed_project(&store, "/tmp/run-queue-cancel");
    let a = seed_card(&store, project_id, "a");
    let b = seed_card(&store, project_id, "b");
    let (handle, mut rx) = spawn_with(&store);

    park_holding_slot(&handle, &mut rx, a).await;
    handle.send(ExecutorCommand::Start { card_id: b });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries } if entries == &[QueuedTarget::Card(b)] => {
            Some(())
        }
        _ => None,
    })
    .await;

    // Stop works on a queued card: the entry is purged and the card parks.
    handle.send(ExecutorCommand::Cancel { card_id: b });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries } if entries.is_empty() => Some(()),
        _ => None,
    })
    .await;
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == b && matches!(c.state, CardState::StartingBlock) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // Freeing a's slot must not resurrect b's cancelled launch.
    handle.send(ExecutorCommand::Answer {
        card_id: a,
        text: "Simplicity".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == a
                && matches!(
                    c.state,
                    CardState::Designing(DesignSub::AwaitingApproval { .. })
                ) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    // Give a straggler pump a beat, then check b never left the starting block.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(matches!(
        store.get_card(b).unwrap().state,
        CardState::StartingBlock
    ));
}

#[tokio::test]
async fn raising_the_cap_drains_the_queue_immediately() {
    let store = store_with_cap(1);
    let project_id = seed_project(&store, "/tmp/run-queue-raise");
    let a = seed_card(&store, project_id, "a");
    let b = seed_card(&store, project_id, "b");
    let (handle, mut rx) = spawn_with(&store);

    park_holding_slot(&handle, &mut rx, a).await;
    handle.send(ExecutorCommand::Start { card_id: b });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries } if entries == &[QueuedTarget::Card(b)] => {
            Some(())
        }
        _ => None,
    })
    .await;

    // Raising the cap frees a slot; b launches without a finishing anything.
    let settings = AppSettings {
        max_concurrent_runs: 2,
        ..AppSettings::default()
    };
    handle.send(ExecutorCommand::SaveSettings {
        settings: Box::new(settings),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(c)
            if c.id == b && matches!(c.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    // a still holds its (parked-on-question) slot the whole time.
    assert!(matches!(
        store.get_card(a).unwrap().state,
        CardState::Designing(DesignSub::Intervention(_))
    ));
}

#[tokio::test]
async fn agent_chat_questions_bypass_a_saturated_cap() {
    let store = store_with_cap(1);
    let project_id = seed_project(&store, "/tmp/run-queue-question");
    let a = seed_card(&store, project_id, "a");
    // A card parked at the manual review gate, ready to be asked about.
    let c = seed_card(&store, project_id, "c");
    store
        .mutate_card(c, |card| {
            card.state = CardState::AwaitingReview(ReviewSub::ReadyForReview);
            Ok(())
        })
        .unwrap();
    let (handle, mut rx) = spawn_with(&store);

    park_holding_slot(&handle, &mut rx, a).await;

    // The question run is exempt: it answers while the cap is saturated.
    handle.send(ExecutorCommand::AskQuestion {
        card_id: c,
        question: "is the boundary sound?".into(),
    });
    let answer = wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::AnswersUpdated { answers } if e.card_id == c => Some(
            answers
                .exchanges
                .last()
                .map(|x| x.answer.clone())
                .unwrap_or_default(),
        ),
        _ => None,
    })
    .await;
    assert!(!answer.is_empty(), "the exempt question run must complete");
    // The question lands the card back where it was asked from…
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(card)
            if card.id == c
                && matches!(
                    card.state,
                    CardState::AwaitingReview(ReviewSub::ReadyForReview)
                ) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    // …while the slot-holder never moved.
    assert!(matches!(
        store.get_card(a).unwrap().state,
        CardState::Designing(DesignSub::Intervention(_))
    ));
}

#[tokio::test]
async fn bumping_a_queued_card_jumps_the_line() {
    let store = store_with_cap(1);
    let project_id = seed_project(&store, "/tmp/run-queue-bump");
    let a = seed_card(&store, project_id, "a");
    let b = seed_card(&store, project_id, "b");
    let c = seed_card(&store, project_id, "c");
    let (handle, mut rx) = spawn_with(&store);

    park_holding_slot(&handle, &mut rx, a).await;
    handle.send(ExecutorCommand::Start { card_id: b });
    handle.send(ExecutorCommand::Start { card_id: c });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries }
            if entries == &[QueuedTarget::Card(b), QueuedTarget::Card(c)] =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // The bump reorders the waiting deque and says so.
    handle.send(ExecutorCommand::BumpQueued { id: c });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::RunQueueChanged { entries }
            if entries == &[QueuedTarget::Card(c), QueuedTarget::Card(b)] =>
        {
            Some(())
        }
        _ => None,
    })
    .await;

    // …so the freed slot goes to c, and b — the earlier arrival — waits.
    handle.send(ExecutorCommand::Answer {
        card_id: a,
        text: "Simplicity".into(),
    });
    wait_for(&mut rx, |e| match &e.kind {
        ExecutorEventKind::CardUpdated(card)
            if card.id == c
                && matches!(card.state, CardState::Designing(DesignSub::Intervention(_))) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    assert!(matches!(
        store.get_card(b).unwrap().state,
        CardState::Designing(DesignSub::Running)
    ));
}
