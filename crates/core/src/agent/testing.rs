//! In-app self-testing for write runs: the prompt-side half of the merged
//! implement-and-preview flow.
//!
//! When a write run (implement / apply-fixes) launches, the executor also brings
//! the card's preview up in the same worktree (see
//! `executor::preview::ensure_preview_for_run`): setup script, then the
//! project's `run_script` as an executor-owned process. The agent is told about
//! it via [`testing_instruction`] and discovers the app's URLs from
//! [`PREVIEW_INFO_FILE`], which the executor writes into the worktree root once
//! the app process is launched. The file is git-excluded (shared
//! `info/exclude`), so neither the finalize commit's `git add -A` nor a commit
//! the agent makes itself can sweep it onto the branch.
//!
//! The agent never owns the server: it must not start, stop, or restart it —
//! the executor's preview machinery is the only thing that can reap the process
//! tree (see the merge/teardown history in `executor::preview`). The server is
//! left running when the run ends, so the human reviewer arrives at a warm,
//! already-serving build.

use crate::domain::model::PreviewUrl;

/// Name of the JSON file the executor writes into a worktree's root once its
/// preview app is up, listing the app's local URLs for the agent to test
/// against. Removed when the preview stops or its process exits.
pub const PREVIEW_INFO_FILE: &str = ".usine-preview.json";

/// Appended to write runs when the project has a `run_script`. Kept
/// provider-agnostic and self-contained, like its siblings in
/// [`crate::agent::commit`] and [`crate::agent::handoff`].
pub fn testing_instruction(run_script: &str) -> String {
    format!(
        "The project's app is being started for you in this worktree (its command: `{run_script}`), \
         owned by the tool that launched you. Do NOT start your own instance and do NOT stop or \
         restart it — it watch-reloads your edits. Once the app is up, `{PREVIEW_INFO_FILE}` in the \
         worktree root lists its local URLs. Setup (dependency install, an isolated database, port \
         allocation) can take a few minutes and the file only appears when the app process is \
         launched, so if it is missing, keep working and check again later.\n\
         After implementing, if the change has behaviour observable in the running app, verify it \
         there before you finish: exercise those URLs (curl, or the project's own scripts), check \
         the change works end to end, fix what you find, and re-verify. If the app never comes up, \
         or the change is not observable this way, skip the in-app check and say so in your \
         hand-off. Leave the app running when you finish — the human who reviews your work uses it \
         next."
    )
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
        let s = testing_instruction("pnpm dev");
        assert!(s.contains("`pnpm dev`"));
        assert!(s.contains(PREVIEW_INFO_FILE));
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
