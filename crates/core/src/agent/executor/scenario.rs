//! A card's scenario script: the optional `.usine-scenario.sh` an agent writes
//! to add the test data the repo's seed lacks (see
//! [`crate::agent::testing::SCENARIO_FILE`]).
//!
//! The preview environment is rebuilt from scratch on every start (the
//! conventional setup script runs `docker compose down -v`, migrates and
//! re-seeds), so data an agent inserts by hand is gone by the time the
//! reviewer looks. The scenario script is the repeatable alternative:
//! `launch_preview` replays it right after the setup script, before the app
//! starts, on every fresh start.
//!
//! It has two homes, kept in step here:
//! - the git-excluded file in the worktree root, which the agent edits and
//!   the preview runs;
//! - [`Card::scenario_script`] in the store, which survives a rebuilt
//!   worktree.
//!
//! The worktree file wins whenever it exists — it is what the agent or the
//! user last edited. A missing file is restored from the card, *except* at the
//! end of a write run ([`capture_scenario`]): that run started from a synced
//! worktree, so a file missing then means the agent deleted it. Only the
//! panel's "Remove" ([`Executor::clear_scenario`]) and `BackToStart` otherwise
//! drop it.

use std::path::Path;

use super::*;
use crate::agent::testing::SCENARIO_FILE;

/// Largest scenario script the card will store. A data-seeding script is a
/// few dozen lines; anything near this is most likely a dump the agent wrote
/// by mistake, which has no business in the card record.
pub(super) const MAX_SCENARIO_BYTES: usize = 64 * 1024;

/// What the worktree's scenario file says, if anything usable.
enum FileState {
    /// No file.
    Absent,
    /// A file with this content (`None` when it is blank).
    Present(Option<String>),
    /// A file that cannot be stored: unreadable, or over the size cap. The
    /// card's copy is left alone and the user is warned.
    Unusable(String),
}

fn read_file(worktree: &Path) -> FileState {
    let path = worktree.join(SCENARIO_FILE);
    match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileState::Absent,
        Err(e) => FileState::Unusable(format!("couldn't read {SCENARIO_FILE}: {e}")),
        Ok(bytes) if bytes.len() > MAX_SCENARIO_BYTES => FileState::Unusable(format!(
            "{SCENARIO_FILE} is {} KiB, over the {} KiB limit — it was not saved with the card",
            bytes.len() / 1024,
            MAX_SCENARIO_BYTES / 1024
        )),
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(s) if s.trim().is_empty() => FileState::Present(None),
            Ok(s) => FileState::Present(Some(s)),
            Err(_) => FileState::Unusable(format!(
                "{SCENARIO_FILE} is not UTF-8 text — it was not saved with the card"
            )),
        },
    }
}

/// Write the card's script back into the worktree (a rebuilt worktree, or a
/// script the user restored). Executable for convenience; the preview runs it
/// through `bash` either way.
fn write_file(worktree: &Path, script: &str) -> std::io::Result<()> {
    let path = worktree.join(SCENARIO_FILE);
    std::fs::write(&path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Store `script` on the card if it differs from what is there, echoing the
/// change to the UI.
fn store_script(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    script: Option<String>,
) {
    let Ok(card) = store.get_card(card_id) else {
        return;
    };
    if card.scenario_script == script {
        return;
    }
    if let Ok(updated) = store.mutate_card(card_id, |c| {
        c.scenario_script = script;
        Ok(())
    }) {
        let _ = evt_tx.unbounded_send(ExecutorEvent::updated(updated));
    }
}

fn warn(evt_tx: &UnboundedSender<ExecutorEvent>, card_id: Uuid, message: String) {
    let _ = evt_tx.unbounded_send(ExecutorEvent::toast(card_id, Severity::Warning, message));
}

/// Bring the worktree file and the card's copy in step before a write run or a
/// preview: a file in the worktree wins and is saved; a missing one is restored
/// from the card. Returns the script now in effect in the worktree.
pub(super) fn sync_scenario(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    worktree: &Path,
) -> Option<String> {
    match read_file(worktree) {
        FileState::Present(script) => {
            store_script(store, evt_tx, card_id, script.clone());
            script
        }
        FileState::Unusable(why) => {
            warn(evt_tx, card_id, why);
            // The file is still there and the preview will still run it.
            Some(String::new())
        }
        FileState::Absent => {
            let script = store.get_card(card_id).ok()?.scenario_script?;
            if let Err(e) = write_file(worktree, &script) {
                warn(
                    evt_tx,
                    card_id,
                    format!("couldn't restore the card's {SCENARIO_FILE}: {e}"),
                );
                return None;
            }
            Some(script)
        }
    }
}

/// Save what a write run left in the worktree. The run started from a synced
/// worktree, so the file is the source of truth: present → saved, absent →
/// the agent deleted it, so the card's copy goes too.
pub(super) fn capture_scenario(
    store: &Store,
    evt_tx: &UnboundedSender<ExecutorEvent>,
    card_id: Uuid,
    worktree: &Path,
) {
    match read_file(worktree) {
        FileState::Present(script) => store_script(store, evt_tx, card_id, script),
        FileState::Absent => store_script(store, evt_tx, card_id, None),
        FileState::Unusable(why) => warn(evt_tx, card_id, why),
    }
}

impl Executor {
    /// The panel's "Remove": drop the card's scenario script, both the stored
    /// copy and the worktree file, so the next fresh preview runs on the seed
    /// alone.
    pub(super) async fn clear_scenario(&self, card_id: Uuid) -> Result<()> {
        let updated = self.store.mutate_card(card_id, |c| {
            c.scenario_script = None;
            Ok(())
        })?;
        if let Some(wt) = updated.worktree_path.as_deref() {
            let _ = std::fs::remove_file(wt.join(SCENARIO_FILE));
        }
        let _ = self.evt_tx.unbounded_send(ExecutorEvent::updated(updated));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::{CardConfig, ProjectConfig};
    use crate::domain::model::Project;

    fn setup() -> (
        Store,
        UnboundedSender<ExecutorEvent>,
        UnboundedReceiver<ExecutorEvent>,
        Uuid,
        tempfile::TempDir,
    ) {
        let store = Store::open_in_memory().unwrap();
        let project = Project::new("p", PathBuf::from("/tmp/p"), ProjectConfig::default());
        store.upsert_project(&project).unwrap();
        let card = Card::new(project.id, "c", "d", CardConfig::default());
        store.upsert_card(&card).unwrap();
        let (tx, rx) = mpsc::unbounded();
        (store, tx, rx, card.id, tempfile::tempdir().unwrap())
    }

    fn stored(store: &Store, id: Uuid) -> Option<String> {
        store.get_card(id).unwrap().scenario_script
    }

    #[test]
    fn a_worktree_file_wins_and_is_saved() {
        let (store, tx, _rx, id, wt) = setup();
        store
            .mutate_card(id, |c| {
                c.scenario_script = Some("old".into());
                Ok(())
            })
            .unwrap();
        std::fs::write(wt.path().join(SCENARIO_FILE), "new").unwrap();
        assert_eq!(
            sync_scenario(&store, &tx, id, wt.path()).as_deref(),
            Some("new")
        );
        assert_eq!(stored(&store, id).as_deref(), Some("new"));
    }

    #[test]
    fn a_missing_file_is_restored_from_the_card() {
        let (store, tx, _rx, id, wt) = setup();
        assert_eq!(sync_scenario(&store, &tx, id, wt.path()), None);
        assert!(!wt.path().join(SCENARIO_FILE).exists());

        store
            .mutate_card(id, |c| {
                c.scenario_script = Some("echo hi\n".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            sync_scenario(&store, &tx, id, wt.path()).as_deref(),
            Some("echo hi\n")
        );
        assert_eq!(
            std::fs::read_to_string(wt.path().join(SCENARIO_FILE)).unwrap(),
            "echo hi\n"
        );
    }

    #[test]
    fn capture_clears_a_deleted_script_and_skips_an_oversized_one() {
        let (store, tx, _rx, id, wt) = setup();
        let file = wt.path().join(SCENARIO_FILE);
        std::fs::write(&file, "seed more").unwrap();
        capture_scenario(&store, &tx, id, wt.path());
        assert_eq!(stored(&store, id).as_deref(), Some("seed more"));

        std::fs::write(&file, "x".repeat(MAX_SCENARIO_BYTES + 1)).unwrap();
        capture_scenario(&store, &tx, id, wt.path());
        assert_eq!(
            stored(&store, id).as_deref(),
            Some("seed more"),
            "an oversized file leaves the stored copy alone"
        );

        std::fs::remove_file(&file).unwrap();
        capture_scenario(&store, &tx, id, wt.path());
        assert_eq!(stored(&store, id), None);
    }

    #[test]
    fn a_blank_file_stores_nothing() {
        let (store, tx, _rx, id, wt) = setup();
        std::fs::write(wt.path().join(SCENARIO_FILE), "  \n").unwrap();
        capture_scenario(&store, &tx, id, wt.path());
        assert_eq!(stored(&store, id), None);
    }
}
