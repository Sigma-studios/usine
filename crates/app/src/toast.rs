//! Global toast queue + host component.
//!
//! First-class error handling: any failure in the core surfaces here as a
//! toast, and successful side-effecting actions confirm the same way. The queue
//! is a `GlobalSignal` so it can be pushed to from anywhere (event reducer,
//! handlers) without threading a handle through every component.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dioxus::prelude::*;
use usine_core::Severity;

#[derive(Clone, PartialEq)]
pub struct Toast {
    pub id: u64,
    pub severity: Severity,
    pub message: String,
}

/// Keep the queue bounded so toasts can't pile up indefinitely.
const MAX_TOASTS: usize = 5;

/// Info/success toasts auto-dismiss after this long. Errors and warnings persist
/// until the user dismisses them by hand, so a problem is never missed.
const TOAST_TTL: Duration = Duration::from_secs(10);

pub static TOASTS: GlobalSignal<Vec<Toast>> = Signal::global(Vec::new);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn push_toast(severity: Severity, message: impl Into<String>) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut toasts = TOASTS.write();
    toasts.push(Toast {
        id,
        severity,
        message: message.into(),
    });
    let len = toasts.len();
    if len > MAX_TOASTS {
        toasts.drain(0..len - MAX_TOASTS);
    }
}

pub fn dismiss_toast(id: u64) {
    TOASTS.write().retain(|t| t.id != id);
}

fn severity_class(sev: Severity) -> &'static str {
    match sev {
        Severity::Info => "toast info",
        Severity::Success => "toast success",
        Severity::Warning => "toast warning",
        Severity::Error => "toast error",
    }
}

fn severity_icon(sev: Severity) -> &'static str {
    match sev {
        Severity::Info => "ℹ",
        Severity::Success => "✓",
        Severity::Warning => "⚠",
        Severity::Error => "✕",
    }
}

#[component]
pub fn ToastHost() -> Element {
    rsx! {
        div { class: "toast-host",
            for toast in TOASTS.read().iter().cloned() {
                ToastItem { key: "{toast.id}", toast }
            }
        }
    }
}

/// A single toast. On mount it schedules its own dismissal after [`TOAST_TTL`]
/// unless it's an error or warning (those stay until dismissed by hand). The
/// timer future is tied to this scope, so a manual dismiss unmounts the
/// component and drops the pending timer.
#[component]
fn ToastItem(toast: Toast) -> Element {
    let id = toast.id;
    let severity = toast.severity;
    // `use_future` runs once on mount (no reactive reads here); errors and
    // warnings return immediately without arming the timer.
    use_future(move || async move {
        if !matches!(severity, Severity::Error | Severity::Warning) {
            tokio::time::sleep(TOAST_TTL).await;
            dismiss_toast(id);
        }
    });

    rsx! {
        div { class: severity_class(severity),
            span { class: "toast-icon", {severity_icon(severity)} }
            span { class: "toast-msg", "{toast.message}" }
            button {
                class: "toast-close",
                "aria-label": "Dismiss notification",
                onclick: move |_| dismiss_toast(id),
                "×"
            }
        }
    }
}
