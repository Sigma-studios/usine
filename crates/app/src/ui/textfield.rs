//! Pushing an app-owned value into an *uncontrolled* text field.
//!
//! Text inputs in this app are uncontrolled (`initial_value`, i.e. the DOM's
//! `defaultValue`) rather than controlled (`value`). A controlled field re-emits
//! a `SetAttribute("value")` patch after every keystroke, and the interpreter
//! applies it a frame late; when macOS turns one keystroke into two DOM
//! mutations — a dead key (`^` then `ê`) or a smart-quote substitution
//! (`"` → `«` `»`) — the first patch is already stale when it lands, so WebKit
//! rewrites the field and parks the caret at the end. See `detail/chat.rs` and
//! commit 790f545 (the same defect, seen as dropped keystrokes under load).
//!
//! The cost of uncontrolled is that writes to `value` after mount are ignored —
//! which is exactly what we want for keystrokes, and exactly wrong for the code
//! paths that *must* put a new value in (a send that clears the box, a plan
//! option that fills it, a probe that prefills a form). The only way back into
//! the DOM is to remount the element, so this hook tracks what the DOM holds and
//! bumps a generation whenever the app-side value diverges for any reason other
//! than the user typing.
//!
//! ```ignore
//! let mut pb = use_push_back(text());
//! // A lone child's `key` is ignored — Dioxus only honors keys among siblings
//! // in a list — so the single-item `for` is what makes the bump remount.
//! rsx! {
//!     for g in [pb.key()] {
//!         input {
//!             key: "{g}",
//!             initial_value: "{text.peek()}",
//!             oninput: move |e| { pb.typed(&e.value()); text.set(e.value()) },
//!         }
//!     }
//! }
//! ```

use dioxus::prelude::*;

/// Handle returned by [`use_push_back`].
#[derive(Clone, Copy, PartialEq)]
pub struct PushBack {
    generation: Signal<u32>,
    in_dom: Signal<String>,
}

/// Watch `current` — the app-side value of an uncontrolled field — and bump a
/// remount generation whenever it changes to something the user did not type.
pub fn use_push_back(current: String) -> PushBack {
    let mut in_dom = use_signal(|| current.clone());
    let mut generation = use_signal(|| 0u32);
    // `use_reactive!` is what makes a non-signal prop drive the effect: without
    // it the closure would capture the first render's `current` forever.
    let latest = use_memo(use_reactive!(|current| current));
    use_effect(move || {
        let c = latest();
        // `peek`, not `read`: the effect must react to `latest` only, so that
        // `typed()` (one call per keystroke) never re-runs it.
        if c != *in_dom.peek() {
            in_dom.set(c);
            generation += 1;
        }
    });
    PushBack { generation, in_dom }
}

impl PushBack {
    /// The element's `key`. Wrap the element in `for g in [pb.key()]`.
    pub fn key(&self) -> u32 {
        (self.generation)()
    }

    /// Call from `oninput`: what the user typed is already in the DOM, so it
    /// must not be pushed back — that would remount the field mid-word.
    pub fn typed(&mut self, v: &str) {
        self.in_dom.set(v.to_string());
    }
}

#[cfg(test)]
mod tests {
    /// The state machine on its own, without a running vdom: `typed` marks a
    /// value as already-in-the-DOM (no remount), anything else remounts.
    fn step(in_dom: &mut String, generation: &mut u32, current: &str) {
        if current != in_dom {
            *in_dom = current.to_string();
            *generation += 1;
        }
    }

    #[test]
    fn typing_never_remounts() {
        let mut in_dom = String::new();
        let mut generation = 0;
        for v in ["h", "he", "hey"] {
            in_dom.clear();
            in_dom.push_str(v); // `typed()`
            step(&mut in_dom, &mut generation, v); // the effect that follows
        }
        assert_eq!(generation, 0);
    }

    #[test]
    fn an_external_write_remounts_once() {
        let (mut in_dom, mut generation) = ("hey".to_string(), 0);
        step(&mut in_dom, &mut generation, ""); // e.g. cleared on send
        assert_eq!(generation, 1);
        step(&mut in_dom, &mut generation, ""); // a later render, same value
        assert_eq!(generation, 1);
    }
}
