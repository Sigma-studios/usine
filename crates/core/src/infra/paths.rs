//! Canonical on-disk locations, resolved in one place so the app, the executor,
//! and the CLI all agree on where Usine keeps its data. Centralizing the
//! `("dev", "usine", "usine")` identifier avoids the two crates drifting apart.

use std::path::PathBuf;

use uuid::Uuid;

/// The data directory for Usine. Defaults to the platform location (e.g.
/// `~/Library/Application Support/usine`), falling back to a namespaced temp
/// dir if it can't be resolved.
///
/// `USINE_DATA_DIR` overrides it entirely — database, worktrees, and
/// attachments all move with it. This is what lets a second Usine instance run
/// fully isolated from the main one (e.g. Usine previewing itself from a card's
/// worktree). An absolute path is recommended; a relative one resolves against
/// the process working directory. Unset or empty means the default. Composes
/// with `USINE_SIM` (the demo DB name applies under whichever dir is active).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("USINE_DATA_DIR") {
        if !dir.is_empty() {
            let p = PathBuf::from(dir);
            return if p.is_absolute() {
                p
            } else {
                std::env::current_dir().map(|c| c.join(&p)).unwrap_or(p)
            };
        }
    }
    directories::ProjectDirs::from("dev", "usine", "usine")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::env::temp_dir().join("usine"))
}

/// Default database file. `demo` selects the throwaway simulator store so a
/// Phase-A demo run never touches the real board.
pub fn store_path(demo: bool) -> PathBuf {
    let name = if demo { "usine-demo.db" } else { "usine.db" };
    data_dir().join(name)
}

/// Root for all card worktrees, kept *outside* any project repo so the user's
/// working tree stays clean (mirrors where the database lives).
pub fn worktrees_root() -> PathBuf {
    data_dir().join("worktrees")
}

/// Where a card's attached images are copied. Deliberately under the data dir —
/// *outside* any project repo — so attachments are never tracked by git.
pub fn attachments_dir(card_id: Uuid) -> PathBuf {
    data_dir().join("attachments").join(card_id.to_string())
}

/// The user-facing name of a stored attachment. Files land in
/// [`attachments_dir`] as `<8 hex>-<original>` (the prefix keeps names unique);
/// this strips the prefix back off. Falls back to the full file name for
/// anything that doesn't follow the convention.
pub fn attachment_label(path: &std::path::Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.split_once('-') {
        Some((_, original)) => original.to_string(),
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One combined test on purpose: env vars are process-global and the test
    /// harness runs tests on parallel threads, so a second test mutating
    /// `USINE_DATA_DIR` would race this one.
    #[test]
    fn data_dir_env_override_moves_everything() {
        let tmp = std::env::temp_dir().join("usine-paths-test");
        std::env::set_var("USINE_DATA_DIR", &tmp);

        assert_eq!(data_dir(), tmp);
        assert_eq!(store_path(false), tmp.join("usine.db"));
        assert_eq!(store_path(true), tmp.join("usine-demo.db"));
        assert_eq!(worktrees_root(), tmp.join("worktrees"));
        let id = Uuid::nil();
        assert!(attachments_dir(id).starts_with(&tmp));

        // Empty counts as unset.
        std::env::set_var("USINE_DATA_DIR", "");
        let default = data_dir();
        assert_ne!(default, tmp);

        std::env::remove_var("USINE_DATA_DIR");
        assert_eq!(data_dir(), default);
    }
}
