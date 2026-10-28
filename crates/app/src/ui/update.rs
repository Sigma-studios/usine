//! The bottom bar's update control, at its far right: a "Check for updates"
//! button that, once an update is found, grows an "Update when idle" toggle.
//! Turning the toggle on builds the update straight away; only the restart
//! waits, until no card, PR review or session has had work in flight for
//! [`IDLE_GRACE`] — and the user isn't mid-edit: no open dialog, no input for
//! [`INPUT_QUIET`]. (Unsent drafts don't hold it: they're saved and come back
//! after the restart.) The bar is always mounted, so the control owns its state —
//! and the one automatic check per launch, [`LAUNCH_CHECK_DELAY`] in. The
//! process work lives in `crate::update`.

use std::time::Duration;

use dioxus::prelude::*;
use usine_core::{now_millis, Severity};

use crate::state::{demo_mode, AppState};
use crate::toast::push_toast;
use crate::update::{self, CheckOutcome};

/// The launch check waits this long so it never competes with startup.
const LAUNCH_CHECK_DELAY: Duration = Duration::from_secs(15);
/// Idle has to hold this long before a restart, so the gap between
/// back-to-back runs (validation, then its fixes) doesn't count.
const IDLE_GRACE: u32 = 10;
/// The restart also waits until the user has gone this long without typing,
/// clicking or scrolling — a restart throws away what's typed in a dialog, and
/// the last second or so of typing the draft store hasn't saved yet.
const INPUT_QUIET: Duration = Duration::from_secs(30);
/// How long a launch check's "Update available" keeps its accent highlight.
const HIGHLIGHT_FOR: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    /// Not checked yet, or the last check failed.
    Unchecked,
    Checking,
    UpToDate,
    Available {
        commits: Option<usize>,
    },
    Building,
    /// Built; it applies on the next launch (or the idle restart).
    Built,
    Restarting,
}

/// What the user has open that holds the idle restart back.
#[derive(Clone, Copy, Debug, PartialEq)]
enum UserHold {
    Dialog,
    /// Typed, clicked or scrolled within [`INPUT_QUIET`].
    Input,
}

/// The control's text for a phase (the toggle and spinner are drawn apart).
/// `waiting` is the agent work in flight; `hold`, what the user has going.
fn label(phase: Phase, restart_on: bool, waiting: usize, hold: Option<UserHold>) -> String {
    match phase {
        Phase::Unchecked => "Check for updates".into(),
        Phase::Checking => "Checking…".into(),
        Phase::UpToDate => "Up to date ✓".into(),
        Phase::Available { commits: None } => "Update available".into(),
        Phase::Available { commits: Some(n) } => {
            let s = if n == 1 { "" } else { "s" };
            format!("Update available · {n} commit{s}")
        }
        Phase::Building => "Building update…".into(),
        Phase::Built if !restart_on => "Update installed — applies on next launch".into(),
        Phase::Built if waiting == 0 => match hold {
            None | Some(UserHold::Input) => "Restarting when idle…".into(),
            Some(UserHold::Dialog) => "Restarting when idle · waiting on an open dialog".into(),
        },
        Phase::Built => {
            let s = if waiting == 1 { "" } else { "s" };
            format!("Restarting when idle · waiting on {waiting} item{s}")
        }
        Phase::Restarting => "Restarting…".into(),
    }
}

/// When the last check ran: its time, or why it failed.
#[derive(Clone, PartialEq)]
enum LastCheck {
    Never,
    At(i64),
    Failed(String),
}

#[component]
pub fn UpdateControl() -> Element {
    let state = use_context::<AppState>();
    let mode = use_hook(update::mode);
    let blocker = update::install_blocker(&mode);
    let mut phase = use_signal(|| Phase::Unchecked);
    // The "Update when idle" toggle.
    let mut restart_on = use_signal(|| false);
    let mut last = use_signal(|| LastCheck::Never);
    // A click before the launch check fires makes the launch check moot.
    let mut clicked = use_signal(|| false);
    let mut highlight = use_signal(|| false);
    // Set by the idle loop while only the user holds the restart back.
    let mut hold = use_signal(|| None::<UserHold>);
    let window = dioxus::desktop::window();

    let check_mode = mode.clone();
    let run_check = use_callback(move |manual: bool| {
        let mode = check_mode.clone();
        spawn(async move {
            phase.set(Phase::Checking);
            match update::check(&mode).await {
                Ok(outcome) => {
                    last.set(LastCheck::At(now_millis()));
                    match outcome {
                        CheckOutcome::UpToDate => phase.set(Phase::UpToDate),
                        CheckOutcome::Available { commits } => {
                            phase.set(Phase::Available { commits });
                            if !manual {
                                highlight.set(true);
                                tokio::time::sleep(HIGHLIGHT_FOR).await;
                                highlight.set(false);
                            }
                        }
                    }
                }
                Err(e) => {
                    phase.set(Phase::Unchecked);
                    if manual {
                        push_toast(Severity::Error, format!("Update check failed: {e}"));
                    }
                    last.set(LastCheck::Failed(e));
                }
            }
        });
    });

    // The one automatic check per launch — silent: a failure only shows in the
    // tooltip.
    use_future(move || async move {
        tokio::time::sleep(LAUNCH_CHECK_DELAY).await;
        if update::launch_check_enabled() && !*clicked.peek() && *phase.peek() == Phase::Unchecked {
            run_check(false);
        }
    });

    let install_mode = mode.clone();
    let start_build = use_callback(move |_: ()| {
        let mode = install_mode.clone();
        let commits = match *phase.peek() {
            Phase::Available { commits } => commits,
            _ => return,
        };
        spawn(async move {
            phase.set(Phase::Building);
            match update::install(&mode).await {
                Ok(()) => phase.set(Phase::Built),
                Err(e) => {
                    push_toast(Severity::Error, format!("Update failed:\n{e}"));
                    phase.set(Phase::Available { commits });
                    restart_on.set(false);
                }
            }
        });
    });

    // The idle restart. Polled once a second with peeks rather than driven by
    // an effect: idle has to *hold* for `IDLE_GRACE`, which an effect firing
    // on each change can't see.
    use_future(move || {
        let window = window.clone();
        async move {
            let mut idle_for = 0u32;
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let armed = *phase.peek() == Phase::Built && *restart_on.peek();
                if !armed || !state.active_work().is_empty() {
                    idle_for = 0;
                    continue;
                }
                let user = user_hold().await;
                if *hold.peek() != user {
                    hold.set(user);
                }
                if user.is_some() {
                    idle_for = 0;
                    continue;
                }
                idle_for += 1;
                if idle_for < IDLE_GRACE {
                    continue;
                }
                idle_for = 0;
                phase.set(Phase::Restarting);
                if demo_mode() {
                    push_toast(
                        Severity::Info,
                        "Demo mode: Usine would restart now to finish the update.",
                    );
                    phase.set(Phase::Built);
                    restart_on.set(false);
                    continue;
                }
                push_toast(Severity::Info, "Restarting to finish the update…");
                // Long enough for the toast to be seen.
                tokio::time::sleep(Duration::from_secs(1)).await;
                match update::relaunch_after_exit() {
                    Ok(()) => crate::quit_app(&state.executor_handle(), &window),
                    Err(e) => {
                        push_toast(Severity::Error, format!("Couldn't restart Usine: {e}"));
                        phase.set(Phase::Built);
                        restart_on.set(false);
                    }
                }
            }
        }
    });

    let current = phase();
    let on = restart_on();
    // Only subscribe to every card/review/session while the count is on screen.
    let waiting = if current == Phase::Built && on {
        state.active_work_count()
    } else {
        0
    };
    let text = label(current, on, waiting, hold());
    let busy = matches!(
        current,
        Phase::Checking | Phase::Building | Phase::Restarting
    );
    let clickable = matches!(current, Phase::Unchecked | Phase::UpToDate);
    let show_toggle = matches!(
        current,
        Phase::Available { .. } | Phase::Building | Phase::Built
    );
    let tip = tooltip(&last(), blocker);
    let class = if highlight() {
        "update-control has-tip is-new"
    } else {
        "update-control has-tip"
    };

    rsx! {
        div { class,
            if clickable {
                button {
                    class: "update-check",
                    onclick: move |_| {
                        clicked.set(true);
                        run_check(true);
                    },
                    "{text}"
                }
            } else {
                span { class: "update-status",
                    if busy {
                        span { class: "spinner" }
                    }
                    "{text}"
                }
            }
            if show_toggle {
                label { class: if blocker.is_some() { "update-toggle disabled" } else { "update-toggle" },
                    input {
                        r#type: "checkbox",
                        checked: on,
                        disabled: blocker.is_some(),
                        onchange: move |_| {
                            let v = !restart_on();
                            restart_on.set(v);
                            if v {
                                start_build(());
                            }
                        },
                    }
                    span { "Update when idle" }
                }
            }
            span { class: "info-tip up", "{tip}" }
        }
    }
}

/// What, if anything, the user has going that a restart would lose: an open
/// dialog (the New card / New session forms and confirm notes keep their text
/// in plain signals, outside the persisted draft store), or input too recent to
/// call them done. Unknown (the eval failed) counts as busy.
async fn user_hold() -> Option<UserHold> {
    // Installs the input listener on first use (re-installed after a webview
    // reload); until then the clock starts at install, so INPUT_QUIET still
    // has to pass in full.
    let js = "if (window.__usineLastInput === undefined) {\
                window.__usineLastInput = Date.now();\
                var bump = function () { window.__usineLastInput = Date.now(); };\
                ['keydown', 'pointerdown', 'wheel', 'input'].forEach(function (t) {\
                  document.addEventListener(t, bump, true);\
                });\
              }\
              dioxus.send([!!document.querySelector('.modal-overlay'),\
                           Date.now() - window.__usineLastInput]);";
    let Ok((dialog, quiet_ms)) = dioxus::document::eval(js).recv::<(bool, f64)>().await else {
        return Some(UserHold::Input);
    };
    if dialog {
        Some(UserHold::Dialog)
    } else if quiet_ms < INPUT_QUIET.as_millis() as f64 {
        Some(UserHold::Input)
    } else {
        None
    }
}

/// The control's own tooltip: the running commit, the last check, and why the
/// toggle is off-limits when it is.
fn tooltip(last: &LastCheck, blocker: Option<&str>) -> String {
    let sha = update::build_sha_short();
    let mut lines = vec![if sha.is_empty() {
        "Running an unknown commit".to_string()
    } else {
        format!("Running commit {sha}")
    }];
    match last {
        LastCheck::Never => {}
        LastCheck::At(at) => {
            let time = chrono::DateTime::from_timestamp_millis(*at)
                .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
                .unwrap_or_default();
            lines.push(format!("Last checked at {time}"));
        }
        LastCheck::Failed(e) => lines.push(format!("Last check failed: {e}")),
    }
    if let Some(why) = blocker {
        lines.push(why.to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_phase_has_its_label() {
        assert_eq!(label(Phase::Unchecked, false, 0, None), "Check for updates");
        assert_eq!(label(Phase::Checking, false, 0, None), "Checking…");
        assert_eq!(label(Phase::UpToDate, false, 0, None), "Up to date ✓");
        assert_eq!(
            label(Phase::Available { commits: None }, false, 0, None),
            "Update available"
        );
        assert_eq!(
            label(Phase::Available { commits: Some(1) }, false, 0, None),
            "Update available · 1 commit"
        );
        assert_eq!(
            label(Phase::Available { commits: Some(4) }, true, 0, None),
            "Update available · 4 commits"
        );
        assert_eq!(label(Phase::Building, true, 0, None), "Building update…");
        assert_eq!(label(Phase::Restarting, true, 0, None), "Restarting…");
    }

    #[test]
    fn a_built_update_says_what_it_waits_for() {
        assert_eq!(
            label(Phase::Built, false, 3, None),
            "Update installed — applies on next launch"
        );
        assert_eq!(label(Phase::Built, true, 0, None), "Restarting when idle…");
        assert_eq!(
            label(Phase::Built, true, 1, None),
            "Restarting when idle · waiting on 1 item"
        );
        assert_eq!(
            label(Phase::Built, true, 2, None),
            "Restarting when idle · waiting on 2 items"
        );
    }

    #[test]
    fn a_built_update_says_when_the_user_holds_it() {
        assert_eq!(
            label(Phase::Built, true, 0, Some(UserHold::Dialog)),
            "Restarting when idle · waiting on an open dialog"
        );
        assert_eq!(
            label(Phase::Built, true, 0, Some(UserHold::Input)),
            "Restarting when idle…"
        );
        // Agent work outranks the user's hold.
        assert_eq!(
            label(Phase::Built, true, 2, Some(UserHold::Dialog)),
            "Restarting when idle · waiting on 2 items"
        );
    }

    #[test]
    fn the_tooltip_carries_a_failed_check_and_the_blocker() {
        let tip = tooltip(&LastCheck::Failed("offline".into()), Some("nope"));
        assert!(tip.contains("Last check failed: offline"), "{tip}");
        assert!(tip.ends_with("nope"), "{tip}");
        assert!(!tooltip(&LastCheck::Never, None).contains("Last"));
    }
}
