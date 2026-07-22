//! Account-level rate-limit usage for the provider CLIs: how much of the
//! session (~5h) and weekly windows is used, and when each resets. Feeds the
//! usage bar at the bottom of the app.
//!
//! - Claude: `claude -p "/usage" --output-format json` is a local command — no
//!   turn runs and no tokens are spent (`num_turns: 0`, zero cost). The
//!   percentages only exist in the human-readable `result` text, so parsing is
//!   deliberately defensive: a wording change hides the segment, never breaks
//!   the app.
//! - Codex: `codex exec --json` doesn't emit its `rate_limits` snapshot on
//!   stdout, but the CLI records one in its session rollout files
//!   (`~/.codex/sessions/**/*.jsonl`) on every turn — including usine's own
//!   exec runs. The newest rollouts are read instead: fresh whenever Codex last
//!   ran anywhere, and windows whose reset has passed are dropped as stale.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

/// One rate-limit window (session or weekly). `resets_text` carries Claude's
/// pre-formatted local-time phrasing ("Jul 21 at 8pm"); `resets_at` carries
/// Codex's unix timestamp. The UI shows whichever side is present.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RateLimitWindow {
    /// 0–100.
    pub used_percent: f64,
    /// Unix seconds (Codex).
    pub resets_at: Option<i64>,
    /// Human-readable local reset time (Claude), timezone suffix stripped.
    pub resets_text: Option<String>,
}

/// A provider's session + weekly windows. Either side may be missing — not
/// reported, unparsable, or expired.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderUsage {
    pub session: Option<RateLimitWindow>,
    pub weekly: Option<RateLimitWindow>,
}

impl ProviderUsage {
    pub fn is_empty(&self) -> bool {
        self.session.is_none() && self.weekly.is_none()
    }
}

/// Everything the usage bar shows: one optional entry per provider. `None`
/// means "nothing to show" (CLI missing, not logged in, no fresh data) — the
/// UI hides that segment rather than render a stale or empty gauge.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageSnapshot {
    pub claude: Option<ProviderUsage>,
    pub codex: Option<ProviderUsage>,
    /// Unix millis of the poll that produced this snapshot (`None` = never).
    /// Shown by the bar's refresh-button tooltip, so it is set on every
    /// refresh even when the numbers themselves didn't move.
    pub refreshed_at: Option<i64>,
}

/// `/usage` answers in ~2s; a hung CLI must not wedge the poll loop.
const CLAUDE_USAGE_TIMEOUT: Duration = Duration::from_secs(45);

/// Ask the Claude CLI for the account's rate-limit usage. Any failure — binary
/// missing, timeout, error result, unrecognized text — yields `None`.
pub async fn fetch_claude_usage() -> Option<ProviderUsage> {
    // Run from the data dir, not the app's cwd, and without session
    // persistence: `claude` keys its transcript storage on the cwd, and the
    // recurring poll would otherwise litter ~/.claude with throwaway session
    // files.
    let dir = crate::infra::paths::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let output = tokio::time::timeout(
        CLAUDE_USAGE_TIMEOUT,
        Command::new("claude")
            .args([
                "-p",
                "/usage",
                "--output-format",
                "json",
                "--no-session-persistence",
            ])
            .current_dir(&dir)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    if v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(true) {
        return None;
    }
    let usage = parse_claude_usage_text(v.get("result")?.as_str()?);
    (!usage.is_empty()).then_some(usage)
}

/// Parse the `/usage` result text, e.g.:
///
/// ```text
/// Current session: 4% used · resets Jul 21 at 8pm (Europe/Paris)
/// Current week (all models): 44% used · resets Jul 24 at 12am (Europe/Paris)
/// ```
///
/// The per-model week line ("Current week (Fable): …") is deliberately skipped —
/// the bar shows one weekly gauge per provider.
pub fn parse_claude_usage_text(text: &str) -> ProviderUsage {
    let mut usage = ProviderUsage::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Current session:") {
            usage.session = parse_claude_usage_line(rest);
        } else if let Some(rest) = line.strip_prefix("Current week (all models):") {
            usage.weekly = parse_claude_usage_line(rest);
        }
    }
    usage
}

/// Parse one window's tail: `4% used · resets Jul 21 at 8pm (Europe/Paris)`.
fn parse_claude_usage_line(rest: &str) -> Option<RateLimitWindow> {
    let rest = rest.trim();
    let used_percent = rest.split('%').next()?.trim().parse::<f64>().ok()?;
    let resets_text = rest
        .split("resets ")
        .nth(1)
        .map(|s| strip_timezone(s.trim()).to_string());
    Some(RateLimitWindow {
        used_percent,
        resets_at: None,
        resets_text,
    })
}

/// `Jul 21 at 8pm (Europe/Paris)` → `Jul 21 at 8pm`.
fn strip_timezone(s: &str) -> &str {
    match s.rfind(" (") {
        Some(i) if s.ends_with(')') => &s[..i],
        _ => s,
    }
}

/// The Codex session-rollout root: `$CODEX_HOME/sessions`, defaulting to
/// `~/.codex/sessions`.
pub fn codex_sessions_root() -> Option<PathBuf> {
    match std::env::var("CODEX_HOME") {
        Ok(home) if !home.is_empty() => Some(PathBuf::from(home).join("sessions")),
        _ => directories::BaseDirs::new().map(|b| b.home_dir().join(".codex").join("sessions")),
    }
}

/// How many of the newest rollout files to scan before concluding there is no
/// rate-limit data (a tiny aborted session may not contain a snapshot).
const CODEX_SCAN_FILES: usize = 5;

/// Read the last `rate_limits` snapshot Codex recorded, newest rollout first.
/// Windows whose reset time has passed are dropped — their percentage describes
/// a window that has since rolled over.
pub fn read_codex_usage(sessions_root: &Path, now_secs: i64) -> Option<ProviderUsage> {
    let mut files = collect_jsonl_files(sessions_root);
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, path) in files.into_iter().take(CODEX_SCAN_FILES) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let found = content
            .lines()
            .rev()
            .filter(|l| l.contains("\"rate_limits\""))
            .find_map(|l| parse_codex_rate_limits(l, now_secs));
        if let Some(usage) = found {
            // Older files are staler still, so an expired newest snapshot
            // means there is nothing worth showing.
            return (!usage.is_empty()).then_some(usage);
        }
    }
    None
}

/// Every `.jsonl` under `root` (rollouts nest as `YYYY/MM/DD/…`), with mtime.
fn collect_jsonl_files(root: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                if let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) {
                    out.push((mtime, path));
                }
            }
        }
    }
    out
}

/// Parse one rollout line carrying `payload.rate_limits`: `primary` is the
/// session window (300-minute), `secondary` the weekly (10080-minute), each
/// with `used_percent` and a `resets_at` unix timestamp.
fn parse_codex_rate_limits(line: &str, now_secs: i64) -> Option<ProviderUsage> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let rl = v.get("payload")?.get("rate_limits")?;
    let window = |key: &str| -> Option<RateLimitWindow> {
        let w = rl.get(key)?;
        let used_percent = w.get("used_percent")?.as_f64()?;
        let resets_at = w.get("resets_at").and_then(|r| r.as_i64());
        if resets_at.is_some_and(|t| t <= now_secs) {
            return None;
        }
        Some(RateLimitWindow {
            used_percent,
            resets_at,
            resets_text: None,
        })
    };
    Some(ProviderUsage {
        session: window("primary"),
        weekly: window("secondary"),
    })
}

/// Current unix time in seconds, for the freshness cutoff.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_usage_text_parses_both_windows_and_strips_the_timezone() {
        let text = "You are currently using your subscription to power your Claude Code usage\n\n\
                    Current session: 4% used · resets Jul 21 at 8pm (Europe/Paris)\n\
                    Current week (all models): 44% used · resets Jul 24 at 12am (Europe/Paris)\n\
                    Current week (Fable): 12% used · resets Jul 24 at 12am (Europe/Paris)\n";
        let usage = parse_claude_usage_text(text);
        let session = usage.session.expect("session window");
        assert_eq!(session.used_percent, 4.0);
        assert_eq!(session.resets_text.as_deref(), Some("Jul 21 at 8pm"));
        let weekly = usage.weekly.expect("weekly window");
        assert_eq!(weekly.used_percent, 44.0);
        assert_eq!(weekly.resets_text.as_deref(), Some("Jul 24 at 12am"));
    }

    #[test]
    fn claude_usage_text_without_the_expected_lines_is_empty() {
        assert!(parse_claude_usage_text("Log in to see usage.").is_empty());
        // A wording change in the percentage part must not panic or invent data.
        assert!(parse_claude_usage_text("Current session: unlimited").is_empty());
    }

    /// A real (sanitized) rollout line from `~/.codex/sessions`.
    const CODEX_LINE: &str = r#"{"timestamp":"2026-04-09T08:06:59.848Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":5.0,"window_minutes":300,"resets_at":1775740019},"secondary":{"used_percent":51.0,"window_minutes":10080,"resets_at":1776326819},"credits":null,"plan_type":"plus"}}}"#;

    #[test]
    fn codex_rate_limits_parse_primary_as_session_and_secondary_as_weekly() {
        let usage = parse_codex_rate_limits(CODEX_LINE, 1775000000).expect("parses");
        let session = usage.session.expect("session window");
        assert_eq!(session.used_percent, 5.0);
        assert_eq!(session.resets_at, Some(1775740019));
        assert_eq!(usage.weekly.expect("weekly window").used_percent, 51.0);
    }

    #[test]
    fn codex_windows_past_their_reset_are_dropped_as_stale() {
        // "Now" is between the session reset and the weekly reset: the session
        // percentage describes a window that already rolled over, the weekly
        // one is still live.
        let usage = parse_codex_rate_limits(CODEX_LINE, 1776000000).expect("parses");
        assert!(usage.session.is_none());
        assert!(usage.weekly.is_some());

        // Past both resets there is nothing left to show.
        let usage = parse_codex_rate_limits(CODEX_LINE, 1790000000).expect("parses");
        assert!(usage.is_empty());
    }

    #[test]
    fn read_codex_usage_takes_the_last_snapshot_of_the_newest_rollout() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026").join("04").join("09");
        std::fs::create_dir_all(&day).unwrap();
        // An older snapshot line followed by the one that should win.
        let newer = CODEX_LINE.replace("\"used_percent\":5.0", "\"used_percent\":9.0");
        std::fs::write(
            day.join("rollout-a.jsonl"),
            format!("{CODEX_LINE}\n{newer}\n"),
        )
        .unwrap();
        let usage = read_codex_usage(dir.path(), 1775000000).expect("snapshot");
        assert_eq!(usage.session.expect("session").used_percent, 9.0);
    }
}
