//! The background PR poll must keep a card's reviewer-comment count *and* its
//! submitted-review list in step. They used to come from different paths — the
//! poll refreshed only the count, while the list was fetched on panel open and
//! cached — so a card could badge a review it didn't list until the user hit ↻.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use usine_core::{
    spawn_executor, Card, CardConfig, CardState, ExecutorConfig, ExecutorEventKind, PrInfo,
    PrReviewSub, Project, ProjectConfig, SimFactory, SimForge, SimGit, Store,
};

#[tokio::test]
async fn poll_refreshes_reviews_alongside_the_comment_count() {
    let store = Store::open_in_memory().unwrap();
    let config = ProjectConfig {
        reviewer: Some("reviewer".into()),
        ..Default::default()
    };
    let project = Project::new("p", PathBuf::from("/tmp/pr-comment-poll"), config);
    store.upsert_project(&project).unwrap();

    // A card parked at the PR gate with an open PR: what the poll looks for.
    let mut card = Card::new(project.id, "t", "d", CardConfig::default());
    card.state = CardState::PrReview(PrReviewSub::Idle);
    card.pr = Some(PrInfo {
        number: 42,
        url: "https://github.com/example/repo/pull/42".into(),
        title: "t".into(),
        state: "open".into(),
        reviewer: Some("reviewer".into()),
    });
    let card_id = card.id;
    store.upsert_card(&card).unwrap();

    // The poll loop's first tick fires immediately on spawn — no command needed.
    let (_handle, mut rx) = spawn_executor(ExecutorConfig {
        store,
        providers: Arc::new(SimFactory),
        forge: Arc::new(SimForge),
        git: Arc::new(SimGit),
    });

    let updated = loop {
        let evt = tokio::time::timeout(Duration::from_secs(15), rx.next())
            .await
            .expect("timed out waiting for the poll's card update")
            .expect("event stream closed unexpectedly");
        if let ExecutorEventKind::CardUpdated(c) = evt.kind {
            if c.id == card_id {
                break c;
            }
        }
    };

    // The count that badges the card (the assigned reviewer's comments)...
    assert_eq!(
        updated.reviewer_comment_count, 3,
        "the sim PR's three comments are all from the assigned reviewer"
    );
    // ...and the total that gates the triage button (all reviewers' comments).
    assert_eq!(
        updated.comment_count, 3,
        "the sim PR carries three review comments in total"
    );
    // ...and the list the panel renders, from the same tick.
    let reviews: Vec<_> = updated
        .reviews
        .iter()
        .map(|r| (r.author.as_str(), r.state.as_str()))
        .collect();
    assert_eq!(
        reviews,
        vec![
            ("octocat", "CHANGES_REQUESTED"),
            ("gemini-code-assist", "COMMENTED"),
        ],
        "the poll must land the submitted reviews on the card, not leave them for a manual refresh"
    );
}
