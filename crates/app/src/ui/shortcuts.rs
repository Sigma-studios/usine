//! App-level keyboard shortcuts that no single component owns:
//!  - Escape closes the topmost dismissable thing — an open card menu first,
//!    otherwise the detail panel.
//!  - ⌘↩ (Ctrl+↩ off macOS) presses a form's main button from anywhere in the
//!    form. A form opts in with two plain attributes: `data-submit-scope` on
//!    the dialog / section, and `data-submit` on the button to press. See
//!    [`submit_hint`] for the label to show in that button's tooltip.
//!
//! Modelled on `search.rs`'s document-level listener rather than an element
//! `onkeydown`, because focus is usually on `<body>` (or on a card the user
//! clicked), where a component's handler never sees the key — which is exactly
//! why the panel had no way out but its `×`.

use dioxus::prelude::*;

use crate::state::AppState;
use crate::ui::cardmenu::{card_menu_open, close_card_menu};

/// The submit shortcut as shown to the user, for `data-submit` tooltips.
pub(crate) fn submit_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘↩"
    } else {
        "Ctrl+↩"
    }
}

/// The ⌘↩ listener. Plain JS that never talks to Rust: it clicks the scope's
/// `[data-submit]` button, so that button's `onclick` stays the only submit
/// logic and its `disabled` gate (`can_submit`, `branch_ready`, …) holds.
///
/// It blurs the focused field before clicking: the card description saves on
/// `change` (fired by blur) and Start reads the saved card, so without the
/// blur a keyboard Start would launch with a stale description. Blur and click
/// reach Rust in that order — same as a mouse click.
fn submit_listener_js() -> String {
    // Cmd only on macOS — Ctrl+Enter must stay free there (as in `search.rs`).
    let modifier = if cfg!(target_os = "macos") {
        "e.metaKey && !e.ctrlKey"
    } else {
        "e.ctrlKey && !e.metaKey"
    };
    format!(
        "if (window.__usineSubmitKeydown)\
           document.removeEventListener('keydown', window.__usineSubmitKeydown);\
         window.__usineSubmitKeydown = function(e){{\
           if (e.key !== 'Enter' || !({modifier}) || e.shiftKey || e.altKey) return;\
           if (e.isComposing || e.repeat) return;\
           var a = document.activeElement;\
           var scope = a && a.closest ? a.closest('[data-submit-scope]') : null;\
           if (!scope) return;\
           var overlay = document.querySelector('.modal-overlay');\
           if (overlay && !overlay.contains(scope)) return;\
           var btn = scope.querySelector('[data-submit]');\
           if (!btn || btn.disabled) return;\
           e.preventDefault();\
           if (a !== scope && a.blur) a.blur();\
           btn.click();\
         }};\
         document.addEventListener('keydown', window.__usineSubmitKeydown);"
    )
}

#[component]
pub fn ShortcutHost() -> Element {
    let state = use_context::<AppState>();

    use_future(move || async move {
        // Defers to anything that owns the keyboard already:
        //  - a modal handles its own Escape (and closing the panel underneath
        //    it would be a surprise);
        //  - a focused text field does too — the panel's title input binds
        //    Escape to revert the rename, and a global close would fire on the
        //    same keystroke and take the panel with it. Any textarea (chat,
        //    PR body, answer box) is the same story: Escape there must not
        //    close the surface being typed into. This also covers the search
        //    box, which owns its own Escape.
        // The card menu has no Escape of its own, so this handler closes it —
        // it floats above the panel, and Escape should peel off the top layer.
        let js = "if (window.__usineEscKeydown)\
                    document.removeEventListener('keydown', window.__usineEscKeydown);\
                  window.__usineEscKeydown = function(e){\
                    if (e.key !== 'Escape') return;\
                    if (document.querySelector('.modal-overlay')) return;\
                    var a = document.activeElement;\
                    if (a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA'\
                              || a.tagName === 'SELECT' || a.isContentEditable)) return;\
                    dioxus.send(true);\
                  };\
                  document.addEventListener('keydown', window.__usineEscKeydown);";
        // Registered on the same eval so a webview reload re-installs both.
        let js = format!("{}{js}", submit_listener_js());
        // The eval channel can die (e.g. a webview reload during dev); re-eval
        // to get a fresh one. The JS replaces any listener a previous eval left
        // behind, so re-registering never stacks handlers.
        loop {
            let mut listener = dioxus::document::eval(&js);
            while listener.recv::<bool>().await.is_ok() {
                // Topmost first: an open menu is what Escape means to close.
                if card_menu_open() {
                    close_card_menu();
                } else {
                    state.select_card(None);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    rsx! {}
}
