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

    // Only the tail is rendered. A long run's transcript reaches tens of
    // thousands of lines, and cloning *and* re-diffing all of them on every
    // arriving line is O(n) work per line on the main thread — which is what
    // pushes the input round trip past the typing interval (see `src/stress.rs`).
    // Cloning the window rather than the whole vec bounds that cost, and the
    // absolute line number as key means an append slides the window by one node
    // instead of rebuilding it.
    let cap = crate::stress::transcript_cap();
    let (hidden, lines) = {
        let map = state.transcripts.read();
        let all = map.get(&id).map(Vec::as_slice).unwrap_or_default();
        let hidden = if cap == 0 {
            0
        } else {
            all.len().saturating_sub(cap)
        };
        (hidden, all[hidden..].to_vec())
    };
    if lines.is_empty() {
        return rsx! {};
    }
    rsx! {
        div { class: "section",
            h3 { "Activity" }
            div { class: "transcript", id: "transcript-{id}",
                if cap > 0 {
                    if hidden > 0 {
                        div { class: "line", span { class: "ts", "… {hidden} earlier lines" } }
                    }
                    for (i, (ts, line)) in lines.iter().enumerate() {
                        div { key: "{hidden + i}", class: "line",
                            span { class: "ts", "{fmt_time(*ts)} " }
                            span { "{line}" }
                        }
                    }
                } else {
                    // Unkeyed, uncapped: the pre-fix rendering, kept so the
                    // harness can measure against the real baseline.
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
}

/// Format a unix-millis timestamp as local `HH:MM:SS`.
fn fmt_time(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_millis_opt(ms).single() {
        Some(dt) => dt.format("%H:%M:%S").to_string(),
        None => String::new(),
    }
}
