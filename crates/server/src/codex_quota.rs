//! Codex plan-quota reader — the "can I keep working?" signal for Codex,
//! the counterpart to the Anthropic header path in [`crate::proxy`].
//!
//! Codex does not expose its rate limits on proxy-visible response headers;
//! instead its CLI records them to the session rollout log at
//! `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`. Each `token_count` event
//! carries a `rate_limits` object:
//!
//! ```json
//! { "timestamp": "2026-08-10T17:39:12.276Z",
//!   "payload": { "type": "token_count",
//!     "rate_limits": {
//!       "primary":   { "used_percent": 45.0, "resets_in_seconds": 7200, "window_minutes": 300 },
//!       "secondary": { "used_percent": 60.0, "resets_in_seconds": 500000, "window_minutes": 10080 } } } }
//! ```
//!
//! `primary` is the short (≈5h) window, `secondary` the long (weekly) one;
//! either may be `null` (e.g. credit-based workspace plans, which report no
//! windowed limit at all). `resets_in_seconds` is relative to the line's
//! `timestamp`, so the absolute reset is `timestamp + resets_in_seconds`.
//!
//! We tail the newest session file for the last line that carries a
//! populated `rate_limits`, poll on a slow cadence, and emit
//! `Event::AgentProviderQuota { agent_id: "codex", .. }` when it changes.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use lazybox_ipc::{ProviderQuota, QuotaWindow};

/// The agent id Codex quota is attributed to — matches the built-in Codex
/// agent's id, so the header widget shows it on the Codex row.
const CODEX_AGENT_ID: &str = "codex";

/// How often to re-read the newest session file. Codex writes a
/// `token_count` event per turn, so a slow poll keeps the widget fresh
/// without watching the file; the quota only changes when Codex runs.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

/// How much of the newest rollout file to read, from the end. Codex writes a
/// `token_count` event every turn, so the last populated `rate_limits` line
/// sits comfortably within the final stretch; reading only the tail bounds
/// each poll's IO regardless of a long session's total size (rollout files
/// grow into the tens of MB), instead of re-reading the whole file.
const TAIL_BYTES: u64 = 256 * 1024;

/// Parse one session-log line into a [`ProviderQuota`]. Returns `None` for a
/// line that is not a `token_count` event, carries no populated window, or
/// isn't valid JSON — the caller keeps scanning older lines.
pub fn parse_session_line(line: &str) -> Option<ProviderQuota> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let payload = value.get("payload")?;
    if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
        return None;
    }
    let rate_limits = payload.get("rate_limits")?;
    let stamped = value
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc).timestamp());

    let quota = ProviderQuota {
        five_hour: window(rate_limits.get("primary"), stamped),
        weekly: window(rate_limits.get("secondary"), stamped),
    };
    (!quota.is_empty()).then_some(quota)
}

fn window(value: Option<&serde_json::Value>, stamped: Option<i64>) -> Option<QuotaWindow> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    let used_percent = value.get("used_percent")?.as_f64()?;
    if !used_percent.is_finite() || used_percent < 0.0 {
        return None;
    }
    let utilization_bp = ((used_percent * 100.0).round() as i64).clamp(0, 10_000) as u32;
    let reset_at = value
        .get("resets_in_seconds")
        .and_then(|s| s.as_i64())
        .zip(stamped)
        .map(|(secs, base)| base + secs);
    Some(QuotaWindow {
        utilization_bp,
        reset_at,
    })
}

/// The most recent populated quota in `sessions_dir`, scanning only the tail
/// of the newest rollout file. `None` when the directory is absent, empty, or
/// carries no windowed limit (a credits-only plan).
pub fn read_latest_quota(sessions_dir: &Path) -> Option<ProviderQuota> {
    let (newest, _mtime) = newest_session_file(sessions_dir)?;
    read_quota_from_tail(&newest)
}

/// Read the last [`TAIL_BYTES`] of `path` and return the newest populated
/// quota in it, scanning complete lines from the end. Bounds the read so a
/// multi-megabyte rollout doesn't get slurped whole every poll. `None` if the
/// tail carries no populated `rate_limits` line.
fn read_quota_from_tail(path: &Path) -> Option<ProviderQuota> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    // When we started mid-file the first line is very likely a fragment of a
    // larger line; skip past the first newline so we never hand a truncated
    // JSON object to the parser. At offset 0 the whole file is present, so
    // the first line is real and we keep it.
    let scan: &str = if start > 0 {
        match text.find('\n') {
            Some(i) => &text[i + 1..],
            None => "",
        }
    } else {
        &text
    };
    scan.lines().rev().find_map(parse_session_line)
}

/// The most-recently-modified `rollout-*.jsonl` under `sessions_dir`
/// (recursively — Codex nests them under `YYYY/MM/DD/`), paired with its
/// modification time so a caller can skip re-reading an unchanged file.
fn newest_session_file(sessions_dir: &Path) -> Option<(PathBuf, SystemTime)> {
    fn walk(dir: &Path, best: &mut Option<(std::time::SystemTime, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&path, best);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
                && let Ok(modified) = meta.modified()
                && best.as_ref().is_none_or(|(t, _)| modified > *t)
            {
                *best = Some((modified, path));
            }
        }
    }
    let mut best = None;
    walk(sessions_dir, &mut best);
    best.map(|(mtime, path)| (path, mtime))
}

/// One poll's outcome against the last-seen newest-file mtime.
enum QuotaPoll {
    /// No rollout file exists yet.
    Absent,
    /// The newest file's mtime matches the previous poll — nothing re-read.
    Unchanged,
    /// The newest file is new or changed since the last poll: its mtime, and
    /// the quota its tail carries (`None` if no populated window is present).
    Fresh {
        mtime: SystemTime,
        quota: Option<ProviderQuota>,
    },
}

/// Locate the newest rollout file and, only when its mtime differs from
/// `since`, read its tail for the latest quota. Skipping the read on an
/// unchanged mtime is the common case — the poll loop runs forever, but the
/// quota only moves while Codex is actively writing.
fn poll_quota(sessions_dir: &Path, since: Option<SystemTime>) -> QuotaPoll {
    let Some((newest, mtime)) = newest_session_file(sessions_dir) else {
        return QuotaPoll::Absent;
    };
    if Some(mtime) == since {
        return QuotaPoll::Unchanged;
    }
    QuotaPoll::Fresh {
        mtime,
        quota: read_quota_from_tail(&newest),
    }
}

/// The default Codex sessions directory, `~/.codex/sessions`.
fn default_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex").join("sessions"))
}

/// Poll the Codex session log and broadcast `AgentProviderQuota` whenever the
/// observed quota changes. Runs only while metering is on (the same opt-in
/// that powers the header widget), so a user who hasn't opted into usage
/// tracking pays nothing. Returns the task handle, or `None` when there is no
/// home directory to read from.
pub fn spawn(config: &crate::ServerConfig) -> Option<tokio::task::JoinHandle<()>> {
    let cfg = lazybox_config::Config::load().unwrap_or_default();
    if !cfg.agent.metering_proxy {
        return None;
    }
    let sessions_dir = default_sessions_dir()?;
    let bus = config.bus.clone();
    Some(tokio::spawn(async move {
        let mut last: Option<ProviderQuota> = None;
        let mut last_mtime: Option<SystemTime> = None;
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let dir = sessions_dir.clone();
            let since = last_mtime;
            let poll = tokio::task::spawn_blocking(move || poll_quota(&dir, since))
                .await
                .unwrap_or(QuotaPoll::Absent);
            let QuotaPoll::Fresh { mtime, quota } = poll else {
                continue;
            };
            last_mtime = Some(mtime);
            if let Some(quota) = quota
                && last.as_ref() != Some(&quota)
            {
                last = Some(quota);
                let _ = bus.send(lazybox_ipc::Event::AgentProviderQuota {
                    agent_id: CODEX_AGENT_ID.to_string(),
                    // Sourced from the shared Codex session log, not a proxied
                    // per-session request — no workspace attribution.
                    session_key: None,
                    quota,
                });
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_windows_with_absolute_reset() {
        let base = DateTime::parse_from_rfc3339("2026-08-10T00:00:00.000Z")
            .unwrap()
            .timestamp();
        let line = r#"{"timestamp":"2026-08-10T00:00:00.000Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":45.0,"resets_in_seconds":7200,"window_minutes":300},"secondary":{"used_percent":60.0,"resets_in_seconds":100,"window_minutes":10080}}}}"#;
        let quota = parse_session_line(line).expect("populated quota");
        let five = quota.five_hour.expect("primary");
        assert_eq!(five.utilization_bp, 4500);
        assert_eq!(five.reset_at, Some(base + 7200));
        assert_eq!(quota.weekly.unwrap().utilization_bp, 6000);
    }

    #[test]
    fn null_windows_are_not_a_quota() {
        // Credits-based plan: primary/secondary null → nothing to show.
        let line = r#"{"timestamp":"2026-08-10T00:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":null,"secondary":null,"credits":{"has_credits":false}}}}"#;
        assert!(parse_session_line(line).is_none());
    }

    #[test]
    fn non_token_count_lines_are_skipped() {
        let line = r#"{"timestamp":"2026-08-10T00:00:00Z","payload":{"type":"agent_message","text":"hi"}}"#;
        assert!(parse_session_line(line).is_none());
    }

    #[test]
    fn a_window_missing_its_reset_still_reports_utilization() {
        let line = r#"{"timestamp":"2026-08-10T00:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.5},"secondary":null}}}"#;
        let five = parse_session_line(line).unwrap().five_hour.unwrap();
        assert_eq!(five.utilization_bp, 1250);
        assert_eq!(five.reset_at, None);
    }

    #[test]
    fn read_latest_scans_newest_file_from_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("2026/08/10");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("rollout-2026-08-10T00-00-00-abc.jsonl");
        // Older populated line, then a newer populated line last → last wins.
        let older = r#"{"timestamp":"2026-08-10T00:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":10.0,"resets_in_seconds":60},"secondary":null}}}"#;
        let newer = r#"{"timestamp":"2026-08-10T00:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":80.0,"resets_in_seconds":60},"secondary":null}}}"#;
        std::fs::write(&file, format!("{older}\n{newer}\n")).unwrap();
        let quota = read_latest_quota(dir.path()).expect("quota from newest file");
        assert_eq!(quota.five_hour.unwrap().utilization_bp, 8000);
    }

    #[test]
    fn poll_skips_reread_when_mtime_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("2026/08/10");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("rollout-2026-08-10T00-00-00-abc.jsonl");
        let line = r#"{"timestamp":"2026-08-10T00:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":42.0,"resets_in_seconds":60},"secondary":null}}}"#;
        std::fs::write(&file, format!("{line}\n")).unwrap();

        // First poll (no prior mtime) reads the file and reports the quota.
        let QuotaPoll::Fresh { mtime, quota } = poll_quota(dir.path(), None) else {
            panic!("first poll should be Fresh");
        };
        assert_eq!(quota.unwrap().five_hour.unwrap().utilization_bp, 4200);

        // Second poll with the same mtime skips the read entirely.
        assert!(
            matches!(poll_quota(dir.path(), Some(mtime)), QuotaPoll::Unchanged),
            "an unchanged mtime must not re-read the file"
        );

        // A stale `since` (older than the file) forces a re-read.
        assert!(
            matches!(
                poll_quota(dir.path(), Some(SystemTime::UNIX_EPOCH)),
                QuotaPoll::Fresh { .. }
            ),
            "a differing mtime must re-read"
        );
    }

    #[test]
    fn poll_reports_absent_without_any_rollout_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(poll_quota(dir.path(), None), QuotaPoll::Absent));
    }

    #[test]
    fn tail_read_finds_the_last_line_past_a_leading_fragment() {
        // A newest line whose byte offset forces the tail window to begin in
        // the middle of an earlier, oversized line. The leading fragment must
        // be discarded (not mis-parsed) and the real last line still found.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rollout-2026-08-10T00-00-00-big.jsonl");
        let filler = format!("{}\n", "x".repeat((TAIL_BYTES as usize) + 4_096));
        let newest = r#"{"timestamp":"2026-08-10T00:00:00Z","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":77.0,"resets_in_seconds":60},"secondary":null}}}"#;
        std::fs::write(&file, format!("{filler}{newest}\n")).unwrap();
        let quota = read_quota_from_tail(&file).expect("quota from tail");
        assert_eq!(quota.five_hour.unwrap().utilization_bp, 7700);
    }
}
