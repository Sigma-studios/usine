//! App-level keyboard shortcuts that no single component owns. Right now that
//! is one: Escape closes the detail panel.
//!
//! Modelled on `search.rs`'s document-level listener rather than an element
//! `onkeydown`, because focus is usually on `<body>` (or on a card the user
//! clicked), where a component's handler never sees the key — which is exactly
//! why the panel had no way out but its `×`.

use dioxus::prelude::*;

use crate::state::AppState;

#[component]
pub fn ShortcutHost() -> Element {
    let state = use_context::<AppState>();

    use_future(move || async move {
        // Defers to anything that owns the keyboard already: a modal handles
        // its own Escape (and closing the panel underneath it would be a
        // surprise), and so does the search box.
        let js = "if (window.__usineEscKeydown)\
                    document.removeEventListener('keydown', window.__usineEscKeydown);\
                  window.__usineEscKeydown = function(e){\
                    if (e.key !== 'Escape') return;\
                    if (document.querySelector('.modal-overlay')) return;\
                    if (document.getElementById('card-search-input')) return;\
                    dioxus.send(true);\
                  };\
                  document.addEventListener('keydown', window.__usineEscKeydown);";
        // The eval channel can die (e.g. a webview reload during dev); re-eval
        // to get a fresh one. The JS replaces any listener a previous eval left
        // behind, so re-registering never stacks handlers.
        loop {
            let mut listener = dioxus::document::eval(js);
            while listener.recv::<bool>().await.is_ok() {
                state.select_card(None);
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    rsx! {}
}
