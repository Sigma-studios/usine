//! Usine desktop app (Dioxus). Thin reactive view over `usine-core`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod state;
#[cfg(debug_assertions)]
mod stress;
/// Release stand-in for the debug-only keystroke-drop harness: the fixes it
/// toggles are always on outside a measurement run.
#[cfg(not(debug_assertions))]
mod stress {
    pub fn fix_a() -> bool {
        true
    }
    pub fn transcript_cap() -> usize {
        500
    }
    pub fn use_stress(_state: crate::state::AppState) {}
    pub fn record_chat_input(_v: &str) {}
}
mod toast;
mod ui;

use dioxus::prelude::*;
use futures::StreamExt;

use state::AppState;
use toast::ToastHost;
use ui::{
    AdoptDialogHost, BoardArea, CardMenuHost, ConfirmHost, DetailArea, DiffDialogHost,
    ProjectSettingsModal, SearchHost, SettingsModal, Sidebar, UsageBar,
};

const CSS: &str = include_str!("style.css");

/// The in-app logo, embedded as a base64 `image/svg+xml` data URI. `logo.svg` is
/// the single source asset: the dock/taskbar PNG is rasterized from it at build
/// time (see `build.rs`), which adds the white interior disk. In the sidebar the
/// `filter: invert(1)` rule renders this transparent black art as white-on-clear.
pub static LOGO_URI: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(include_str!("../assets/logo.svg"));
    format!("data:image/svg+xml;base64,{b64}")
});

/// The filled icon PNG, rasterized from `logo.svg` by `build.rs` (dock + window
/// icon want raster pixels, not SVG).
const ICON_PNG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/logo_filled.png"));

/// The user-visible app name. Sim/demo windows are labeled so a test instance
/// is never mistaken for the real one — in the window title, and in the macOS
/// Dock via `set_macos_display_name` (Windows/Linux taskbars show the window
/// title).
fn app_display_name() -> &'static str {
    if state::demo_mode() {
        "Usine (test)"
    } else {
        "Usine"
    }
}

fn main() {
    // Installed `usine`, launched from a shell: re-exec once into a detached
    // background session so the prompt returns immediately and the window
    // outlives the terminal. No-op in dev/hot-reload builds and on the re-exec'd
    // child — see `detach_from_terminal`.
    detach_from_terminal();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,usine_core=info,usine=info".into()),
        )
        .try_init();

    use dioxus::desktop::{Config, WindowBuilder};
    let mut window = WindowBuilder::new().with_title(app_display_name());
    if let Some(icon) = load_window_icon() {
        window = window.with_window_icon(Some(icon));
    }
    let cfg = Config::new().with_window(window);
    // Dioxus builds a default "Window / Edit" menu bar for every platform. On
    // macOS that lives in the system menu bar and carries the standard
    // Cmd+C/V/Q shortcuts, so it stays; on Linux/Windows it is drawn *inside*
    // the window as a GTK/Win32 menu strip above our own chrome, which looks
    // out of place and does nothing the app doesn't already do (the webview
    // handles clipboard keys itself). Drop it there — except in debug builds,
    // where the same menu carries the Help entries for devtools and float on
    // top.
    #[cfg(all(not(target_os = "macos"), not(debug_assertions)))]
    let cfg = cfg.with_menu(None);
    dioxus::LaunchBuilder::desktop().with_cfg(cfg).launch(App);
}

/// Make the installed `usine` command act like a detached GUI launcher: when run
/// from a shell it re-execs itself into a fresh session with stdio sent to
/// `/dev/null`, then the original process exits so the prompt returns at once and
/// closing the terminal won't take the app down with it.
///
/// - Only the release binary detaches; debug builds (`cargo run`, `dx serve`)
///   stay in the foreground so hot-reload and live logs keep working.
/// - The re-exec'd child sets `USINE_DETACHED=1` so it skips straight to the app.
/// - `USINE_NO_DETACH=1` forces the foreground (e.g. to watch logs live).
/// - Windows needs nothing here: the `windows_subsystem = "windows"` attribute
///   already makes a GUI-subsystem binary hand control back to the shell at once.
#[cfg(unix)]
fn detach_from_terminal() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    if cfg!(debug_assertions)
        || std::env::var_os("USINE_DETACHED").is_some()
        || std::env::var_os("USINE_NO_DETACH").is_some()
    {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.args(std::env::args_os().skip(1))
        .env("USINE_DETACHED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Start a new session in the child (after fork, before exec) so it has no
    // controlling terminal and survives the launching shell exiting.
    unsafe {
        cmd.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }
    if cmd.spawn().is_ok() {
        std::process::exit(0);
    }
    // Spawn failed — fall through and just run in the foreground.
}

#[cfg(not(unix))]
fn detach_from_terminal() {}

/// Decode the embedded filled logo into a taskbar/window icon (Windows/Linux;
/// macOS ignores the window icon — the dock icon is set separately at startup).
fn load_window_icon() -> Option<dioxus::desktop::tao::window::Icon> {
    let img = image::load_from_memory(ICON_PNG)
        .ok()?
        .resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    dioxus::desktop::tao::window::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// Set the macOS dock icon at runtime to the filled logo (the window icon is
/// ignored there, and we aren't shipping a `.app` bundle).
#[cfg(target_os = "macos")]
fn set_macos_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(ICON_PNG);
    let image = NSImage::initWithData(NSImage::alloc(), &data);
    if let Some(image) = image {
        let app = NSApplication::sharedApplication(mtm);
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

/// Rename the running process in the macOS Dock / Cmd+Tab switcher. We don't
/// ship a `.app` bundle, so the Dock name comes from the LaunchServices
/// registration and the only way to change it at runtime is the private
/// LaunchServices SPI (the same one Chromium and the JDK use). Every symbol is
/// resolved via `dlsym` and null-checked, so on a macOS that drops the SPI this
/// silently does nothing and only the window title carries the label.
///
/// Returns whether the rename call succeeded. The process has to be registered
/// with LaunchServices first, and that checkin can lag window creation by a
/// while on a cold debug start — so the caller retries until this reports
/// success.
#[cfg(target_os = "macos")]
fn set_macos_display_name(name: &str) -> bool {
    use std::ffi::{c_int, c_void, CStr};

    type LSGetCurrentApplicationASN = unsafe extern "C" fn() -> *const c_void;
    type LSSetApplicationInformationItem = unsafe extern "C" fn(
        c_int,
        *const c_void,
        *const c_void,
        *const c_void,
        *mut *const c_void,
    ) -> c_int;

    unsafe {
        let handle = libc::dlopen(
            c"/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/LaunchServices".as_ptr(),
            libc::RTLD_LAZY,
        );
        if handle.is_null() {
            return false;
        }
        let sym = |name: &CStr| libc::dlsym(handle, name.as_ptr());
        let get_asn = sym(c"_LSGetCurrentApplicationASN");
        let set_item = sym(c"_LSSetApplicationInformationItem");
        // A `CFStringRef const` global — dlsym gives its address, so deref once.
        let display_name_key = sym(c"_kLSDisplayNameKey") as *const *const c_void;
        if get_asn.is_null() || set_item.is_null() || display_name_key.is_null() {
            return false;
        }
        let get_asn: LSGetCurrentApplicationASN = std::mem::transmute(get_asn);
        let set_item: LSSetApplicationInformationItem = std::mem::transmute(set_item);

        // The ASN and the dereferenced key come from the SPI too — treat them
        // as just as untrustworthy as the symbols themselves. A null ASN also
        // means "not checked in with LaunchServices yet", i.e. retry later.
        let asn = get_asn();
        let key = *display_name_key;
        if asn.is_null() || key.is_null() {
            return false;
        }

        // NSString is toll-free bridged to CFString, so its pointer passes
        // straight through as the CFStringRef value.
        let ns_name = objc2_foundation::NSString::from_str(name);
        let name_ptr: *const c_void = (&raw const *ns_name).cast();
        const K_LS_DEFAULT_SESSION_ID: c_int = -2;
        set_item(
            K_LS_DEFAULT_SESSION_ID,
            asn,
            key,
            name_ptr,
            std::ptr::null_mut(),
        ) == 0
    }
}

/// Reflect the number of cards waiting on the user as the macOS dock badge —
/// the little red circle. Reactive: re-runs whenever the card list changes and
/// clears the badge when nothing needs attention. Uses tao's
/// [`WindowExtMacOS::set_badge_label`] (cross-platform tao has no single badge
/// API — it's `set_badge_label` on macOS, `set_badge_count` on Unix — so this
/// is macOS-only for now). AppKit requires the main thread, which is where
/// Dioxus runs effects.
#[cfg(target_os = "macos")]
fn use_dock_badge(state: AppState) {
    use dioxus::desktop::tao::platform::macos::WindowExtMacOS;
    let window = dioxus::desktop::window();
    use_effect(move || {
        let cards = state
            .cards
            .read()
            .iter()
            .filter(|c| c.needs_attention())
            .count();
        // PRs waiting on the user's review count toward the badge too.
        let reviews = state.review_attention_count();
        let count = cards + reviews;
        window
            .window
            .set_badge_label((count > 0).then(|| count.to_string()));
    });
}

#[component]
fn App() -> Element {
    use dioxus::desktop::{tao::event::Event, use_wry_event_handler, WindowEvent};

    let state = use_context_provider(AppState::init);

    #[cfg(target_os = "macos")]
    use_hook(set_macos_dock_icon);

    // Label the Dock / Cmd+Tab entry of a sim window. LaunchServices checkin
    // can lag well behind the first render, and a rename attempted before it
    // is silently lost — so keep retrying until one sticks.
    #[cfg(target_os = "macos")]
    use_future(|| async {
        if !state::demo_mode() {
            return;
        }
        for _ in 0..60 {
            if set_macos_display_name(app_display_name()) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    });

    // Show a dock badge whenever cards are waiting on the user.
    #[cfg(target_os = "macos")]
    use_dock_badge(state);

    // Reap running previews as the app closes. Previews run in their own process
    // groups (so a preview's whole tree can be reaped), which also detaches them
    // from the app's group — without this they outlive the app, holding ports and
    // docker infra with no handle left to stop them. Fires on window close and on
    // loop-destroyed (covers macOS Cmd+Q, which skips CloseRequested). Captures an
    // owned handle so it never reads a signal while the runtime is tearing down.
    let exec = state.executor_handle();
    use_wry_event_handler(move |event, _| {
        if matches!(
            event,
            Event::LoopDestroyed
                | Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                }
        ) {
            exec.shutdown();
        }
    });

    // Debug-only keystroke-drop harness (inert unless `USINE_STRESS=1`).
    stress::use_stress(state);

    // Drain executor events into signals — the single reduce point.
    use_future(move || async move {
        if let Some(mut rx) = state.take_event_rx() {
            while let Some(evt) = rx.next().await {
                state.apply_event(evt);
            }
        }
    });

    rsx! {
        style { dangerous_inner_html: CSS }
        div { class: "app-shell",
            div { class: "app",
                Sidebar {}
                BoardArea {}
                DetailArea {}
            }
            UsageBar {}
        }
        ToastHost {}
        SearchHost {}
        CardMenuHost {}
        ConfirmHost {}
        AdoptDialogHost {}
        DiffDialogHost {}
        SettingsModal {}
        ProjectSettingsModal {}
    }
}
