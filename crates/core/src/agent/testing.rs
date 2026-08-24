//! In-app self-testing for write runs: the prompt-side half of the merged
//! implement-and-preview flow.
//!
//! When a write run (implement / apply-fixes) launches on a project whose
//! `auto_preview` is on, the executor also brings the card's preview up in the
//! same worktree (see `executor::preview::ensure_preview_for_run`): setup
//! script, then the project's `run_script` as an executor-owned process. The
//! agent is told about it via [`testing_instruction`] and discovers the app's
//! URLs from [`PREVIEW_INFO_FILE`], which the executor writes into the worktree
//! root once the app process is launched. With `auto_preview` off, nothing
//! starts eagerly; instead the agent gets [`testing_instruction_on_request`]
//! and can ask for the app mid-run by creating [`PREVIEW_REQUEST_FILE`] in the
//! worktree root — a watcher bound to the run's lifetime picks it up and
//! starts the same preview. Both files are git-excluded (shared
//! `info/exclude`, registered before the run starts), so neither the finalize
//! commit's `git add -A` nor a commit the agent makes itself can sweep them
//! onto the branch.
//!
//! The agent never owns the server: it must not start, stop, or restart it —
//! the executor's preview machinery is the only thing that can reap the process
//! tree (see the merge/teardown history in `executor::preview`). The server
//! stays up only while the automated pipeline runs: when the card parks, the
//! finalizers stop it (`reap_idle_preview`), tearing the worktree's isolated
//! infra down with it.

use crate::domain::model::PreviewUrl;

/// Name of the JSON file the executor writes into a worktree's root once its
/// preview app is up, listing the app's local URLs for the agent to test
/// against. Removed when the preview stops or its process exits.
pub const PREVIEW_INFO_FILE: &str = ".usine-preview.json";

/// Sentinel file an agent creates in its worktree root to request the app when
/// the project's `auto_preview` is off. The executor's request watcher deletes
/// it and starts the preview.
pub const PREVIEW_REQUEST_FILE: &str = ".usine-preview-request";

/// Appended to write runs when the project has a `run_script` and starts the
/// app eagerly (`auto_preview` on). Kept provider-agnostic and self-contained,
/// like its siblings in [`crate::agent::commit`] and [`crate::agent::handoff`].
/// `has_ports` picks the observation route: declared preview ports mean the
/// agent exercises URLs; none means a windowed app it should screenshot.
pub fn testing_instruction(run_script: &str, has_ports: bool) -> String {
    format!(
        "The project's app is being started for you in this worktree (its command: `{run_script}`), \
         owned by the tool that launched you. Do NOT start your own instance and do NOT stop or \
         restart it — it watch-reloads your edits. Once the app is up, `{PREVIEW_INFO_FILE}` in the \
         worktree root lists its local URLs. Setup (dependency install, an isolated database, port \
         allocation) can take a few minutes and the file only appears when the app process is \
         launched, so if it is missing, keep working and check again later.\n{}\n\
         Leave the app running when you finish — the tool that launched you stops it once the work \
         parks.",
        observation_route(has_ports)
    )
}

/// The on-request twin of [`testing_instruction`]: appended to write runs when
/// the project has a `run_script` but `auto_preview` is off. The app is not
/// started eagerly; the agent asks for it via [`PREVIEW_REQUEST_FILE`].
pub fn testing_instruction_on_request(run_script: &str, has_ports: bool) -> String {
    format!(
        "The project's app (its command: `{run_script}`) is NOT started automatically for this \
         run. If your change has behaviour observable in the running app, request the app NOW — \
         its setup (dependency install, an isolated database, port allocation) takes minutes — by \
         creating an empty `{PREVIEW_REQUEST_FILE}` file in the worktree root, then keep working \
         while it comes up and watch for `{PREVIEW_INFO_FILE}` in the worktree root, which lists \
         its local URLs once the app process is launched. The tool that launched you owns that \
         process: do NOT start your own instance and do NOT stop or restart it — it watch-reloads \
         your edits.\n{}\n\
         Leave the app running when you finish — the tool that launched you stops it once the work \
         parks.",
        observation_route(has_ports)
    )
}

/// How the agent should actually observe its change in the running app, shared
/// by both instruction variants. With declared preview ports the app is
/// URL-reachable; without them it's a windowed program, so the only observation
/// is a screenshot — of the app's window ONLY, never the full display: the
/// rest of the screen is the user's other windows (mail, chats, secrets),
/// which must not enter the agent's context. A failure to capture the window
/// (e.g. a missing screen-recording permission) must be reported, not guessed
/// around — and not worked around with a full-screen grab.
fn observation_route(has_ports: bool) -> &'static str {
    if has_ports {
        "After implementing, if the change has behaviour observable in the running app, verify it \
         there before you finish: exercise those URLs (curl, or the project's own scripts), check \
         the change works end to end, fix what you find, and re-verify. If the app never comes up, \
         or the change is not observable this way, skip the in-app check and say so in your \
         hand-off."
    } else {
        "The app has no declared URLs — it runs as a windowed program. After implementing, if the \
         change is visible in the app, verify it by capturing a screenshot of the app's window \
         ONLY and reading the image — on macOS `screencapture -l <window-id>` to a temporary png, \
         resolving the window id first (e.g. via `osascript` or `GetWindowID`). NEVER capture the \
         full display: the rest of the screen is the user's other windows and their contents, \
         which must not enter your context — and the app may be obscured or on another Space \
         anyway. Fix what you find and re-verify. If you can't capture the app's window \
         specifically (e.g. a missing screen-recording permission, or no way to resolve its \
         window id), don't fall back to a full-screen grab and don't guess — say exactly that in \
         your hand-off."
    }
}

/// The contents of [`PREVIEW_INFO_FILE`]: the running app's URLs. `urls` can be
/// empty (no ports declared, or no port offset yet) — the file's existence still
/// tells the agent the app process is up.
pub fn preview_info_json(urls: &[PreviewUrl]) -> String {
    let urls: Vec<serde_json::Value> = urls
        .iter()
        .map(|u| serde_json::json!({ "label": u.label, "url": u.url }))
        .collect();
    serde_json::json!({ "status": "running", "urls": urls }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_names_the_run_script_and_info_file() {
        let s = testing_instruction("pnpm dev", true);
        assert!(s.contains("`pnpm dev`"));
        assert!(s.contains(PREVIEW_INFO_FILE));
        // Eager mode: the app is started for the agent, no sentinel involved.
        assert!(!s.contains(PREVIEW_REQUEST_FILE));
        // Ports declared → the URL route, not the screenshot route.
        assert!(s.contains("curl"));
        assert!(!s.contains("screenshot"));
    }

    #[test]
    fn portless_instruction_takes_the_screenshot_route() {
        let s = testing_instruction("cargo run", false);
        assert!(s.contains("screenshot"));
        assert!(s.contains("screen-recording permission"));
        assert!(!s.contains("curl"));
        // Window-scoped capture only — a full-display grab would feed the
        // user's other windows into the agent's context.
        assert!(s.contains("-l <window-id>"));
        assert!(s.contains("NEVER capture the full display"));
    }

    #[test]
    fn on_request_instruction_names_the_sentinel() {
        let s = testing_instruction_on_request("pnpm dev", true);
        assert!(s.contains("`pnpm dev`"));
        assert!(s.contains(PREVIEW_REQUEST_FILE));
        assert!(s.contains(PREVIEW_INFO_FILE));
        assert!(s.contains("NOT started automatically"));
        // The ownership contract holds in both modes.
        assert!(s.contains("do NOT stop or restart it"));
        assert!(s.contains("curl"));
    }

    #[test]
    fn on_request_portless_takes_the_screenshot_route() {
        let s = testing_instruction_on_request("cargo run", false);
        assert!(s.contains(PREVIEW_REQUEST_FILE));
        assert!(s.contains("screenshot"));
        assert!(!s.contains("curl"));
    }

    #[test]
    fn info_json_round_trips() {
        let urls = vec![PreviewUrl {
            label: "web".into(),
            url: "http://localhost:3100".into(),
        }];
        let v: serde_json::Value = serde_json::from_str(&preview_info_json(&urls)).unwrap();
        assert_eq!(v["status"], "running");
        assert_eq!(v["urls"][0]["url"], "http://localhost:3100");

        let v: serde_json::Value = serde_json::from_str(&preview_info_json(&[])).unwrap();
        assert!(v["urls"].as_array().unwrap().is_empty());
    }
}
