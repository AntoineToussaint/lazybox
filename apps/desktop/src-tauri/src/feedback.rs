//! Help → Send feedback (packages/surfaces/src/FeedbackDialog.tsx): what the
//! report may attach, and writing it to a folder of its own. Nothing here
//! sends anything anywhere: the person reads the report in the window, then
//! saves it, copies it, or posts it as a GitHub issue themselves.
//!
//! The window redacts (feedback-model.ts `redact`) before it shows anything,
//! so the preview is exactly what is saved; the raw log tail never leaves the
//! app's own processes.

use base64::Engine;
use serde::Serialize;
use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
};
use tauri::Manager;

/// The most of the Runner's log a report reads: its end, at most this many bytes and lines.
const LOG_BYTES: u64 = 96 * 1024;
const LOG_LINES: usize = 200;
const MAX_REPORT: usize = 1024 * 1024;
const MAX_SCREENSHOT: usize = 32 * 1024 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerLog {
    path: String,
    tail: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    os: String,
    arch: String,
    home: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner_log: Option<RunnerLog>,
}

/// The last `lines` complete lines of a file, read from at most its last `bytes`.
pub fn tail(path: &Path, bytes: u64, lines: usize) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    file.take(bytes).read_to_end(&mut buffer)?;
    let text = String::from_utf8_lossy(&buffer);
    // A read that starts mid-file starts mid-line: drop that partial line.
    let text = if start > 0 {
        text.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        &text
    };
    let all: Vec<&str> = text.trim_end().lines().collect();
    Ok(all[all.len().saturating_sub(lines)..].join("\n"))
}

fn macos_version() -> String {
    Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// Versions are the window's to read (the app's, the Runner's); this adds
/// what only the native side knows: macOS, the home directory to redact,
/// and an installed release's Runner log.
#[tauri::command]
pub async fn feedback_diagnostics(app: tauri::AppHandle) -> Result<Diagnostics, String> {
    let home = app
        .path()
        .home_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let release = crate::release::Release::detect(&app);
    tauri::async_runtime::spawn_blocking(move || {
        let runner_log = release
            .and_then(|r| r.status().ok())
            .map(|status| status.log)
            .filter(|log| !log.is_empty())
            .and_then(|log| {
                tail(Path::new(&log), LOG_BYTES, LOG_LINES)
                    .ok()
                    .map(|tail| RunnerLog { path: log, tail })
            });
        Diagnostics {
            os: macos_version(),
            arch: std::env::consts::ARCH.to_string(),
            home,
            runner_log,
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// "2026-11-17 09.41.07", local time when `date` answers, otherwise UTC.
fn stamp() -> String {
    if let Some(local) = Command::new("/bin/date")
        .arg("+%Y-%m-%d %H.%M.%S")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| s.len() == 19)
    {
        return local;
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    utc_stamp(seconds)
}

/// A Unix time as "YYYY-MM-DD HH.MM.SS" in UTC (days to civil date, Howard Hinnant's algorithm).
pub fn utc_stamp(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}.{:02}.{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Writes `feedback.md` (and `screenshot.png`) to a new folder in `parent`,
/// named for when it was written; never over an earlier report.
pub fn write_report(
    parent: &Path,
    stamp: &str,
    markdown: &str,
    screenshot: Option<&[u8]>,
) -> Result<PathBuf, String> {
    if markdown.len() > MAX_REPORT {
        return Err("The report is too long to save".into());
    }
    if let Some(png) = screenshot
        && (png.len() > MAX_SCREENSHOT || !png.starts_with(b"\x89PNG\r\n\x1a\n"))
    {
        return Err("The screenshot is not a PNG image".into());
    }
    fs::create_dir_all(parent).map_err(|e| format!("Could not open {}: {e}", parent.display()))?;
    let base = format!("Lazybox feedback {stamp}");
    let folder = (1..100)
        .map(|n| {
            parent.join(if n == 1 {
                base.clone()
            } else {
                format!("{base} {n}")
            })
        })
        .find(|candidate| fs::create_dir(candidate).is_ok())
        .ok_or_else(|| format!("Could not create a folder in {}", parent.display()))?;
    fs::write(folder.join("feedback.md"), markdown)
        .map_err(|e| format!("Could not save the report: {e}"))?;
    if let Some(png) = screenshot {
        fs::write(folder.join("screenshot.png"), png)
            .map_err(|e| format!("Could not save the screenshot: {e}"))?;
    }
    Ok(folder)
}

/// Saves a report the person has read, in Downloads. Returns the folder.
#[tauri::command]
pub fn save_feedback(
    app: tauri::AppHandle,
    markdown: String,
    screenshot: Option<String>,
) -> Result<String, String> {
    let png = screenshot
        .map(|data| base64::engine::general_purpose::STANDARD.decode(data))
        .transpose()
        .map_err(|_| "The screenshot could not be read".to_string())?;
    let parent = app
        .path()
        .download_dir()
        .or_else(|_| app.path().home_dir())
        .map_err(|e| e.to_string())?;
    write_report(&parent, &stamp(), &markdown, png.as_deref()).map(|p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_keeps_whole_lines_from_the_end() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("runner.log");
        let text: String = (1..=500).map(|n| format!("line {n}\n")).collect();
        fs::write(&log, &text).expect("log");
        let last = tail(&log, 1 << 20, 3).expect("tail");
        assert_eq!(last, "line 498\nline 499\nline 500");
        // A byte limit inside a line drops that partial line.
        let bounded = tail(&log, 20, 10).expect("tail");
        assert!(bounded.lines().all(|l| l.starts_with("line ")));
        assert!(bounded.ends_with("line 500"));
        assert!(!bounded.starts_with("ne"));
        assert!(tail(&dir.path().join("missing.log"), 10, 10).is_err());
    }

    #[test]
    fn a_report_gets_a_folder_of_its_own() {
        let dir = tempfile::tempdir().expect("dir");
        let png = b"\x89PNG\r\n\x1a\nrest";
        let first =
            write_report(dir.path(), "2026-11-17 09.41.07", "# Report", Some(png)).expect("first");
        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("Lazybox feedback 2026-11-17 09.41.07")
        );
        assert_eq!(
            fs::read_to_string(first.join("feedback.md")).expect("md"),
            "# Report"
        );
        assert_eq!(fs::read(first.join("screenshot.png")).expect("png"), png);
        // The same second again: a new folder, never over the first.
        let second =
            write_report(dir.path(), "2026-11-17 09.41.07", "# Two", None).expect("second");
        assert!(second.ends_with("Lazybox feedback 2026-11-17 09.41.07 2"));
        assert!(!second.join("screenshot.png").exists());
        assert!(write_report(dir.path(), "x", "# Bad", Some(b"GIF89a")).is_err());
        assert!(write_report(dir.path(), "x", &"x".repeat(MAX_REPORT + 1), None).is_err());
    }

    #[test]
    fn utc_stamps_are_calendar_dates() {
        assert_eq!(utc_stamp(0), "1970-01-01 00.00.00");
        assert_eq!(utc_stamp(1_795_000_000), "2026-11-18 11.06.40");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00.00.00");
    }
}
