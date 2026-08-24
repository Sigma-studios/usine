//! Browser-style find bar for the board: Cmd+F (Ctrl+F off macOS) pops a small
//! search box
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
    // Register the global shortcut and keep draining its channel for the
    // lifetime of the app. Modals own the keyboard while one is open (the diff
    // viewer has its own Cmd+F-adjacent semantics), so the shortcut defers to
    // any `.modal-overlay` on screen. Escape is also caught here so the box
    // closes even when focus has wandered off the input (e.g. after clicking a
    // card): the listener sends `true` to open/refocus and `false` to close.
    //
    // On macOS the accelerator is Cmd only — Ctrl+F is the Cocoa emacs binding
    // for "forward one character" inside text fields and must pass through.
    use_future(|| async {
        let modifier = if cfg!(target_os = "macos") {
            "e.metaKey && !e.ctrlKey"
        } else {
            "e.ctrlKey && !e.metaKey"
        };
        let js = format!(
            "if (window.__usineSearchKeydown)\
               document.removeEventListener('keydown', window.__usineSearchKeydown);\
             window.__usineSearchKeydown = function(e){{\
               if (document.querySelector('.modal-overlay')) return;\
               if ({modifier} && !e.altKey && !e.shiftKey\
                   && (e.key === 'f' || e.key === 'F')) {{\
                 e.preventDefault();\
                 dioxus.send(true);\
               }} else if (e.key === 'Escape'\
                          && document.getElementById('card-search-input')) {{\
                 e.preventDefault();\
                 dioxus.send(false);\
               }}\
             }};\
             document.addEventListener('keydown', window.__usineSearchKeydown);"
        );
        // The eval channel can die (e.g. a webview reload during dev). Re-eval
        // to get a fresh channel; the JS replaces any listener a previous eval
        // left behind, so re-registering never stacks handlers.
        loop {
            let mut listener = dioxus::document::eval(&js);
            while let Ok(open) = listener.recv::<bool>().await {
                if !open {
                    close();
                } else if SEARCH.read().is_none() {
                    *SEARCH.write() = Some(String::new());
                    // First open is focused by `onmounted` below.
                } else {
                    focus_input();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
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
                // Uncontrolled (see `detail/chat.rs`): the box is minted fresh
                // on every open, so `defaultValue` seeds it and nothing ever
                // needs to push a value back into it.
                initial_value: "{q}",
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
