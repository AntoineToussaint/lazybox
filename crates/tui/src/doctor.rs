//! `pilot doctor worktrees` — inspect and clean up orphaned worktrees.
//!
//! Scans every directory under `<state_root>/worktrees/` plus every
//! bare-clone metadata table under `<state_root>/repos/`, classifies
//! each row (untracked / stopped session / branch deleted upstream /
//! prunable / locked / broken), and surfaces uncommitted +
//! unpushed-commit state so the user knows what's safe to drop.
//!
//! Usage shapes:
//! ```text
//! pilot doctor worktrees                         # interactive (per-row prompt)
//! pilot doctor worktrees --json                  # machine-readable, read-only
//! pilot doctor worktrees --delete-safe           # delete every clearly-safe row, skip the rest
//! pilot doctor worktrees --yes                   # skip prompts, delete everything flagged
//!                                                # (still refuses uncommitted / unpushed)
//! pilot doctor worktrees --yes --force           # additionally override safety
//! ```
//!
//! The destructive paths use `WorktreeManager::delete_inspected`,
//! which goes through `git worktree remove` + `git worktree prune` —
//! never raw `rm -rf` — so git's metadata stays consistent.

use pilot_core::SessionId;
use pilot_git_ops::{OrphanReason, TrackedSession, WorktreeInspection, WorktreeManager};
use std::io::{BufRead, Write};
use std::time::SystemTime;

/// Parsed `pilot doctor worktrees` argv. Centralised so the CLI
/// dispatch in `main.rs` is small.
#[derive(Debug, Default, Clone)]
pub struct DoctorOptions {
    /// Emit a JSON array of inspections and exit. Implies read-only.
    pub json: bool,
    /// Delete every entry where `is_safe_to_delete` is true, skip
    /// the rest. Non-interactive.
    pub delete_safe: bool,
    /// Skip the per-row prompt — delete every orphaned entry the
    /// inspector finds. Still respects the safety classifier unless
    /// `force` is also set.
    pub yes: bool,
    /// Override the safety classifier — delete even entries with
    /// uncommitted changes / unpushed commits / locked metadata.
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorOutcome {
    /// Inspection (and any actions) completed cleanly.
    Ok,
    /// At least one delete failed. CLI maps to exit code 1.
    PartialFailure,
    /// User asked for help / passed an unknown arg.
    BadArgs,
}

/// CLI entry point. Reads the store to attach session ids to on-disk
/// worktrees, runs the inspector, then either prints (`--json`,
/// non-interactive) or prompts (interactive).
pub async fn run(args: &[String]) -> anyhow::Result<DoctorOutcome> {
    let opts = match parse_args(args) {
        ParsedArgs::Opts(o) => o,
        ParsedArgs::Help => {
            print_usage();
            return Ok(DoctorOutcome::Ok);
        }
        ParsedArgs::Unknown(flag) => {
            println!("doctor: unknown flag {flag:?}");
            print_usage();
            return Ok(DoctorOutcome::BadArgs);
        }
    };

    let mgr = WorktreeManager::default_base();
    let tracked = collect_tracked_sessions();
    let inspections = mgr.inspect_worktrees(&tracked).await?;

    if opts.json {
        print_json(&inspections);
        return Ok(DoctorOutcome::Ok);
    }

    print_summary(&inspections);

    // Read-only when no action flag is passed AND stdin isn't a TTY.
    // Interactive mode reads from stdin; piped invocations (`pilot
    // doctor worktrees | jq` style) get the summary and exit clean.
    if !opts.delete_safe && !opts.yes && !is_stdin_tty() {
        return Ok(DoctorOutcome::Ok);
    }

    let mut failures: usize = 0;
    for row in &inspections {
        if !row.is_orphaned() {
            continue;
        }
        let action = decide_action(row, &opts);
        match action {
            Action::Skip(reason) => {
                println!("· skip  {}  ({reason})", row.path.display());
            }
            Action::Delete { force } => {
                match mgr.delete_inspected(row, force).await {
                    Ok(()) => println!("✓ delete {}", row.path.display()),
                    Err(e) => {
                        eprintln!("✗ delete {}: {e}", row.path.display());
                        failures += 1;
                    }
                }
            }
        }
    }

    if failures > 0 {
        Ok(DoctorOutcome::PartialFailure)
    } else {
        Ok(DoctorOutcome::Ok)
    }
}

#[derive(Debug)]
enum Action {
    Skip(&'static str),
    Delete { force: bool },
}

fn decide_action(row: &WorktreeInspection, opts: &DoctorOptions) -> Action {
    if opts.delete_safe {
        if row.is_safe_to_delete {
            return Action::Delete { force: false };
        }
        if row.has_uncommitted_changes {
            return Action::Skip("has uncommitted changes");
        }
        if row.has_unpushed_commits {
            return Action::Skip("has unpushed commits");
        }
        if row.reasons.contains(&OrphanReason::Locked) {
            return Action::Skip("locked");
        }
        return Action::Skip("not flagged as safe");
    }
    if opts.yes {
        if opts.force {
            return Action::Delete { force: true };
        }
        if row.has_uncommitted_changes {
            return Action::Skip("uncommitted changes (pass --force to override)");
        }
        if row.has_unpushed_commits {
            return Action::Skip("unpushed commits (pass --force to override)");
        }
        if row.reasons.contains(&OrphanReason::Locked) {
            return Action::Skip("locked (pass --force to override)");
        }
        return Action::Delete { force: false };
    }

    // Interactive prompt.
    print_row(row);
    let hint = if row.has_uncommitted_changes || row.has_unpushed_commits {
        "[delete (FORCE) / skip / keep] (default: keep)"
    } else {
        "[delete / skip / keep] (default: skip)"
    };
    print!("  action {hint}: ");
    std::io::stdout().flush().ok();
    let answer = read_line().to_lowercase();
    let answer = answer.trim();
    match (answer, row.has_uncommitted_changes || row.has_unpushed_commits) {
        ("d" | "delete", true) => Action::Delete { force: true },
        ("d" | "delete", false) => Action::Delete { force: false },
        ("s" | "skip", _) => Action::Skip("user skipped"),
        ("k" | "keep" | "", _) => Action::Skip("user kept"),
        _ => Action::Skip("unrecognized answer → kept"),
    }
}

/// Walk every persisted workspace, pull each session out as a
/// `TrackedSession` with `is_stopped` derived from the session's
/// run state. Used to attach session ids to on-disk worktrees and
/// to flag closed sessions whose worktree never got reaped.
fn collect_tracked_sessions() -> Vec<TrackedSession> {
    let Some(store) = pilot_server::open_store() else {
        return Vec::new();
    };
    let records = match store.list_workspaces() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("doctor: list_workspaces: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::with_capacity(records.len() * 2);
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<pilot_core::Workspace>(&json) else {
            continue;
        };
        for session in workspace.sessions {
            let is_stopped =
                matches!(session.state, pilot_core::SessionRunState::Stopped);
            out.push(TrackedSession {
                session_id: short_session_id(&session.id),
                worktree_path: session.worktree_path,
                is_stopped,
            });
        }
    }
    out
}

fn short_session_id(id: &SessionId) -> String {
    let s = id.to_string();
    s.get(..8).unwrap_or(&s).to_string()
}

fn print_summary(inspections: &[WorktreeInspection]) {
    let total = inspections.len();
    let orphans = inspections.iter().filter(|r| r.is_orphaned()).count();
    let safe = inspections
        .iter()
        .filter(|r| r.is_orphaned() && r.is_safe_to_delete)
        .count();
    let with_local_work = inspections
        .iter()
        .filter(|r| r.has_uncommitted_changes || r.has_unpushed_commits)
        .count();
    println!(
        "worktrees: {total} total, {orphans} flagged ({safe} clearly safe, {with_local_work} with local work)\n"
    );
    for row in inspections {
        print_row(row);
    }
    println!();
}

fn print_row(row: &WorktreeInspection) {
    let tags = if row.reasons.is_empty() {
        "ok".to_string()
    } else {
        row.reasons
            .iter()
            .map(|r| r.tag())
            .collect::<Vec<_>>()
            .join(",")
    };
    let session = row.session_id.as_deref().unwrap_or("-");
    let branch = row.branch.as_deref().unwrap_or("-");
    let size = format_bytes(row.size_bytes);
    let mtime = row
        .last_modified
        .as_ref()
        .map(format_age)
        .unwrap_or_else(|| "-".into());
    let mut flags = Vec::<&str>::new();
    if row.has_uncommitted_changes {
        flags.push("DIRTY");
    }
    if row.has_unpushed_commits {
        flags.push("UNPUSHED");
    }
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!("  [{}]", flags.join(","))
    };
    println!(
        "  {path}\n    branch={branch}  session={session}  size={size}  mtime={mtime}  status={tags}{flag_str}",
        path = row.path.display(),
    );
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if n >= GB {
        format!("{:.1}G", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1}M", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1}K", n as f64 / KB as f64)
    } else {
        format!("{n}B")
    }
}

fn format_age(t: &SystemTime) -> String {
    let now = SystemTime::now();
    let secs = now
        .duration_since(*t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn print_json(inspections: &[WorktreeInspection]) {
    // Hand-roll the JSON to avoid a serde derive on the inspector
    // types — keeps `pilot-git-ops` from growing a serde dep just for
    // CLI output.
    let mut out = String::from("[\n");
    for (i, row) in inspections.iter().enumerate() {
        let reasons: Vec<String> = row
            .reasons
            .iter()
            .map(|r| format!("{:?}", json_string(r.tag())))
            .collect();
        out.push_str(&format!(
            "  {{\n    \"path\": {},\n    \"bare_path\": {},\n    \"branch\": {},\n    \"session_id\": {},\n    \"reasons\": [{}],\n    \"size_bytes\": {},\n    \"last_modified_unix\": {},\n    \"has_uncommitted_changes\": {},\n    \"has_unpushed_commits\": {},\n    \"is_safe_to_delete\": {}\n  }}",
            json_string(&row.path.to_string_lossy()),
            row.bare_path
                .as_ref()
                .map(|p| json_string(&p.to_string_lossy()))
                .unwrap_or_else(|| "null".into()),
            row.branch
                .as_ref()
                .map(|b| json_string(b))
                .unwrap_or_else(|| "null".into()),
            row.session_id
                .as_ref()
                .map(|s| json_string(s))
                .unwrap_or_else(|| "null".into()),
            reasons.join(", "),
            row.size_bytes,
            row.last_modified
                .as_ref()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|| "null".into()),
            row.has_uncommitted_changes,
            row.has_unpushed_commits,
            row.is_safe_to_delete,
        ));
        if i + 1 != inspections.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("]\n");
    println!("{out}");
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn read_line() -> String {
    let stdin = std::io::stdin();
    let mut buf = String::new();
    stdin.lock().read_line(&mut buf).ok();
    buf
}

fn is_stdin_tty() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        unsafe { libc_isatty(std::io::stdin().as_raw_fd()) }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
unsafe fn libc_isatty(fd: i32) -> bool {
    // We don't pull in `libc` for one symbol — link by name and
    // accept the small portability ding (this binary is Unix-first).
    unsafe extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    unsafe { isatty(fd) == 1 }
}

#[derive(Debug)]
enum ParsedArgs {
    Opts(DoctorOptions),
    Help,
    Unknown(String),
}

fn parse_args(args: &[String]) -> ParsedArgs {
    let mut opts = DoctorOptions::default();
    for a in args {
        match a.as_str() {
            "--json" => opts.json = true,
            "--delete-safe" => opts.delete_safe = true,
            "--yes" | "-y" => opts.yes = true,
            "--force" => opts.force = true,
            "-h" | "--help" => return ParsedArgs::Help,
            other => return ParsedArgs::Unknown(other.to_string()),
        }
    }
    // `--delete-safe` and `--yes` are mutually exclusive: yes
    // implies "delete everything orphaned"; delete-safe filters to
    // the safe subset. If both are passed we honor delete-safe (the
    // narrower of the two) so accidental combinations don't
    // surprise-delete dirty worktrees.
    if opts.delete_safe {
        opts.yes = false;
    }
    ParsedArgs::Opts(opts)
}

fn print_usage() {
    println!(
        "usage: pilot doctor worktrees [--json|--delete-safe|--yes [--force]]\n\
         \n\
         Inspect every worktree under <PILOT_HOME>/v2/worktrees/. Flag\n\
         orphaned ones (untracked, branch deleted upstream, prunable,\n\
         locked, etc.) and optionally clean them up.\n\
         \n\
         Flags:\n\
           --json          Emit JSON and exit (read-only).\n\
           --delete-safe   Delete every entry classified as clearly safe\n\
                           (no uncommitted changes, no unpushed commits,\n\
                            not locked). Skip everything else.\n\
           --yes, -y       Delete every flagged entry. Still refuses\n\
                           dirty / unpushed / locked unless --force.\n\
           --force         Override safety checks (must be paired with\n\
                           --yes; ignored with --delete-safe).\n\
           -h, --help      Show this help.\n\
         \n\
         With no flags, prompts per row interactively.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> ParsedArgs {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn no_flags_yields_defaults() {
        match parse(&[]) {
            ParsedArgs::Opts(o) => {
                assert!(!o.json && !o.delete_safe && !o.yes && !o.force);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn json_flag_recognized() {
        match parse(&["--json"]) {
            ParsedArgs::Opts(o) => assert!(o.json),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn delete_safe_clears_yes_when_both_passed() {
        // Defensive precedence: --delete-safe is the narrower of the
        // two, so passing both is treated as "delete safe only".
        match parse(&["--delete-safe", "--yes"]) {
            ParsedArgs::Opts(o) => {
                assert!(o.delete_safe);
                assert!(!o.yes, "delete-safe wins over yes");
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn help_short_and_long_match() {
        assert!(matches!(parse(&["-h"]), ParsedArgs::Help));
        assert!(matches!(parse(&["--help"]), ParsedArgs::Help));
    }

    #[test]
    fn unknown_flag_returned_verbatim() {
        match parse(&["--bogus"]) {
            ParsedArgs::Unknown(s) => assert_eq!(s, "--bogus"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn bytes_formatter_picks_units() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(2048), "2.0K");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0M");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0G");
    }

    #[test]
    fn json_string_escapes_specials() {
        assert_eq!(json_string("hi"), "\"hi\"");
        assert_eq!(json_string("a\"b"), "\"a\\\"b\"");
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("\\"), "\"\\\\\"");
        // Below-0x20 control char gets unicode-escaped.
        assert_eq!(json_string("\x01"), "\"\\u0001\"");
    }
}

