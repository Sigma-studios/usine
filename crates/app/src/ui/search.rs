//! Browser-style find bar for the board: Ctrl+F / Cmd+F pops a small search box
//! in the top-right corner that live-filters cards by title. A single global
//! holds the query; `SearchHost` renders the box at the app root, like the toast
//! and confirm hosts, and the boards read `query()` to filter what they render.
//!
//! The shortcut is captured by a document-level JS listener piped back over the
//! eval channel — element-local `onkeydown` handlers can't see keystrokes while
//! focus rests on `<body>`, and tao never receives key events while the webview
//! has focus.

use dioxus::prelude::*;

/// `None` = closed (never filters); `Some(q)` = box open with query `q`.
static SEARCH: GlobalSignal<Option<String>> = Signal::global(|| None);

/// The active query, if the box is open with a non-empty one. Reading this from
/// a component subscribes it, so filtering is live per keystroke.
pub(crate) fn query() -> Option<String> {
    SEARCH.read().clone().filter(|q| !q.trim().is_empty())
}

/// Case-insensitive substring match, same semantics as the diff viewer's file
/// filter.
pub(crate) fn matches(title: &str, q: &str) -> bool {
    title.to_lowercase().contains(&q.trim().to_lowercase())
}

fn close() {
    *SEARCH.write() = None;
}

/// Re-focus the input and select its text — pressing Cmd+F while the box is
/// already open should behave like a browser's find bar.
fn focus_input() {
    dioxus::document::eval(
        "requestAnimationFrame(function(){\
           var el = document.getElementById('card-search-input');\
           if (el) { el.focus(); el.select(); }\
         });",
    );
}

#[component]
pub fn SearchHost() -> Element {
    // Register the global shortcut once and keep draining its channel for the
    // lifetime of the app. Modals own the keyboard while one is open (the diff
    // viewer has its own Cmd+F-adjacent semantics), so the shortcut defers to
    // any `.modal-overlay` on screen.
    use_future(|| async {
        let mut listener = dioxus::document::eval(
            "document.addEventListener('keydown', function(e){\
               if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey\
                   && (e.key === 'f' || e.key === 'F')) {\
                 if (document.querySelector('.modal-overlay')) return;\
                 e.preventDefault();\
                 dioxus.send(true);\
               }\
             });",
        );
        while listener.recv::<bool>().await.is_ok() {
            let already_open = SEARCH.read().is_some();
            if !already_open {
                *SEARCH.write() = Some(String::new());
                // First open is focused by `onmounted` below.
            } else {
                focus_input();
            }
        }
    });

    let open = SEARCH.read().is_some();
    if !open {
        return rsx! {};
    }
    let q = SEARCH.read().clone().unwrap_or_default();

    rsx! {
        div { class: "card-search",
            input {
                id: "card-search-input",
                r#type: "text",
                placeholder: "Search cards…",
                "aria-label": "Search cards",
                value: "{q}",
                oninput: move |e| *SEARCH.write() = Some(e.value()),
                onkeydown: move |e: KeyboardEvent| {
                    if e.key() == Key::Escape {
                        e.prevent_default();
                        close();
                    }
                },
                onmounted: move |e: MountedEvent| {
                    spawn(async move {
                        let _ = e.data().set_focus(true).await;
                    });
                },
            }
            button {
                class: "card-search-close",
                title: "Close search",
                onclick: move |_| close(),
                "✕"
            }
        }
    }
}
