//! Drag-to-resize handles for the two side panels (projects sidebar, card /
//! review detail) and the bridge that keeps their widths alive.
//!
//! Two layers. The *live* one is a CSS custom property (`--sidebar-w`,
//! `--detail-w`) that JS writes inline on `<html>` while a drag is in flight:
//! Dioxus renders into `#main` and never touches that node, so the executor's
//! event stream can re-render the (expensive) detail panel mid-drag without
//! snapping the edge back. The *persisted* one is [`AppSettings`], written once
//! on pointer-up through the ordinary `SaveSettings` → `SettingsUpdated` path
//! and rendered back as a `:root` block, so there's no first-frame flash.
//!
//! The rule tying them together: anything that changes a width outside a drag
//! (keyboard nudge, double-click reset) must set the inline var *as well as*
//! saving, or the stale drag value would keep winning over the `:root` sheet.
//!
//! The listener is document-level and delegated (same pattern as
//! [`crate::ui::search`]) because a re-render can replace the handle node
//! mid-drag — element-local handlers and pointer capture would die with it.

use dioxus::prelude::*;
use serde::Deserialize;

use crate::state::AppState;

/// Which side panel a handle resizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Sidebar,
    Detail,
}

/// Per-panel geometry, shared by the Rust clamp and the generated JS so the two
/// can't drift apart.
struct PanelSpec {
    /// Value of the `data-resize` attribute, and the tag sent back over the
    /// eval channel.
    key: &'static str,
    /// CSS custom property carrying the live width.
    var: &'static str,
    /// Selector of the element being resized (used by JS to read its start
    /// width — the two `.detail` sites are the same slot on screen).
    selector: &'static str,
    min: u32,
    max: u32,
    default: u32,
    /// Share of the viewport the panel may never exceed, mirroring the `vw` cap
    /// in the stylesheet: a saved width must not make the board unreachable on
    /// a small window.
    viewport_cap: f64,
    label: &'static str,
}

const SIDEBAR: PanelSpec = PanelSpec {
    key: "sidebar",
    var: "--sidebar-w",
    selector: ".sidebar",
    min: 160,
    max: 420,
    default: 200,
    viewport_cap: 0.33,
    label: "Resize the projects sidebar",
};

const DETAIL: PanelSpec = PanelSpec {
    key: "detail",
    var: "--detail-w",
    selector: ".detail",
    min: 300,
    max: 800,
    default: 380,
    viewport_cap: 0.5,
    label: "Resize the detail panel",
};

impl Panel {
    fn spec(self) -> &'static PanelSpec {
        match self {
            Panel::Sidebar => &SIDEBAR,
            Panel::Detail => &DETAIL,
        }
    }
}

/// Clamp to the panel's absolute bounds. The viewport cap lives in CSS (and in
/// the drag JS) only — it depends on the current window size, which must not
/// leak into the stored value.
fn clamp(spec: &PanelSpec, width: u32) -> u32 {
    width.clamp(spec.min, spec.max)
}

/// What the drag JS sends back on pointer-up / double-click.
#[derive(Deserialize)]
struct ResizeMsg {
    panel: String,
    width: u32,
}

/// Push a width straight into the live layer, for the paths that don't go
/// through a drag (keyboard, reset). Without this the inline var left by an
/// earlier drag would outrank the `:root` block the save re-renders.
fn set_live(spec: &PanelSpec, width: u32) {
    let var = spec.var;
    dioxus::document::eval(&format!(
        "document.documentElement.style.setProperty('{var}', '{width}px');"
    ));
}

/// Store `width` for `panel` in the global settings (clamped), and mirror it
/// into the live layer.
fn apply(state: &AppState, panel: Panel, width: u32, live: bool) {
    let spec = panel.spec();
    let width = clamp(spec, width);
    let mut s = state.settings.read().clone();
    match panel {
        Panel::Sidebar => s.sidebar_width = width,
        Panel::Detail => s.detail_width = width,
    }
    if live {
        set_live(spec, width);
    }
    state.save_settings(s);
}

fn current(state: &AppState, panel: Panel) -> u32 {
    let s = state.settings.read();
    clamp(
        panel.spec(),
        match panel {
            Panel::Sidebar => s.sidebar_width,
            Panel::Detail => s.detail_width,
        },
    )
}

/// The grab strip on a panel's inner edge. Mouse dragging is handled by the
/// delegated listener in [`PanelResizeHost`]; this component owns only the
/// keyboard affordances, so the handle stays reachable without a pointer.
#[component]
pub fn PanelResizer(panel: Panel) -> Element {
    let state = use_context::<AppState>();
    let spec = panel.spec();
    let class = match panel {
        Panel::Sidebar => "panel-resizer panel-resizer-right has-tip",
        Panel::Detail => "panel-resizer panel-resizer-left has-tip",
    };

    rsx! {
        div {
            class: "{class}",
            "data-resize": "{spec.key}",
            role: "separator",
            "aria-orientation": "vertical",
            "aria-label": "{spec.label}",
            tabindex: "0",
            onkeydown: move |e: KeyboardEvent| {
                // Arrows nudge (Shift = coarse), Home restores the default.
                let step = if e.modifiers().shift() { 64 } else { 16 };
                let sign = if panel == Panel::Sidebar { 1 } else { -1 };
                let width = match e.key() {
                    Key::ArrowLeft => (current(&state, panel) as i64 - sign * step).max(0) as u32,
                    Key::ArrowRight => (current(&state, panel) as i64 + sign * step).max(0) as u32,
                    Key::Home => panel.spec().default,
                    _ => return,
                };
                e.prevent_default();
                apply(&state, panel, width, true);
            },
            // Hover hint. Not a native `title`: macOS WKWebView ignores that
            // attribute, so the codebase's `.info-tip` pattern is used instead
            // (the tip is `pointer-events: none`, so it can't swallow a grab).
            span { class: "info-tip", "Drag to resize · double-click to reset" }
        }
    }
}

/// Mounted once at the app root: renders the persisted widths as `:root` custom
/// properties and runs the document-level drag listener.
#[component]
pub fn PanelResizeHost() -> Element {
    let state = use_context::<AppState>();

    use_future(move || async move {
        let js = drag_js();
        // The eval channel can die (e.g. a webview reload during dev); re-eval
        // for a fresh one. The JS replaces any listener a previous eval left
        // behind, so re-registering never stacks handlers.
        loop {
            let mut listener = dioxus::document::eval(&js);
            while let Ok(msg) = listener.recv::<ResizeMsg>().await {
                let panel = if msg.panel == SIDEBAR.key {
                    Panel::Sidebar
                } else {
                    Panel::Detail
                };
                // The drag (or the dblclick reset) has already written the live
                // var; only the store needs catching up.
                apply(&state, panel, msg.width, false);
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    });

    let sw = current(&state, Panel::Sidebar);
    let dw = current(&state, Panel::Detail);
    rsx! {
        style {
            dangerous_inner_html: ":root{{--sidebar-w:{sw}px;--detail-w:{dw}px;}}",
        }
    }
}

/// The delegated drag listener. `pointermove`/`pointerup` go on `window`, not on
/// the handle: a re-render can swap the handle node out mid-drag. `pointercancel`
/// and window `blur` also end the drag, so releasing outside the webview can't
/// leave `body.resizing` stuck on.
fn drag_js() -> String {
    let (s, d) = (&SIDEBAR, &DETAIL);
    format!(
        "(function(){{\
           var SPECS = {{\
             '{skey}': {{ v: '{svar}', sel: '{ssel}', min: {smin}, max: {smax}, def: {sdef}, cap: {scap}, sign: 1 }},\
             '{dkey}': {{ v: '{dvar}', sel: '{dsel}', min: {dmin}, max: {dmax}, def: {ddef}, cap: {dcap}, sign: -1 }}\
           }};\
           function clamp(sp, w) {{\
             var max = Math.min(sp.max, Math.floor(window.innerWidth * sp.cap));\
             return Math.round(Math.max(sp.min, Math.min(Math.max(max, sp.min), w)));\
           }}\
           function live(sp, w) {{\
             document.documentElement.style.setProperty(sp.v, w + 'px');\
           }}\
           if (window.__usineResizeDown) {{\
             document.removeEventListener('pointerdown', window.__usineResizeDown);\
             document.removeEventListener('dblclick', window.__usineResizeDbl);\
           }}\
           window.__usineResizeDown = function(e) {{\
             if (e.button !== 0 || !e.target.closest) return;\
             var h = e.target.closest('[data-resize]');\
             if (!h) return;\
             var key = h.getAttribute('data-resize');\
             var sp = SPECS[key];\
             var el = sp && document.querySelector(sp.sel);\
             if (!el) return;\
             e.preventDefault();\
             var startX = e.clientX, startW = el.getBoundingClientRect().width;\
             var last = null;\
             document.body.classList.add('resizing', 'resizing-' + key);\
             function move(ev) {{\
               last = clamp(sp, startW + sp.sign * (ev.clientX - startX));\
               live(sp, last);\
             }}\
             function end() {{\
               window.removeEventListener('pointermove', move);\
               window.removeEventListener('pointerup', end);\
               window.removeEventListener('pointercancel', end);\
               window.removeEventListener('blur', end);\
               document.body.classList.remove('resizing', 'resizing-' + key);\
               /* `startW` is the *rendered* width, which the vw cap may have\
                  shrunk below the stored one; persisting it after a plain click\
                  would quietly clamp the saved width down for good. Only a real\
                  move produces a width worth saving. */\
               if (last !== null) dioxus.send({{ panel: key, width: last }});\
             }}\
             window.addEventListener('pointermove', move);\
             window.addEventListener('pointerup', end);\
             window.addEventListener('pointercancel', end);\
             window.addEventListener('blur', end);\
           }};\
           window.__usineResizeDbl = function(e) {{\
             if (!e.target.closest) return;\
             var h = e.target.closest('[data-resize]');\
             if (!h) return;\
             var sp = SPECS[h.getAttribute('data-resize')];\
             if (!sp) return;\
             e.preventDefault();\
             live(sp, sp.def);\
             dioxus.send({{ panel: h.getAttribute('data-resize'), width: sp.def }});\
           }};\
           document.addEventListener('pointerdown', window.__usineResizeDown);\
           document.addEventListener('dblclick', window.__usineResizeDbl);\
         }})();",
        skey = s.key, svar = s.var, ssel = s.selector, smin = s.min, smax = s.max,
        sdef = s.default, scap = s.viewport_cap,
        dkey = d.key, dvar = d.var, dsel = d.selector, dmin = d.min, dmax = d.max,
        ddef = d.default, dcap = d.viewport_cap,
    )
}
