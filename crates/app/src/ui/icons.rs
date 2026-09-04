//! The line icons used on cards and in the detail header.
//!
//! All are 16×16, drawn on a 24-unit grid with `currentColor` strokes, so they
//! inherit the button's color and hover state rather than carrying their own.

use dioxus::prelude::*;

/// The shared `<svg>` shell: only the paths differ between icons.
#[component]
fn Glyph(children: Element) -> Element {
    rsx! {
        svg {
            width: "16",
            height: "16",
            view_box: "0 0 24 24",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            {children}
        }
    }
}

/// Start the app.
#[component]
pub(super) fn IconPlay() -> Element {
    rsx! {
        Glyph { polygon { points: "6 4 20 12 6 20 6 4" } }
    }
}

/// Stop the running app.
#[component]
pub(super) fn IconStop() -> Element {
    rsx! {
        Glyph { rect { x: "6", y: "6", width: "12", height: "12", rx: "2" } }
    }
}

/// Open the running app in a browser.
#[component]
pub(super) fn IconExternal() -> Element {
    rsx! {
        Glyph {
            path { d: "M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" }
            polyline { points: "15 3 21 3 21 9" }
            line { x1: "10", y1: "14", x2: "21", y2: "3" }
        }
    }
}

/// Show the card's diff: an added line over a removed one.
#[component]
pub(super) fn IconDiff() -> Element {
    rsx! {
        Glyph {
            path { d: "M4 8h4M6 6v4" }
            line { x1: "11", y1: "8", x2: "20", y2: "8" }
            line { x1: "4", y1: "16", x2: "8", y2: "16" }
            line { x1: "11", y1: "16", x2: "20", y2: "16" }
        }
    }
}

/// Attach a file to the card.
#[component]
pub(super) fn IconPaperclip() -> Element {
    rsx! {
        Glyph {
            path { d: "M21.44 11.05l-9.19 9.19a6 6 0 0 1-8.49-8.49l9.19-9.19a4 4 0 0 1 5.66 5.66l-9.2 9.19a2 2 0 0 1-2.83-2.83l8.49-8.48" }
        }
    }
}

/// Fold a board lane to a rail (rotated 180° by CSS for the expand direction).
#[component]
pub(super) fn IconChevronLeft() -> Element {
    rsx! {
        Glyph { polyline { points: "15 18 9 12 15 6" } }
    }
}
