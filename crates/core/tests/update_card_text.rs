//! `UpdateCardText` — the MCP `update_card` tool's write — touches only the
//! text fields it names. It runs against a card mid-run (it is deliberately not
//! exclusive), so it must leave the executor-owned state alone, and unlike
//! `SaveCard` it carries no snapshot that could roll back a config change.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorCommand, ExecutorConfig,
    ExecutorEventKind, Project, ProjectConfig, RunSub, Severity, SimFactory, SimForge, SimGit,
    Store,
};

fn setup(state: CardState) -> (Store, Card) {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "old title", "the task", CardConfig::default());
    card.state = state;
    store.upsert_card(&card).unwrap();
    (store, card)
}

fn executor(
    store: &Store,
) -> (
    usine_core::ExecutorHandle,
    futures::channel::mpsc::UnboundedReceiver<usine_core::ExecutorEvent>,
) {
    spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    })
}

/// Wait for the toast the dispatcher raises when a handler refuses.
async fn expect_error_toast(
    rx: &mut futures::channel::mpsc::UnboundedReceiver<usine_core::ExecutorEvent>,
) {
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(5), rx.next())
            .await
            .expect("timed out waiting for the refusal")
            .expect("event stream closed");
        if let ExecutorEventKind::Toast { severity, .. } = evt.kind {
            assert_eq!(severity, Severity::Error);
            return;
        }
    }
}

/// The MCP checks the starting block on its own read; a `Start` landing after
/// that read must still stop the description rewrite (and the title with it).
#[tokio::test]
async fn a_description_update_on_a_started_card_is_refused_by_the_executor() {
    let (store, card) = setup(CardState::Implementing(RunSub::Running));
    let (handle, mut rx) = executor(&store);
    handle.send(ExecutorCommand::UpdateCardText {
        card_id: card.id,
        title: Some("new title".into()),
        description: Some("rewritten".into()),
    });
    expect_error_toast(&mut rx).await;
    let stored = store.get_card(card.id).unwrap();
    assert_eq!(stored.title, "old title");
    assert_eq!(stored.description, "the task");
}

/// Same race for the MCP's delete: a card started since the tool's read stays.
#[tokio::test]
async fn a_starting_block_only_delete_of_a_started_card_is_refused() {
    let (store, card) = setup(CardState::Implementing(RunSub::Running));
    let (handle, mut rx) = executor(&store);
    handle.send(ExecutorCommand::DeleteCard {
        card_id: card.id,
        starting_block_only: true,
    });
    expect_error_toast(&mut rx).await;
    assert!(store.get_card(card.id).is_ok());
}

#[tokio::test]
async fn a_starting_block_only_delete_of_an_unstarted_card_deletes_it() {
    let (store, card) = setup(CardState::StartingBlock);
    let (handle, mut rx) = executor(&store);
    handle.send(ExecutorCommand::DeleteCard {
        card_id: card.id,
        starting_block_only: true,
    });
    loop {
        let evt = tokio::time::timeout(Duration::from_secs(5), rx.next())
            .await
            .expect("timed out waiting for the removal")
            .expect("event stream closed");
        if evt.card_id == card.id && matches!(evt.kind, ExecutorEventKind::CardRemoved) {
            break;
        }
    }
    assert!(store.get_card(card.id).is_err());
}

#[tokio::test]
async fn a_title_update_on_a_running_card_changes_only_the_title() {
    let store = Store::open_in_memory().unwrap();
    let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
    store.upsert_project(&project).unwrap();
    let mut card = Card::new(project.id, "old title", "the task", CardConfig::default());
    card.state = CardState::Implementing(RunSub::Running);
    card.last_session = Some("sess-1".into());
    card.branch = Some("usine/x".into());
    store.upsert_card(&card).unwrap();

    let (handle, mut rx) = spawn_executor(ExecutorConfig {
        store: store.clone(),
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });
    handle.send(ExecutorCommand::UpdateCardText {
        card_id: card.id,
        title: Some("new title".into()),
        description: None,
    });

    let echoed = loop {
        let evt = tokio::time::timeout(Duration::from_secs(5), rx.next())
            .await
            .expect("timed out waiting for the echo")
            .expect("event stream closed");
        if let ExecutorEventKind::CardUpdated(c) = evt.kind {
            if c.id == card.id {
                break *c;
            }
        }
    };
    assert_eq!(echoed.title, "new title");

    let stored = store.get_card(card.id).unwrap();
    assert_eq!(stored.title, "new title");
    assert_eq!(stored.description, "the task");
    assert_eq!(stored.state, CardState::Implementing(RunSub::Running));
    assert_eq!(stored.last_session.as_deref(), Some("sess-1"));
    assert_eq!(stored.branch.as_deref(), Some("usine/x"));
    assert_eq!(stored.config, card.config);
    assert_eq!(stored.session_id, card.session_id);
}
