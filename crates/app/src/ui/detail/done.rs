//! What a finished card leaves behind. Done is where a card spends the rest of
//! its life, and every panel that rendered the run's output belongs to an
//! earlier state — so without this the panel shows the prompt and nothing else.
//! Everything here is read back from records that already survive the merge.

use dioxus::prelude::*;
use usine_core::Card;
use uuid::Uuid;

use super::{FixOutcomes, HandoffPanel, PrLink};
use crate::state::AppState;
use crate::ui::widgets::ArtifactText;

/// The outcome of a merged card: where the work landed, and what the run said
/// about it. Rendered only for a card that has a PR — a card marked done without
/// one (an investigation, say) has nothing to show here and keeps its old panel.
#[component]
pub(super) fn DonePanel(card: Card) -> Element {
    let pr = card.pr.clone();
    let branch = card.branch.clone();
    rsx! {
        if let Some(p) = pr {
            div { class: "section",
                h3 { "Outcome" }
                PrLink { number: p.number, url: p.url }
                // A card can also reach Done via "Mark done" with the PR still
                // open, so report the record rather than assuming a merge.
                div { class: "card-meta",
                    if p.state == "merged" {
                        span { class: "badge merged", "✓ Merged" }
                    } else {
                        span { class: "badge status", "{p.state}" }
                    }
                }
                if let Some(branch) = branch {
                    div { class: "wt-path", "{branch}" }
                }
            }
            OutcomeArtifacts { card_id: card.id }
        }
    }
}

/// The stored artifacts of the run that produced the work: the implement run's
/// hand-off and the fixes recap. Each is omitted when absent; a card with
/// neither renders nothing.
#[component]
pub(super) fn OutcomeArtifacts(card_id: Uuid) -> Element {
    let state = use_context::<AppState>();
    let handoff = state.handoffs.read().get(&card_id).cloned();
    let recap = state.review_recaps.read().get(&card_id).cloned();
    let has_fixes = recap.is_some() || state.fix_reports.read().contains_key(&card_id);
    rsx! {
        if let Some(handoff) = handoff {
            HandoffPanel { card_id, handoff }
        }
        if has_fixes {
            div { class: "section",
                h3 { "Fixes" }
                FixOutcomes { card_id }
                if let Some(recap) = recap {
                    div { class: "hint", "Fixes recap" }
                    ArtifactText { text: recap }
                }
            }
        }
    }
}
