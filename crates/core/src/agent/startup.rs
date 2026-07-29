//! One-time startup reconciliation the app runs before showing the board. Kept
//! in core (not the UI layer) so it's unit-testable and the view stays thin.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::domain::config::AppSettings;
use crate::domain::model::{now_millis, Provider, ReviewStatus};
use crate::domain::state_machine::{transition, Transition};
use crate::error::Result;
use crate::infra::git::detect_base_branch;
use crate::infra::paths::worktrees_root;
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

/// A Docker Compose stack as Docker itself reports it: the project name (what
/// `docker compose -p` addresses) and the directory it was launched from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ComposeStack {
    project: String,
    working_dir: PathBuf,
}

/// Tear down Compose stacks stranded by worktrees that no longer exist.
///
/// A project's `setup-worktree.sh` typically brings up isolated infra (a
/// per-worktree database container + volume) and its `teardown-worktree.sh`
/// takes it down. Both scripts live *inside the worktree*, so once the
/// directory is gone the stack can no longer be torn down by the normal path —
/// nothing left on disk knows its Compose project name. Every path that removes
/// a worktree stops the preview (and runs teardown) first, but a crash, a
/// force-quit, a worktree removed by hand, or a branch whose base predates the
/// teardown script all leak a container, a volume and a host port permanently.
///
/// Docker itself holds the missing knowledge: Compose labels every container
/// with its project name and originating directory, and `docker compose -p NAME
/// down -v` works off those labels with no compose file present. So the stacks
/// are recoverable from Docker even when the worktree is not.
///
/// Returns how many stacks were torn down. Best-effort throughout: no Docker,
/// no daemon, or an unparseable listing all mean "nothing to do".
///
/// **Blocks** — each teardown shells out to Docker for a second or more. Call it
/// off the startup path (see the caller) so a slow or wedged daemon delays no
/// one.
pub fn reconcile_orphaned_worktree_stacks() -> usize {
    let root = worktrees_root();
    // A missing root is the dangerous case, not the trivial one: if the data
    // directory is merely unreachable (external disk not mounted yet,
    // `USINE_DATA_DIR` pointing somewhere not yet created) then *every* live
    // worktree looks deleted and the sweep would tear down every running stack.
    // Absent proof the root is there, do nothing.
    if !root.is_dir() {
        return 0;
    }
    let orphans = orphaned_stacks(discover_compose_stacks(), &root, |p| p.exists());
    orphans.iter().filter(|p| compose_down(p)).count()
}

/// Select the stacks that are Usine's *and* stranded, given every stack Docker
/// knows about. Split out from the IO so the selection rule — the part that
/// decides what gets destroyed — is unit-testable.
///
/// Ownership is decided by location, never by name: the conventional
/// `wt-<something>` project name is derived from a worktree's directory name, so
/// unrelated checkouts a user drives by hand carry it too. Only a working
/// directory *under Usine's worktrees root* proves the stack is ours.
fn orphaned_stacks(
    stacks: impl IntoIterator<Item = ComposeStack>,
    root: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Vec<String> {
    stacks
        .into_iter()
        .filter(|s| s.working_dir.starts_with(root) && !exists(&s.working_dir))
        .map(|s| s.project)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Every Compose stack Docker knows about, from its container labels. Includes
/// stopped containers: a stack whose container exited still owns its volume and
/// is just as stranded. Empty when Docker is absent or unreachable.
fn discover_compose_stacks() -> Vec<ComposeStack> {
    let out = Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            "label=com.docker.compose.project",
            "--format",
            // Tab-separated: a working directory can (and by default does)
            // contain spaces, so whitespace splitting would corrupt the path.
            "{{.Label \"com.docker.compose.project\"}}\t{{.Label \"com.docker.compose.project.working_dir\"}}",
        ])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_stack_line)
        .collect()
}

/// Parse one `project\tworking_dir` listing row. `None` for a malformed row or
/// one missing either label — a stack with no project name can't be addressed,
/// and one with no working directory can't be proven ours.
fn parse_stack_line(line: &str) -> Option<ComposeStack> {
    let (project, dir) = line.trim_end().split_once('\t')?;
    if project.is_empty() || dir.is_empty() {
        return None;
    }
    Some(ComposeStack {
        project: project.to_string(),
        working_dir: PathBuf::from(dir),
    })
}

/// `docker compose -p NAME down -v --remove-orphans` — containers, network and
/// named volumes, addressed purely by project label so it works with the
/// compose file long deleted. Returns whether it succeeded.
fn compose_down(project: &str) -> bool {
    Command::new("docker")
        .args(["compose", "-p", project, "down", "-v", "--remove-orphans"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
        for installed in [(|_: Provider| true) as fn(Provider) -> bool, |p| {
            p == Provider::Claude
        }] {
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

    /// Build a stack rooted under `root` (the shape Usine produces:
    /// `<root>/<project-slug>/<card-uuid>`).
    fn usine_stack(root: &Path, name: &str) -> ComposeStack {
        ComposeStack {
            project: format!("wt-{name}"),
            working_dir: root.join("proj-abc1234").join(name),
        }
    }

    #[test]
    fn sweep_selects_only_stacks_whose_worktree_is_gone() {
        let root = PathBuf::from("/data/worktrees");
        let live = usine_stack(&root, "aaa");
        let gone = usine_stack(&root, "bbb");
        let live_dir = live.working_dir.clone();

        let orphans = orphaned_stacks(vec![live, gone], &root, |p| p == live_dir);
        assert_eq!(orphans, vec!["wt-bbb".to_string()]);
    }

    /// The guard that matters: the `wt-` project name is derived from a
    /// directory name, so a checkout the user drives by hand carries it too.
    /// Ownership must key off the root path, never the name — otherwise the
    /// sweep destroys databases it does not own.
    #[test]
    fn sweep_never_touches_stacks_outside_the_worktrees_root() {
        let root = PathBuf::from("/data/worktrees");
        let foreign = ComposeStack {
            project: "wt-slot-2".into(),
            working_dir: PathBuf::from("/home/u/atelier/slots/slot-2"),
        };
        let unrelated = ComposeStack {
            project: "some-app".into(),
            working_dir: PathBuf::from("/home/u/some-app"),
        };
        // Nothing outside the root exists, yet neither is swept.
        let orphans = orphaned_stacks(vec![foreign, unrelated], &root, |_| false);
        assert!(orphans.is_empty(), "swept a stack it does not own");
    }

    /// A sibling directory sharing the root's name prefix is not under it.
    #[test]
    fn sweep_matches_path_components_not_string_prefixes() {
        let root = PathBuf::from("/data/worktrees");
        let sibling = ComposeStack {
            project: "wt-x".into(),
            working_dir: PathBuf::from("/data/worktrees-backup/proj/x"),
        };
        assert!(orphaned_stacks(vec![sibling], &root, |_| false).is_empty());
    }

    #[test]
    fn sweep_deduplicates_multi_container_stacks() {
        let root = PathBuf::from("/data/worktrees");
        // One stranded stack, reported once per container (db + cache).
        let stack = usine_stack(&root, "ccc");
        let orphans = orphaned_stacks(vec![stack.clone(), stack], &root, |_| false);
        assert_eq!(orphans, vec!["wt-ccc".to_string()]);
    }

    #[test]
    fn stack_lines_parse_paths_containing_spaces() {
        // The real default data dir has a space in it.
        let line = "wt-abc\t/Users/u/Library/Application Support/dev.usine.usine/worktrees/p/abc";
        let stack = parse_stack_line(line).expect("should parse");
        assert_eq!(stack.project, "wt-abc");
        assert!(stack.working_dir.ends_with("worktrees/p/abc"));
        assert_eq!(
            stack.working_dir.parent().unwrap().parent().unwrap(),
            PathBuf::from("/Users/u/Library/Application Support/dev.usine.usine/worktrees")
        );
    }

    #[test]
    fn stack_lines_without_both_labels_are_skipped() {
        // A container with no working_dir label can't be proven ours, and one
        // with no project name can't be addressed by `docker compose -p`.
        assert!(parse_stack_line("wt-abc\t").is_none());
        assert!(parse_stack_line("\t/data/worktrees/p/abc").is_none());
        assert!(parse_stack_line("no-tab-at-all").is_none());
        assert!(parse_stack_line("").is_none());
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
