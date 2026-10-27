//! One-off sessions: ad-hoc Claude/Codex chats in any folder, outside the
//! card board. `SessionList` takes the board area, `SessionDetail` the detail
//! panel, and `NewSessionHost` hosts the new-session dialog at the app root.

use std::path::{Path, PathBuf};

use usine_core::{Session, SessionStatus};
use uuid::Uuid;

use crate::ui::confirm::{request_confirm, ConfirmAction, ConfirmRequest};

mod detail;
mod list;
mod new;
mod work;

pub(crate) use detail::SessionDetail;
pub(crate) use list::SessionList;
pub(crate) use new::{open_new_session, session_start_rejected, NewSessionHost};

/// The user's home directory, if known.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `~` and `~/…` expanded against `home`; anything else as typed (trimmed).
pub(crate) fn expand_tilde(input: &str, home: Option<&Path>) -> PathBuf {
    let input = input.trim();
    match (input, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (s, Some(h)) if s.starts_with("~/") => h.join(&s[2..]),
        (s, _) => PathBuf::from(s),
    }
}

/// `path` with the home prefix shown as `~`.
pub(crate) fn shorten_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(h) = home {
        if path == h {
            return "~".into();
        }
        if let Ok(rest) = path.strip_prefix(h) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// A short relative time ("just now", "5 min ago", "3 h ago", "2 d ago").
pub(crate) fn ago(ts: i64, now: i64) -> String {
    let secs = (now - ts).max(0) / 1000;
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => format!("{} h ago", secs / 3600),
        _ => format!("{} d ago", secs / 86_400),
    }
}

/// The list/sidebar dot for one session, in the sidebar's four-state scheme.
pub(crate) fn session_dot(s: &Session) -> &'static str {
    match &s.status {
        SessionStatus::Running => "proj-dot working",
        _ if s.archived || s.blocked => "proj-dot idle",
        SessionStatus::NeedsAnswer(_) => "proj-dot urgent",
        SessionStatus::Failed { .. } if s.unread => "proj-dot urgent",
        _ if s.unread => "proj-dot",
        _ => "proj-dot idle",
    }
}

/// Ask for the optional message before marking session `id` blocked — the
/// same dialog a board card's "Mark blocked" opens. Unblocking doesn't ask.
pub(crate) fn request_block_session(id: Uuid, title: &str) {
    request_confirm(ConfirmRequest {
        title: "Mark blocked".into(),
        message: format!(
            "Mark “{title}” as blocked? It stops counting toward the attention badges until you unmark it.",
        ),
        confirm_label: "Mark blocked".into(),
        danger: false,
        action: ConfirmAction::BlockSession(id),
    });
}

/// Show `path` in Finder (or the platform's file manager).
pub(crate) fn reveal(path: &Path) {
    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(not(target_os = "macos"))]
    let child = std::process::Command::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn();
    if let Err(e) = child {
        tracing::warn!("could not reveal {}: {e}", path.display());
    }
}

/// Open an `http(s)` URL in the default browser.
pub(crate) fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let child = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(target_os = "macos"))]
    let child = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = child {
        tracing::warn!("could not open {url}: {e}");
    }
}

/// Extensions the OS "open" handler would run rather than display.
const LAUNCHABLE_EXTENSIONS: &[&str] = &[
    "app",
    "command",
    "terminal",
    "tool",
    "desktop",
    "sh",
    "bash",
    "zsh",
    "csh",
    "ksh",
    "fish",
    "workflow",
    "action",
    "scpt",
    "scptd",
    "applescript",
    "pkg",
    "mpkg",
    "prefpane",
    "jar",
    "appimage",
    "run",
    "exe",
    "bat",
    "cmd",
    "com",
    "msi",
    "ps1",
    "vbs",
    // Location/shortcut files: `open` follows them to another target, which
    // can itself be an app. Diffed PR files reach `open_file` too.
    "fileloc",
    "inetloc",
    "webloc",
    "url",
    "lnk",
    "scr",
];

/// Whether handing `path` to `open`/`xdg-open` would execute it (an app
/// bundle, a script, an executable) instead of showing it.
pub(crate) fn is_launchable(path: &Path) -> bool {
    let by_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| LAUNCHABLE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
    if by_ext {
        return true;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.is_dir() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return true;
        }
    }
    false
}

/// Open `path` in its default app (a PDF in Preview, …), off the UI thread.
/// Anything that would be run rather than shown is only revealed, and so is
/// a file nothing can open.
pub(crate) fn open_file(path: &Path) {
    let path = path.to_path_buf();
    std::thread::spawn(move || {
        if is_launchable(&path) {
            reveal(&path);
            return;
        }
        #[cfg(target_os = "macos")]
        let status = std::process::Command::new("open").arg(&path).status();
        #[cfg(not(target_os = "macos"))]
        let status = std::process::Command::new("xdg-open").arg(&path).status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                tracing::warn!("could not open {}: {s}", path.display());
                reveal(&path);
            }
            Err(e) => {
                tracing::warn!("could not open {}: {e}", path.display());
                reveal(&path);
            }
        }
    });
}

/// Folders a new session is likely to want, most relevant first, without
/// duplicates (by canonical path) and only ones that exist: recent session
/// folders (newest activity first, top 8), then registered projects.
pub(crate) fn folder_suggestions(sessions: &[Session], projects: &[PathBuf]) -> Vec<PathBuf> {
    let mut recent: Vec<&Session> = sessions.iter().collect();
    recent.sort_by_key(|s| std::cmp::Reverse(s.last_activity_at));
    let candidates = recent
        .into_iter()
        .map(|s| s.folder.clone())
        .take(8)
        .chain(projects.iter().cloned());
    dedup_existing(candidates, &[])
}

/// `candidates` in order, keeping only existing directories and dropping any
/// whose canonical path was already kept (or appears in `exclude`).
pub(crate) fn dedup_existing(
    candidates: impl IntoIterator<Item = PathBuf>,
    exclude: &[PathBuf],
) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = exclude
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    let mut out = Vec::new();
    for p in candidates {
        let Ok(canon) = p.canonicalize() else {
            continue;
        };
        if !canon.is_dir() || seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        out.push(p);
    }
    out
}

/// Folders recent Claude Code sessions ran in, newest first (by the mtime of
/// their `~/.claude/projects/<encoded>` dir). The dir name's encoding is lossy
/// (`/` and `.` both become `-`), so each folder is read from the `cwd` field
/// of the newest transcript inside instead. Best-effort: anything unreadable
/// is skipped.
pub(crate) fn claude_code_folders(home: &Path, limit: usize) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".claude").join("projects")) else {
        return Vec::new();
    };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_dir() {
                return None;
            }
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.0));
    dirs.into_iter()
        .take(limit * 2)
        .filter_map(|(_, dir)| newest_transcript_cwd(&dir))
        .take(limit)
        .collect()
}

/// The `cwd` recorded in the newest `.jsonl` transcript of a Claude Code
/// project dir.
fn newest_transcript_cwd(dir: &Path) -> Option<PathBuf> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|e| e.0)?
        .1;
    use std::io::BufRead;
    let file = std::fs::File::open(newest).ok()?;
    // The first lines can be summaries without a cwd; a few dozen is plenty.
    std::io::BufReader::new(file)
        .lines()
        .take(50)
        .map_while(Result::ok)
        .find_map(|line| cwd_of_line(&line))
}

fn cwd_of_line(line: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let cwd = v.get("cwd")?.as_str()?;
    (!cwd.is_empty()).then(|| PathBuf::from(cwd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expands_only_at_the_start() {
        let home = Path::new("/Users/me");
        assert_eq!(expand_tilde("~", Some(home)), PathBuf::from("/Users/me"));
        assert_eq!(
            expand_tilde(" ~/Downloads ", Some(home)),
            PathBuf::from("/Users/me/Downloads")
        );
        assert_eq!(expand_tilde("/a/~b", Some(home)), PathBuf::from("/a/~b"));
        assert_eq!(expand_tilde("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn home_is_shortened() {
        let home = Path::new("/Users/me");
        assert_eq!(shorten_home(Path::new("/Users/me/p"), Some(home)), "~/p");
        assert_eq!(shorten_home(Path::new("/Users/me"), Some(home)), "~");
        assert_eq!(shorten_home(Path::new("/tmp"), Some(home)), "/tmp");
    }

    #[test]
    fn relative_times() {
        assert_eq!(ago(0, 30_000), "just now");
        assert_eq!(ago(0, 5 * 60_000), "5 min ago");
        assert_eq!(ago(0, 3 * 3_600_000), "3 h ago");
        assert_eq!(ago(0, 49 * 3_600_000), "2 d ago");
    }

    #[test]
    fn suggestions_dedupe_and_drop_missing_folders() {
        let a = tempfile_dir();
        let missing = PathBuf::from("/definitely/not/here");
        let got = dedup_existing([a.clone(), missing, a.join("."), a.clone()], &[]);
        assert_eq!(got, vec![a.clone()]);
        assert!(dedup_existing([a.clone()], std::slice::from_ref(&a)).is_empty());
    }

    #[test]
    fn cwd_is_read_from_a_transcript_line() {
        assert_eq!(
            cwd_of_line(r#"{"type":"user","cwd":"/Users/me/bevy/run-2d"}"#),
            Some(PathBuf::from("/Users/me/bevy/run-2d"))
        );
        assert_eq!(cwd_of_line(r#"{"type":"summary"}"#), None);
        assert_eq!(cwd_of_line("not json"), None);
    }

    #[test]
    fn launchable_attachments_are_not_opened() {
        assert!(is_launchable(Path::new("/x/run.command")));
        assert!(is_launchable(Path::new("/x/Evil.APP")));
        assert!(is_launchable(Path::new("/x/a.desktop")));
        assert!(!is_launchable(Path::new("/x/missing.pdf")));

        let dir = tempfile_dir();
        assert!(is_launchable(&dir), "directories (bundles) are revealed");
        let doc = dir.join("doc.pdf");
        std::fs::write(&doc, b"%PDF").unwrap();
        assert!(!is_launchable(&doc));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let bin = dir.join("tool");
            std::fs::write(&bin, b"#!/bin/sh").unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(is_launchable(&bin));
        }
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("usine-sessions-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
