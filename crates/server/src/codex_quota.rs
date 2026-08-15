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
use std::time::Duration;

use chrono::{DateTime, Utc};
use lazybox_ipc::{ProviderQuota, QuotaWindow};

/// The agent id Codex quota is attributed to — matches the built-in Codex
/// agent's id, so the header widget shows it on the Codex row.
const CODEX_AGENT_ID: &str = "codex";

/// How often to re-read the newest session file. Codex writes a
/// `token_count` event per turn, so a slow poll keeps the widget fresh
/// without watching the file; the quota only changes when Codex runs.
const POLL_INTERVAL: Duration = Duration::from_secs(30);

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

/// The most recent populated quota in `sessions_dir`, scanning the newest
/// rollout file from the end. `None` when the directory is absent, empty, or
/// carries no windowed limit (a credits-only plan).
pub fn read_latest_quota(sessions_dir: &Path) -> Option<ProviderQuota> {
    let newest = newest_session_file(sessions_dir)?;
    let contents = std::fs::read_to_string(&newest).ok()?;
    contents.lines().rev().find_map(parse_session_line)
}

/// The most-recently-modified `rollout-*.jsonl` under `sessions_dir`
/// (recursively — Codex nests them under `YYYY/MM/DD/`).
fn newest_session_file(sessions_dir: &Path) -> Option<PathBuf> {
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
    best.map(|(_, path)| path)
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
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        loop {
            ticker.tick().await;
            let dir = sessions_dir.clone();
            let quota = tokio::task::spawn_blocking(move || read_latest_quota(&dir))
                .await
                .ok()
                .flatten();
            if let Some(quota) = quota
                && last.as_ref() != Some(&quota)
            {
                last = Some(quota);
                let _ = bus.send(lazybox_ipc::Event::AgentProviderQuota {
                    agent_id: CODEX_AGENT_ID.to_string(),
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
}
