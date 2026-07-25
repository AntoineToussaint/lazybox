//! `lazybox worktree` — a standalone, daemon-free surface over the
//! worktree inspector (`crates/git-ops::inspect`), so the per-task
//! worktrees lazybox provisions can be *seen* and *reclaimed* from the
//! CLI before they fill the disk (issue #574).
//!
//!   lazybox worktree list            read-only report: every worktree
//!                                    with size, age, orphan reasons,
//!                                    and per-run totals (total on disk
//!                                    + how much is safely reclaimable)
//!   lazybox worktree gc [--force]    reclaim the safe orphaned worktrees
//!         [--dry-run]                (merged/closed upstream, stopped or
//!                                    untracked session) — confirms first
//!                                    unless `--force`; `--dry-run` only
//!                                    reports
//!
//! Both reuse the exact inspection + safety gates the in-TUI worktree
//! inspector uses (`WorktreeManager::{inspect_worktrees,delete_inspected}`):
//! a worktree is only ever reclaimed when it is flagged orphaned AND has
//! no uncommitted changes, no unpushed commits, and isn't locked. Dirty /
//! unpushed / locked orphans are surfaced but never touched from here —
//! reclaim those deliberately in the TUI (`Shift-D` inspector) where the
//! per-row force lives.
//!
//! Output goes to stdout because `init_tracing` redirects fd 2 into the
//! log file.

use std::collections::HashSet;
use std::path::PathBuf;

use lazybox_git_ops::{TrackedSession, WorktreeInspection, WorktreeManager};
use lazybox_server::lifecycle::{self, ServerStatus};

pub async fn worktree_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => list().await,
        Some("gc") => gc(&args[1..]).await,
        _ => {
            println!(
                "usage: lazybox worktree [list | gc [--force] [--dry-run]]\n\n\
                 list  read-only report of every managed worktree (size, age, orphan reasons)\n\
                 gc    reclaim the safe orphaned worktrees; confirms first unless --force"
            );
            std::process::exit(2);
        }
    }
}

/// `lazybox worktree list` — read-only inventory of every managed
/// worktree with sizes and orphan reasons, plus the two totals that
/// make the leak visible: bytes on disk and bytes safely reclaimable.
async fn list() -> anyhow::Result<()> {
    let mgr = WorktreeManager::default_base();
    let tracked = collect_tracked_sessions();
    let inspections = mgr.inspect_worktrees(&tracked).await?;

    let root = lazybox_core::paths::worktrees_root();
    if inspections.is_empty() {
        println!("No managed worktrees under {}.", root.display());
        return Ok(());
    }

    println!(
        "{} managed worktree{} under {}:\n",
        inspections.len(),
        if inspections.len() == 1 { "" } else { "s" },
        root.display(),
    );
    for row in &inspections {
        println!("  {}", format_row(row));
    }

    let total = total_bytes(&inspections);
    let reclaim = reclaimable_bytes(&inspections);
    let reclaim_n = reap_set(&inspections).len();
    println!(
        "\n{} on disk · {} reclaimable across {} safe orphan{}",
        format_size(total),
        format_size(reclaim),
        reclaim_n,
        if reclaim_n == 1 { "" } else { "s" },
    );
    if reclaim_n > 0 {
        println!("Run `lazybox worktree gc` to reclaim them.");
    }
    Ok(())
}

/// `lazybox worktree gc` — reclaim the safe orphaned worktrees.
///
/// Refuses while a daemon (standalone or the embedded one behind a live
/// TUI) is running: a standalone reap can't see the daemon's in-memory
/// live-terminal map, so it could pull a worktree out from under an
/// attached agent. `list` stays available; deletion waits until lazybox
/// is closed (or reclaim per-row in the TUI inspector).
async fn gc(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let force = crate::take_flag(&mut args, "--force");
    let dry_run = crate::take_flag(&mut args, "--dry-run");

    if let ServerStatus::Running { pid } = lifecycle::status() {
        println!(
            "lazybox is running (pid {pid}) — refusing to reclaim worktrees while it may hold \
             live agent/shell sessions.\n\
             Quit lazybox first, or reclaim a row from the TUI inspector (`Shift-D`)."
        );
        std::process::exit(2);
    }

    let mgr = WorktreeManager::default_base();
    let tracked = collect_tracked_sessions();
    let inspections = mgr.inspect_worktrees(&tracked).await?;
    let reap = reap_set(&inspections);

    if reap.is_empty() {
        let unsafe_orphans = inspections
            .iter()
            .filter(|r| r.is_orphaned() && !r.is_safe_to_delete)
            .count();
        if unsafe_orphans > 0 {
            println!(
                "Nothing to reclaim. {unsafe_orphans} orphaned worktree(s) hold uncommitted / \
                 unpushed / locked work — reclaim those deliberately in the TUI inspector (`Shift-D`)."
            );
        } else {
            println!("Nothing to reclaim — no safe orphaned worktrees.");
        }
        return Ok(());
    }

    let reclaim = reclaimable_bytes(&inspections);
    println!(
        "{} safe orphaned worktree{} · {} reclaimable:\n",
        reap.len(),
        if reap.len() == 1 { "" } else { "s" },
        format_size(reclaim),
    );
    for row in &reap {
        println!("  {}", format_row(row));
    }
    println!();

    if dry_run {
        println!("--dry-run: nothing deleted.");
        return Ok(());
    }

    if !force
        && !confirm(&format!(
            "Delete {} worktree{} and reclaim {}? [y/N] ",
            reap.len(),
            if reap.len() == 1 { "" } else { "s" },
            format_size(reclaim),
        ))
    {
        println!("Aborted.");
        return Ok(());
    }

    let mut removed = 0usize;
    let mut freed = 0u64;
    for row in &reap {
        // force=false: the reap set already passed the safety gate, and
        // we never want to bypass the uncommitted/unpushed/locked guard.
        match mgr.delete_inspected(row, false).await {
            Ok(()) => {
                removed += 1;
                freed += row.size_bytes;
            }
            Err(e) => println!("  ! {}: {e}", row.path.display()),
        }
    }
    println!(
        "\nReclaimed {removed}/{} worktree{} · {} freed.",
        reap.len(),
        if reap.len() == 1 { "" } else { "s" },
        format_size(freed),
    );
    Ok(())
}

/// The reclaim set: orphaned worktrees the inspector deems safe to
/// delete (no uncommitted / unpushed work, not locked). Exactly the
/// rows the in-TUI "delete safe" bulk action targets.
fn reap_set(inspections: &[WorktreeInspection]) -> Vec<&WorktreeInspection> {
    inspections
        .iter()
        .filter(|r| r.is_orphaned() && r.is_safe_to_delete)
        .collect()
}

/// Total bytes across every inspected worktree — "how much worktree disk
/// lazybox is holding right now".
fn total_bytes(inspections: &[WorktreeInspection]) -> u64 {
    inspections.iter().map(|r| r.size_bytes).sum()
}

/// Bytes the GC would free — the size of the safe reclaim set.
fn reclaimable_bytes(inspections: &[WorktreeInspection]) -> u64 {
    reap_set(inspections).iter().map(|r| r.size_bytes).sum()
}

/// One inspector row as a single line, mirroring the TUI inspector's
/// `[reasons] name · branch · size · flags` shape.
fn format_row(row: &WorktreeInspection) -> String {
    let name = row
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| row.path.display().to_string());
    let reasons = if row.reasons.is_empty() {
        "healthy".to_string()
    } else {
        row.reasons
            .iter()
            .map(|r| r.tag())
            .collect::<Vec<_>>()
            .join(",")
    };
    let branch = row.branch.as_deref().unwrap_or("(detached)");
    let mut flags = Vec::<&str>::new();
    if row.has_uncommitted_changes {
        flags.push("DIRTY");
    }
    if row.has_unpushed_commits {
        flags.push("UNPUSHED");
    }
    if row.is_orphaned() && row.is_safe_to_delete {
        flags.push("safe-reclaim");
    }
    let flag_str = if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(","))
    };
    format!(
        "[{reasons}] {name} · {branch} · {}{flag_str}",
        format_size(row.size_bytes),
    )
}

/// Prompt on stdout, read one line from stdin, and treat only an
/// explicit `y` / `yes` (case-insensitive) as consent — anything else,
/// including EOF (a piped/no-tty invocation), is "no".
fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => confirmed(&line),
    }
}

/// Pure consent test, split out so it can be unit-tested without stdin.
fn confirmed(input: &str) -> bool {
    matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Project every persisted session into the inspector's
/// [`TrackedSession`] shape — the daemon-free twin of the server's
/// `collect_tracked_sessions`. A session in `SessionRunState::Stopped`
/// marks its worktree as an orphan candidate. Reads the production DB
/// best-effort: a missing / unreadable store yields an empty list, so
/// every on-disk worktree is then treated as untracked (still guarded
/// by the safety gate before any deletion).
fn collect_tracked_sessions() -> Vec<TrackedSession> {
    use lazybox_store::Store;

    let db_path = lazybox_core::paths::state_db();
    if !db_path.exists() {
        return Vec::new();
    }
    let Ok(store) = lazybox_store::SqliteStore::open(&db_path) else {
        return Vec::new();
    };
    let Ok(records) = store.list_workspaces() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<lazybox_core::Workspace>(&json) else {
            continue;
        };
        for session in workspace.sessions {
            if !seen.insert(session.worktree_path.clone()) {
                continue;
            }
            let is_stopped = matches!(session.state, lazybox_core::SessionRunState::Stopped);
            let raw = session.id.to_string();
            let session_id = raw.get(..8).unwrap_or(&raw).to_string();
            out.push(TrackedSession {
                session_id,
                worktree_path: session.worktree_path,
                is_stopped,
            });
        }
    }
    out
}

/// Human-readable byte size, matching the TUI inspector's `format_size`
/// (`crates/tui/src/realm/model/modals.rs`) so the CLI and the modal
/// agree on units.
fn format_size(n: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_git_ops::OrphanReason;
    use std::path::PathBuf;

    fn inspection(
        name: &str,
        size: u64,
        reasons: Vec<OrphanReason>,
        safe: bool,
    ) -> WorktreeInspection {
        WorktreeInspection {
            path: PathBuf::from(format!("/wt/{name}")),
            bare_path: Some(PathBuf::from("/repos/acme/widget.git")),
            branch: Some(format!("lazybox/{name}")),
            session_id: None,
            reasons,
            size_bytes: size,
            last_modified: None,
            has_uncommitted_changes: false,
            has_unpushed_commits: false,
            is_safe_to_delete: safe,
        }
    }

    #[test]
    fn reap_set_is_orphaned_and_safe_only() {
        let rows = vec![
            // Healthy worktree — never reaped.
            inspection("healthy", 100, vec![], false),
            // Orphaned but unsafe (dirty/unpushed/locked) — not reaped.
            inspection(
                "dirty",
                200,
                vec![OrphanReason::BranchDeletedUpstream],
                false,
            ),
            // Orphaned + safe — reaped.
            inspection("gone", 300, vec![OrphanReason::BranchDeletedUpstream], true),
            inspection("stopped", 400, vec![OrphanReason::SessionStopped], true),
        ];
        let reap = reap_set(&rows);
        let names: Vec<_> = reap
            .iter()
            .map(|r| r.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["gone", "stopped"]);
    }

    #[test]
    fn totals_split_disk_from_reclaimable() {
        let rows = vec![
            inspection("healthy", 100, vec![], false),
            inspection("dirty", 200, vec![OrphanReason::SessionStopped], false),
            inspection("gone", 300, vec![OrphanReason::BranchDeletedUpstream], true),
            inspection("stopped", 400, vec![OrphanReason::SessionStopped], true),
        ];
        // Everything on disk.
        assert_eq!(total_bytes(&rows), 1000);
        // Only the safe orphans.
        assert_eq!(reclaimable_bytes(&rows), 700);
    }

    #[test]
    fn safe_but_not_orphaned_is_never_reclaimable() {
        // is_safe_to_delete=true with no orphan reason must not be
        // counted — a live, healthy worktree is "safe" in the trivial
        // sense but there's no reason to reap it.
        let rows = vec![inspection("live", 500, vec![], true)];
        assert!(reap_set(&rows).is_empty());
        assert_eq!(reclaimable_bytes(&rows), 0);
        assert_eq!(total_bytes(&rows), 500);
    }

    #[test]
    fn format_row_tags_a_safe_orphan() {
        let row = inspection(
            "gone",
            2 * 1024 * 1024,
            vec![OrphanReason::BranchDeletedUpstream],
            true,
        );
        let line = format_row(&row);
        assert!(line.contains("branch-deleted-upstream"), "{line}");
        assert!(line.contains("safe-reclaim"), "{line}");
        assert!(line.contains("2.0M"), "{line}");
    }

    #[test]
    fn format_row_labels_a_healthy_worktree() {
        let row = inspection("live", 100, vec![], false);
        let line = format_row(&row);
        assert!(line.contains("healthy"), "{line}");
        assert!(!line.contains("safe-reclaim"), "{line}");
    }

    #[test]
    fn confirmed_only_accepts_yes() {
        assert!(confirmed("y"));
        assert!(confirmed("Y"));
        assert!(confirmed("yes"));
        assert!(confirmed("  YES \n"));
        assert!(!confirmed("n"));
        assert!(!confirmed(""));
        assert!(!confirmed("no"));
        assert!(!confirmed("yeah"));
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(1024), "1.0K");
        assert_eq!(format_size(1024 * 1024), "1.0M");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0G");
    }
}
