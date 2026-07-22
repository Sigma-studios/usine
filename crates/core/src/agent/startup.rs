//! One-time startup reconciliation the app runs before showing the board. Kept
//! in core (not the UI layer) so it's unit-testable and the view stays thin.

use crate::domain::config::AppSettings;
use crate::domain::model::{now_millis, Provider, ReviewStatus};
use crate::domain::state_machine::{transition, Transition};
use crate::error::Result;
use crate::infra::git::detect_base_branch;
use crate::infra::persistence::Store;

/// On the very first startup (no settings record yet), pick the default
/// provider from which agent CLIs are installed: Codex when it is the sole one
/// present, Claude otherwise. Persists the seeded settings — so the choice is
/// made once and never overrides a later user decision — only when at least one
/// CLI was actually found: with neither installed (fresh machine, or a packaged
/// launch under a minimal PATH) it falls back to Claude without persisting, so
/// detection re-runs on the next startup once a CLI exists. Availability is
/// injected (`installed`) so tests don't depend on the host's PATH. Returns the
/// seeded provider; `None` when settings already exist.
pub fn seed_default_provider(
    store: &Store,
    installed: impl Fn(Provider) -> bool,
) -> Result<Option<Provider>> {
    if store.has_settings()? {
        return Ok(None);
    }
    let (claude, codex) = (installed(Provider::Claude), installed(Provider::Codex));
    if !claude && !codex {
        return Ok(Some(Provider::Claude));
    }
    let provider = if codex && !claude {
        Provider::Codex
    } else {
        Provider::Claude
    };
    store.save_settings(&AppSettings::default_for(provider))?;
    Ok(Some(provider))
}

/// Whether `name` resolves to something `Command::new(name)` can spawn when a
/// run launches the agent CLI. On Windows that means `name.exe` only: since the
/// CVE-2024-24576 hardening, `Command` refuses to run `.cmd`/`.bat` shims (what
/// npm installs for codex) under the bare name, so counting them here would
/// report a CLI as available whose runs then fail at spawn.
pub fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        if dir.as_os_str().is_empty() {
            return false;
        }
        if cfg!(windows) {
            is_executable_file(&dir.join(name).with_extension("exe"))
        } else {
            is_executable_file(&dir.join(name))
        }
    })
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // `metadata` follows symlinks (the claude installer symlinks
    // `~/.local/bin/claude`), unlike `symlink_metadata`.
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// After a restart, a card persisted in a "running" sub-state has no process
/// behind it. Mark each as interrupted (a `Failed` state carrying `message`, off
/// which the UI keys its "Resume" affordance) so the user can resume explicitly.
/// Returns how many cards were reconciled.
pub fn reconcile_interrupted_runs(store: &Store, message: &str) -> Result<usize> {
    let mut reconciled = 0;
    for mut card in store.list_cards()? {
        if card.state.is_running() {
            if let Ok(next) = transition(
                &card.state,
                Transition::AgentError {
                    message: message.to_string(),
                },
            ) {
                card.state = next;
                card.updated_at = now_millis();
                store.upsert_card(&card)?;
                reconciled += 1;
            }
        }
    }
    Ok(reconciled)
}

/// After a restart, a PR-review task persisted in the running `Reviewing` state
/// has no agent behind it. Mark each `Failed` (retryable — the UI keys "Retry"
/// off it) carrying `message`. Returns how many tasks were reconciled.
pub fn reconcile_interrupted_reviews(store: &Store, message: &str) -> Result<usize> {
    let mut reconciled = 0;
    for mut task in store.list_review_tasks()? {
        if task.status.is_running() {
            let prev = std::mem::replace(&mut task.status, ReviewStatus::ToReview);
            task.status = ReviewStatus::Failed {
                previous: Box::new(prev),
                message: message.to_string(),
            };
            task.updated_at = now_millis();
            store.upsert_review_task(&task)?;
            reconciled += 1;
        }
    }
    Ok(reconciled)
}

/// Keep each project's *detected* base branch in sync with what its repo
/// actually has. This only refreshes `base_branch`; a user pin
/// (`pinned_base_branch`) lives beside it, is never touched here, and wins at
/// read time via `effective_base_branch()`. Returns how many projects were
/// updated.
pub fn sync_base_branches(store: &Store) -> Result<usize> {
    let mut updated = 0;
    for mut project in store.list_projects()? {
        let detected = detect_base_branch(&project.path);
        if detected != project.config.base_branch {
            project.config.base_branch = detected;
            store.upsert_project(&project)?;
            updated += 1;
        }
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::CardConfig;
    use crate::domain::model::{Card, CardState, Project, RunSub};
    use std::path::PathBuf;

    #[test]
    fn first_run_seeds_codex_when_it_is_the_only_cli() {
        let store = Store::open_in_memory().unwrap();
        let codex_only = |p: Provider| p == Provider::Codex;

        let seeded = seed_default_provider(&store, codex_only).unwrap();
        assert_eq!(seeded, Some(Provider::Codex));

        // The persisted settings carry Codex model presets, not Claude's.
        let settings = store.settings().unwrap();
        assert_eq!(settings.default_provider, Provider::Codex);
        assert_eq!(settings, AppSettings::default_for(Provider::Codex));

        // The check is one-shot: a second startup does nothing.
        assert_eq!(seed_default_provider(&store, codex_only).unwrap(), None);
    }

    #[test]
    fn first_run_defaults_to_claude_otherwise() {
        // Both installed and Claude-only: both seed and persist Claude.
        for installed in [
            (|_: Provider| true) as fn(Provider) -> bool,
            |p| p == Provider::Claude,
        ] {
            let store = Store::open_in_memory().unwrap();
            assert_eq!(
                seed_default_provider(&store, installed).unwrap(),
                Some(Provider::Claude)
            );
            assert_eq!(store.settings().unwrap().default_provider, Provider::Claude);
        }
    }

    #[test]
    fn no_cli_found_defaults_to_claude_without_disarming_detection() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(
            seed_default_provider(&store, |_| false).unwrap(),
            Some(Provider::Claude)
        );
        // Nothing was persisted, so detection stays armed…
        assert!(!store.has_settings().unwrap());
        // …and installing codex before the next launch still seeds Codex.
        assert_eq!(
            seed_default_provider(&store, |p| p == Provider::Codex).unwrap(),
            Some(Provider::Codex)
        );
        assert_eq!(store.settings().unwrap().default_provider, Provider::Codex);
    }

    #[test]
    fn existing_settings_are_never_overridden() {
        let store = Store::open_in_memory().unwrap();
        // The user already chose Claude; installing only codex later must not
        // flip their settings.
        store.save_settings(&AppSettings::default()).unwrap();

        let seeded = seed_default_provider(&store, |p| p == Provider::Codex).unwrap();
        assert_eq!(seeded, None);
        assert_eq!(store.settings().unwrap().default_provider, Provider::Claude);
    }

    #[test]
    fn reconcile_marks_running_cards_failed() {
        let store = Store::open_in_memory().unwrap();
        let project = Project::new("p", PathBuf::from("/tmp/p"), Default::default());
        store.upsert_project(&project).unwrap();

        let mut running = Card::new(project.id, "r", "d", CardConfig::default());
        running.state = CardState::Implementing(RunSub::Running);
        store.upsert_card(&running).unwrap();

        let idle = Card::new(project.id, "i", "d", CardConfig::default()); // StartingBlock
        store.upsert_card(&idle).unwrap();

        assert_eq!(
            reconcile_interrupted_runs(&store, "interrupted").unwrap(),
            1
        );
        assert!(store.get_card(running.id).unwrap().state.is_failed());
        // The non-running card is untouched.
        assert!(matches!(
            store.get_card(idle.id).unwrap().state,
            CardState::StartingBlock
        ));
    }

    #[test]
    fn reconcile_marks_reviewing_tasks_failed() {
        use crate::domain::model::ReviewTask;

        let store = Store::open_in_memory().unwrap();
        let project = Project::new("p", PathBuf::from("/tmp/p"), Default::default());
        store.upsert_project(&project).unwrap();

        let mut reviewing = ReviewTask::new(project.id, 1, "t", "octocat", "url", "feat", "main");
        reviewing.status = ReviewStatus::Reviewing;
        store.upsert_review_task(&reviewing).unwrap();

        // A ToReview task is not running and must be left alone.
        let idle = ReviewTask::new(project.id, 2, "t2", "hubot", "url", "fix", "main");
        store.upsert_review_task(&idle).unwrap();

        assert_eq!(
            reconcile_interrupted_reviews(&store, "interrupted").unwrap(),
            1
        );
        assert!(store
            .get_review_task(reviewing.id)
            .unwrap()
            .status
            .is_failed());
        assert!(matches!(
            store.get_review_task(idle.id).unwrap().status,
            ReviewStatus::ToReview
        ));
    }

    #[test]
    fn base_branch_sync_refreshes_detected_but_keeps_pin() {
        let store = Store::open_in_memory().unwrap();
        // A path with no git repo: detection falls back to "dev".
        let mut project = Project::new(
            "p",
            PathBuf::from("/tmp/does-not-exist"),
            Default::default(),
        );
        project.config.base_branch = "stale".to_string();
        project.config.pinned_base_branch = Some("release/v2".to_string());
        store.upsert_project(&project).unwrap();

        assert_eq!(sync_base_branches(&store).unwrap(), 1);
        let synced = store.get_project(project.id).unwrap();
        // The detected value was refreshed…
        assert_eq!(synced.config.base_branch, "dev");
        // …but the pin survives and still wins.
        assert_eq!(
            synced.config.pinned_base_branch.as_deref(),
            Some("release/v2")
        );
        assert_eq!(synced.config.effective_base_branch(), "release/v2");
    }
}
