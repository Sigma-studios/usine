//! Account-level rate-limit usage for the provider CLIs: how much of the
//! session (~5h) and weekly windows is used, and when each resets. Feeds the
//! usage bar at the bottom of the app.
//!
//! - Claude: a stream-json `get_usage` control request to `claude -p` — no
//!   turn runs and no tokens are spent; the CLI fetches the plan's rate limits
//!   with its own login. The reply's shape is marked experimental by the CLI,
//!   so parsing is deliberately defensive: a format change hides the segment,
//!   never breaks the app. Fable's separate weekly bucket is parsed too, as
//!   `weekly_model`. (`claude -p /usage` was used before, but it now scans
//!   every transcript of the last week and takes about a minute.)
//! - Codex: `codex exec --json` doesn't emit its `rate_limits` snapshot on
//!   stdout, but the CLI records one in its session rollout files
//!   (`~/.codex/sessions/**/*.jsonl`) on every turn — including usine's own
//!   exec runs. The newest rollouts are read instead: fresh whenever Codex last
//!   ran anywhere, and windows whose reset has passed are dropped as stale.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

/// One rate-limit window (session or weekly). `resets_at` carries the reset as
/// a unix timestamp (both real providers); `resets_text` a pre-formatted
/// local-time phrasing ("Jul 21 at 8pm"), which only the simulator still
/// produces. The UI shows whichever side is present.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RateLimitWindow {
    /// 0–100.
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    /// Human-readable local reset time (simulator only).
    pub resets_text: Option<String>,
}

/// A provider's session + weekly windows. Any side may be missing — not
/// reported, unparsable, or expired.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderUsage {
    pub session: Option<RateLimitWindow>,
    pub weekly: Option<RateLimitWindow>,
    /// A model family with its own weekly cap, as the provider names it
    /// ("Fable") plus that window. Claude bills Fable against a separate weekly
    /// bucket, so a Fable card can hit a wall while `weekly` still reads
    /// comfortable. The name is carried rather than hardcoded, so a rename or a
    /// second such family needs no code change. Codex reports none.
    pub weekly_model: Option<(String, RateLimitWindow)>,
}

impl ProviderUsage {
    pub fn is_empty(&self) -> bool {
        self.session.is_none() && self.weekly.is_none() && self.weekly_model.is_none()
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
    /// Shown by the bar's hover tooltip, so it is set on every
    /// refresh even when the numbers themselves didn't move.
    pub refreshed_at: Option<i64>,
}

/// The `get_usage` control request answers in ~1s (it reads the plan's rate
/// limits from the API and skips the transcript scan); a hung CLI must not
/// wedge the poll loop. Kept well under the bar's 60s spinner safety timer.
const CLAUDE_USAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// The `request_id` usine tags its `get_usage` control request with, so the
/// matching `control_response` can be told apart from anything else on stdout.
const CLAUDE_USAGE_REQUEST_ID: &str = "usine-usage";

/// Ask the Claude CLI for the account's rate-limit usage. Any failure — binary
/// missing, timeout, error reply, rate limits unavailable — yields `None` and
/// is logged.
///
/// Sends one stream-json `get_usage` control request with `skip_behaviors`
/// (the "what's contributing to your usage" transcript scan, which takes a
/// minute on a busy machine and made `claude -p /usage` outlive any sane
/// timeout), then closes stdin. The CLI handles its own OAuth login and token
/// refresh; usine never touches the credentials.
pub async fn fetch_claude_usage() -> Option<ProviderUsage> {
    match tokio::time::timeout(CLAUDE_USAGE_TIMEOUT, request_claude_rate_limits()).await {
        Ok(Ok(rl)) => {
            let usage = parse_claude_rate_limits(&rl);
            if usage.is_empty() {
                // Normal for a fresh account with no window opened yet.
                tracing::debug!("claude usage: no rate-limit windows reported");
                None
            } else {
                Some(usage)
            }
        }
        Ok(Err(e)) => {
            tracing::warn!("claude usage unavailable: {e}");
            None
        }
        Err(_) => {
            tracing::warn!(
                "claude usage unavailable: no answer within {}s",
                CLAUDE_USAGE_TIMEOUT.as_secs()
            );
            None
        }
    }
}

/// How long the CLI gets to exit on its own once it has answered (stdin is
/// already closed, so it normally exits within about a second). Only past this
/// is it killed — a clean exit lets it stop the MCP servers it started and
/// finish writing `~/.claude.json` / refreshed credentials.
const CLAUDE_USAGE_EXIT_GRACE: Duration = Duration::from_secs(10);

/// Most stderr kept to explain a CLI that exits without answering.
const CLAUDE_STDERR_CAP: usize = 4096;

/// Spawn the CLI, send the `get_usage` request and return the reply's
/// `rate_limits` object. Dropping the future (on timeout) kills the child;
/// once the reply is in, the child is left to exit in the background.
async fn request_claude_rate_limits() -> Result<serde_json::Value, String> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    // Run from the data dir, not the app's cwd, and without session
    // persistence: `claude` keys its transcript storage on the cwd, and the
    // recurring poll would otherwise litter ~/.claude with throwaway session
    // files.
    let dir = crate::infra::paths::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut child = Command::new("claude")
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
        ])
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't start `claude`: {e}"))?;

    // Drain stderr concurrently (a full pipe would block the CLI), keeping the
    // head of it to explain an exit without an answer (not logged in, a flag a
    // future CLI rejects…).
    let stderr_task = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            let mut kept = Vec::new();
            let mut buf = [0u8; 1024];
            let mut stderr = stderr;
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let room = CLAUDE_STDERR_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            String::from_utf8_lossy(&kept).trim().to_string()
        })
    });

    let request = serde_json::json!({
        "type": "control_request",
        "request_id": CLAUDE_USAGE_REQUEST_ID,
        "request": { "subtype": "get_usage", "skip_behaviors": true },
    });
    {
        let mut stdin = child.stdin.take().ok_or("no stdin pipe")?;
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(|e| format!("couldn't send the usage request: {e}"))?;
        // Dropping stdin closes it: the CLI answers the control request and
        // exits instead of waiting for a prompt.
    }

    let stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| format!("couldn't read the CLI's output: {e}"))?
    {
        if let Some(reply) = match_usage_response(&line, CLAUDE_USAGE_REQUEST_ID) {
            // Let the CLI wind down by itself rather than force-killing it
            // mid-cleanup. Keep draining stdout so it never blocks on a full
            // pipe; only a CLI still alive after the grace period is killed.
            tokio::spawn(async move {
                let exited = tokio::time::timeout(CLAUDE_USAGE_EXIT_GRACE, async {
                    while let Ok(Some(_)) = lines.next_line().await {}
                    child.wait().await
                })
                .await;
                if exited.is_err() {
                    tracing::debug!("claude usage: CLI still running after answering; killing it");
                    let _ = child.kill().await;
                }
            });
            return reply;
        }
    }
    let status = child.wait().await.ok();
    let stderr = match stderr_task {
        Some(task) => task.await.unwrap_or_default(),
        None => String::new(),
    };
    let status = status.map_or_else(|| "unknown status".to_string(), |s| s.to_string());
    Err(if stderr.is_empty() {
        format!("the CLI exited without answering the usage request ({status})")
    } else {
        format!("the CLI exited without answering the usage request ({status}): {stderr}")
    })
}

/// Inspect one stdout line: `None` if it isn't the `control_response` for
/// `request_id`, otherwise the reply's `rate_limits` object or why there is
/// none. The response shape is marked experimental by the CLI, so the
/// rate-limit object is looked for at `response.response.rate_limits` first,
/// then `response.rate_limits`.
fn match_usage_response(line: &str, request_id: &str) -> Option<Result<serde_json::Value, String>> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type")?.as_str()? != "control_response" {
        return None;
    }
    let resp = v.get("response")?;
    let id = resp
        .get("request_id")
        .or_else(|| v.get("request_id"))
        .and_then(|i| i.as_str());
    if id != Some(request_id) {
        return None;
    }
    if resp.get("subtype").and_then(|s| s.as_str()) == Some("error") {
        let msg = resp
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error");
        return Some(Err(format!("the CLI refused the usage request: {msg}")));
    }
    let body = resp.get("response").unwrap_or(resp);
    if body.get("rate_limits_available").and_then(|b| b.as_bool()) == Some(false) {
        return Some(Err(
            "rate limits unavailable (not logged in to a claude.ai plan?)".to_string(),
        ));
    }
    let rl = body
        .get("rate_limits")
        .or_else(|| resp.get("rate_limits"))
        .filter(|rl| rl.is_object());
    Some(
        rl.cloned()
            .ok_or_else(|| "the reply carried no rate limits".to_string()),
    )
}

/// Map the `get_usage` reply's `rate_limits` object onto the bar's windows.
///
/// `limits[]` rows are read by `kind`: `session` (or `five_hour`) is the
/// session gauge, `weekly_all` the weekly one, and `weekly_scoped` with a model
/// name the per-model gauge (Fable's separate bucket). Should several
/// per-model rows appear, the tightest one wins (highest percentage, ties to
/// the first) — that is the cap a run hits first, and the bar has room for one
/// such gauge. Unknown kinds and rows without a numeric `percent` are skipped.
/// When `limits` is absent or yields nothing (a renamed `kind`), the older
/// fields are read instead: `five_hour` / `seven_day` (`utilization`,
/// `resets_at`) and `model_scoped[]`.
pub fn parse_claude_rate_limits(rl: &serde_json::Value) -> ProviderUsage {
    let mut usage = ProviderUsage::default();
    let window = |percent: Option<&serde_json::Value>, resets: Option<&serde_json::Value>| {
        Some(RateLimitWindow {
            used_percent: percent?.as_f64()?,
            resets_at: resets.and_then(|r| r.as_str()).and_then(parse_iso8601_secs),
            resets_text: None,
        })
    };
    let offer_model = |usage: &mut ProviderUsage, name: Option<&str>, w: RateLimitWindow| {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return;
        };
        let tighter = usage
            .weekly_model
            .as_ref()
            .is_none_or(|(_, cur)| w.used_percent > cur.used_percent);
        if tighter {
            usage.weekly_model = Some((name.to_string(), w));
        }
    };

    if let Some(limits) = rl.get("limits").and_then(|l| l.as_array()) {
        for row in limits {
            let Some(w) = window(row.get("percent"), row.get("resets_at")) else {
                continue;
            };
            match row.get("kind").and_then(|k| k.as_str()) {
                Some("session" | "five_hour") => usage.session = Some(w),
                Some("weekly_all") => usage.weekly = Some(w),
                Some("weekly_scoped") => {
                    let name = row
                        .pointer("/scope/model/display_name")
                        .and_then(|n| n.as_str());
                    offer_model(&mut usage, name, w);
                }
                _ => {}
            }
        }
        // Nothing recognised (a renamed `kind`?): the older fields below may
        // still be in the same reply, so fall through to them.
        if !usage.is_empty() {
            return usage;
        }
    }

    let legacy = |key: &str| {
        let w = rl.get(key)?;
        window(w.get("utilization"), w.get("resets_at"))
    };
    usage.session = legacy("five_hour");
    usage.weekly = legacy("seven_day");
    for row in rl
        .get("model_scoped")
        .and_then(|m| m.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(w) = window(row.get("utilization"), row.get("resets_at")) {
            offer_model(
                &mut usage,
                row.get("display_name").and_then(|n| n.as_str()),
                w,
            );
        }
    }
    usage
}

/// `2026-11-26T22:00:00.995455+00:00` → unix seconds. Accepts
/// `YYYY-MM-DDTHH:MM:SS`, optional fractional seconds (dropped), then `Z` or a
/// `±HH:MM` offset. Anything else is `None` (the gauge then shows no reset).
pub fn parse_iso8601_secs(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, min, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let rb = rest.as_bytes();
            if rb.len() != 6 || rb[3] != b':' {
                return None;
            }
            let sign = match rb[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits = |p: &str| -> Option<i64> {
                p.bytes()
                    .all(|c| c.is_ascii_digit())
                    .then(|| p.parse().ok())?
            };
            let (oh, om) = (digits(&rest[1..3])?, digits(&rest[4..6])?);
            if oh > 23 || om > 59 {
                return None;
            }
            sign * (oh * 3600 + om * 60)
        }
    };
    // Days since the unix epoch for a proleptic Gregorian date (Howard
    // Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + min * 60 + sec - offset)
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
        // Codex reports no per-model bucket.
        weekly_model: None,
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

    /// A real `get_usage` reply's `rate_limits`, trimmed.
    const CLAUDE_RATE_LIMITS: &str = r#"{
        "five_hour": null,
        "seven_day": {"utilization": 39, "resets_at": "2026-11-26T22:00:00.995455+00:00"},
        "limits": [
            {"kind": "weekly_all", "group": "weekly", "percent": 39, "severity": "normal",
             "resets_at": "2026-11-26T22:00:00.995455+00:00", "scope": null},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal",
             "resets_at": "2026-11-26T22:00:00+00:00",
             "scope": {"model": {"display_name": "Fable", "id": null}, "surface": null}}
        ],
        "model_scoped": [{"display_name": "Fable", "utilization": 0, "resets_at": "2026-11-26T22:00:00+00:00"}]
    }"#;

    fn json(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn claude_rate_limits_parse_the_weekly_and_per_model_rows() {
        let usage = parse_claude_rate_limits(&json(CLAUDE_RATE_LIMITS));
        assert!(usage.session.is_none());
        let weekly = usage.weekly.expect("weekly window");
        assert_eq!(weekly.used_percent, 39.0);
        assert_eq!(weekly.resets_at, Some(1795730400));
        assert_eq!(weekly.resets_text, None);
        let (name, fable) = usage.weekly_model.expect("per-model weekly window");
        assert_eq!(name, "Fable");
        assert_eq!(fable.used_percent, 0.0);
        assert_eq!(fable.resets_at, Some(1795730400));
    }

    #[test]
    fn a_session_row_becomes_the_session_window() {
        let rl = json(
            r#"{"limits":[{"kind":"session","percent":12.5,"resets_at":"2026-11-24T18:00:00Z"}]}"#,
        );
        let session = parse_claude_rate_limits(&rl).session.expect("session");
        assert_eq!(session.used_percent, 12.5);
        assert_eq!(session.resets_at, Some(1795543200));
    }

    #[test]
    fn the_tightest_per_model_weekly_cap_wins() {
        let rl = json(
            r#"{"limits":[
                {"kind":"weekly_scoped","percent":12,"scope":{"model":{"display_name":"Fable"}}},
                {"kind":"weekly_scoped","percent":71,"scope":{"model":{"display_name":"Mythos"}}},
                {"kind":"weekly_scoped","percent":40,"scope":{"model":{"display_name":"Other"}}}
            ]}"#,
        );
        let (name, window) = parse_claude_rate_limits(&rl)
            .weekly_model
            .expect("per-model weekly window");
        assert_eq!(name, "Mythos");
        assert_eq!(window.used_percent, 71.0);
        assert_eq!(window.resets_at, None);
    }

    #[test]
    fn null_limits_fall_back_to_the_legacy_fields() {
        let mut rl = json(CLAUDE_RATE_LIMITS);
        rl["limits"] = serde_json::Value::Null;
        rl["five_hour"] = json(r#"{"utilization": 7, "resets_at": "2026-11-24T18:00:00Z"}"#);
        let usage = parse_claude_rate_limits(&rl);
        assert_eq!(usage.session.expect("session").used_percent, 7.0);
        let weekly = usage.weekly.expect("weekly");
        assert_eq!(weekly.used_percent, 39.0);
        assert_eq!(weekly.resets_at, Some(1795730400));
        assert_eq!(usage.weekly_model.expect("per-model").0, "Fable");
    }

    #[test]
    fn unrecognised_limit_kinds_fall_back_to_the_legacy_fields() {
        let mut rl = json(CLAUDE_RATE_LIMITS);
        rl["limits"] = json(r#"[{"kind":"weekly","percent":39}]"#);
        let usage = parse_claude_rate_limits(&rl);
        let weekly = usage.weekly.expect("weekly from seven_day");
        assert_eq!(weekly.used_percent, 39.0);
        assert_eq!(weekly.resets_at, Some(1795730400));
        assert_eq!(usage.weekly_model.expect("per-model").0, "Fable");
    }

    #[test]
    fn unexpected_rate_limit_shapes_are_empty_not_a_panic() {
        assert!(parse_claude_rate_limits(&json(r#"{"limits":[]}"#)).is_empty());
        assert!(parse_claude_rate_limits(&json("{}")).is_empty());
        assert!(parse_claude_rate_limits(&json("null")).is_empty());
        let odd = json(
            r#"{"limits":[
                {"kind":"brand_new_kind","percent":50},
                {"kind":"weekly_all","percent":"50%"},
                {"kind":"weekly_scoped","percent":9,"scope":null},
                {"percent":3}
            ]}"#,
        );
        assert!(parse_claude_rate_limits(&odd).is_empty());
    }

    #[test]
    fn iso8601_timestamps_convert_to_unix_seconds() {
        assert_eq!(parse_iso8601_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601_secs("2026-11-26T22:00:00Z"), Some(1795730400));
        assert_eq!(
            parse_iso8601_secs("2026-11-26T22:00:00+00:00"),
            Some(1795730400)
        );
        assert_eq!(
            parse_iso8601_secs("2026-11-26T22:00:00.995455+00:00"),
            Some(1795730400)
        );
        // 22:00 at +02:00 is 20:00 UTC: two hours earlier.
        assert_eq!(
            parse_iso8601_secs("2026-11-26T22:00:00+02:00"),
            Some(1795730400 - 7200)
        );
        assert_eq!(
            parse_iso8601_secs("2026-11-26T22:00:00-05:30"),
            Some(1795730400 + 5 * 3600 + 1800)
        );
        // Leap day.
        assert_eq!(parse_iso8601_secs("2024-02-29T00:00:00Z"), Some(1709164800));
        for bad in [
            "",
            "garbage",
            "2026-11-26",
            "2026-11-26T22:00:00",
            "2026-11-26 22:00:00Z",
            "2026-13-26T22:00:00Z",
            "2026-11-26T22:00:00.Z",
            "2026-11-26T22:00:00+0200",
            "2026-11-26T2a:00:00Z",
        ] {
            assert_eq!(parse_iso8601_secs(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_the_matching_control_response_is_taken() {
        let ok = r#"{"type":"control_response","response":{"subtype":"success","request_id":"usine-usage","response":{"rate_limits_available":true,"rate_limits":{"limits":[]}}}}"#;
        assert_eq!(
            match_usage_response(ok, "usine-usage"),
            Some(Ok(json(r#"{"limits":[]}"#)))
        );
        // Another request's reply, other message types and noise are skipped.
        assert_eq!(match_usage_response(ok, "someone-else"), None);
        assert_eq!(
            match_usage_response(r#"{"type":"system","subtype":"init"}"#, "usine-usage"),
            None
        );
        assert_eq!(match_usage_response("not json", "usine-usage"), None);
    }

    #[test]
    fn error_and_unavailable_replies_yield_an_error() {
        let err = r#"{"type":"control_response","response":{"subtype":"error","request_id":"usine-usage","error":"Unsupported control request"}}"#;
        assert!(matches!(
            match_usage_response(err, "usine-usage"),
            Some(Err(_))
        ));
        let unavailable = r#"{"type":"control_response","response":{"subtype":"success","request_id":"usine-usage","response":{"rate_limits_available":false,"rate_limits":null}}}"#;
        assert!(matches!(
            match_usage_response(unavailable, "usine-usage"),
            Some(Err(_))
        ));
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
