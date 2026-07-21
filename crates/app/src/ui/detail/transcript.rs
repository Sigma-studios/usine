//! The activity transcript feed for a card or a review task (the transcript map
//! is keyed by either entity's id), with bottom-stick autoscroll.

use dioxus::prelude::*;
use uuid::Uuid;

use crate::state::AppState;

#[component]
pub(super) fn TranscriptView(id: Uuid) -> Element {
    let state = use_context::<AppState>();

    // Keep the feed pinned to the bottom as new lines arrive, unless the user has
    // scrolled up. A small bit of JS tracks "at bottom" on scroll and tails the
    // feed when it is. This effect re-runs whenever the transcript changes.
    use_effect(move || {
        let _ = state.transcripts.read().get(&id).map(|v| v.len());
        let js = format!(
            "(function(){{var el=document.getElementById('transcript-{id}');if(!el)return;\
             if(!el.dataset.stickInit){{el.dataset.stickInit='1';el.dataset.stick='1';\
             el.addEventListener('scroll',function(){{el.dataset.stick=((el.scrollHeight-el.scrollTop-el.clientHeight)<30)?'1':'0';}});}}\
             if(el.dataset.stick==='1'){{el.scrollTop=el.scrollHeight;}}}})();"
        );
        dioxus::document::eval(&js);
    });

    let lines = state
        .transcripts
        .read()
        .get(&id)
        .cloned()
        .unwrap_or_default();
    if lines.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "section",
            h3 { "Activity" }
            div { class: "transcript", id: "transcript-{id}",
                for (ts, line) in lines.iter() {
                    div { class: "line",
                        span { class: "ts", "{fmt_time(*ts)} " }
                        span { "{line}" }
                    }
                }
            }
        }
    }
}

/// Format a unix-millis timestamp as local `HH:MM:SS`.
fn fmt_time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%H:%M:%S").to_string(),
        None => String::new(),
    }
}
