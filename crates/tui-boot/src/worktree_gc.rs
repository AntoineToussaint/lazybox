//! `lazybox worktree` — a standalone, daemon-free surface over the
//! worktree inspector (`crates/git-ops::inspect`), so the per-task
//! worktrees lazybox provisions can be *seen* and *reclaimed* from the
//! CLI before they fill the disk (issue #574).
//!
//!   lazybox worktree list            read-only report: every worktree
//!                                    with size, age, orphan reasons,
//!                                    and per-run totals (worktree disk
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
//! reclaim those deliberately in the TUI worktree inspector (Settings →
//! Inspect worktrees…) where the per-row force lives.
//!
//! Output goes to stdout because `init_tracing` redirects fd 2 into the
//! log file.

use std::collections::HashMap;
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
    let reap = reap_set(&inspections);
    let review = review_set(&inspections);
    let review_bytes: u64 = review.iter().map(|r| r.size_bytes).sum();

    println!(
        "\n{} in worktrees (bare clones under repos/ not counted).",
        format_size(total),
    );
    println!(
        "  {} auto-reclaimable across {} safe orphan{}",
        format_size(reclaim),
        reap.len(),
        if reap.len() == 1 { "" } else { "s" },
    );
    if !review.is_empty() {
        // The disk hogs usually land here: orphans with no backing bare
        // clone, uncommitted, unpushed, or locked. `gc` won't touch them
        // — they need a look before deletion, which is what the TUI
        // inspector's per-row force is for.
        println!(
            "  {} across {} orphan{} needing review (no bare clone / uncommitted / unpushed / locked)",
            format_size(review_bytes),
            review.len(),
            if review.len() == 1 { "" } else { "s" },
        );
    }
    if !reap.is_empty() {
        println!("\nRun `lazybox worktree gc` to reclaim the safe orphans.");
    }
    if !review.is_empty() {
        println!("Review the rest in the worktree inspector (Settings → Inspect worktrees…).");
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
             Quit lazybox first, or reclaim a row from the worktree inspector \
             (Settings → Inspect worktrees…)."
        );
        std::process::exit(2);
    }

    let mgr = WorktreeManager::default_base();
    let tracked = collect_tracked_sessions();
    let inspections = mgr.inspect_worktrees(&tracked).await?;
    let reap = reap_set(&inspections);
    let review = review_set(&inspections);

    // Whatever `gc` can't safely reap, name the disk it holds and where
    // to deal with it — so a big orphan is never silently ignored.
    let review_note = || {
        if !review.is_empty() {
            let bytes: u64 = review.iter().map(|r| r.size_bytes).sum();
            println!(
                "{} across {} orphan{} need review (no bare clone / uncommitted / unpushed / \
                 locked) — reclaim in the worktree inspector (Settings → Inspect worktrees…).",
                format_size(bytes),
                review.len(),
                if review.len() == 1 { "" } else { "s" },
            );
        }
    };

    if reap.is_empty() {
        if review.is_empty() {
            println!("Nothing to reclaim — no safe orphaned worktrees.");
        } else {
            print!("Nothing to auto-reclaim. ");
            review_note();
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
    review_note();

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

    // Re-check: the inspection walk can take a while on a large base dir,
    // and a lazybox launched in that window now holds live sessions the
    // first check couldn't see. Bail before touching anything on disk.
    if let ServerStatus::Running { pid } = lifecycle::status() {
        println!("lazybox started (pid {pid}) during inspection — aborting without deleting.");
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

/// Whether `gc` will actually reclaim a row: an orphan the inspector
/// deemed safe (no uncommitted / unpushed work, not locked) AND one whose
/// non-force delete will succeed. `delete_inspected(force=false)` refuses
/// an orphan with no backing bare clone while it still holds files — it
/// can't verify the content is disposable — so those are left for the TUI
/// inspector's per-row force (they carry near-zero disk anyway). Keeping
/// this the single source of truth means the "safe-reclaim" tag, the
/// reclaimable total, and the delete loop can never disagree.
fn is_reclaimable(row: &WorktreeInspection) -> bool {
    row.is_orphaned() && row.is_safe_to_delete && row.bare_path.is_some()
}

/// The reclaim set: every row `gc` will reclaim. See [`is_reclaimable`].
fn reap_set(inspections: &[WorktreeInspection]) -> Vec<&WorktreeInspection> {
    inspections.iter().filter(|r| is_reclaimable(r)).collect()
}

/// Orphaned worktrees `gc` will NOT auto-reclaim — uncommitted / unpushed
/// / locked, or lacking a backing bare clone to verify against. Surfaced
/// so the disk they hold stays visible; reclaim them deliberately in the
/// TUI inspector's per-row force.
fn review_set(inspections: &[WorktreeInspection]) -> Vec<&WorktreeInspection> {
    inspections
        .iter()
        .filter(|r| r.is_orphaned() && !is_reclaimable(r))
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
    if is_reclaimable(row) {
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

    // Aggregate per worktree path: several sessions (an agent + a shell)
    // routinely share one worktree. A path counts as stopped only when
    // EVERY session on it is stopped — if any is still live-ish, the
    // worktree must not be classed as an ended-session orphan, so a naive
    // last/first-wins pick would be wrong. The inspector keys tracked
    // sessions by path, so emitting one row per path is also what it
    // expects. `index` preserves first-seen order + session id.
    let mut out: Vec<TrackedSession> = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    for record in records {
        let Some(json) = record.workspace_json else {
            continue;
        };
        let Ok(workspace) = serde_json::from_str::<lazybox_core::Workspace>(&json) else {
            continue;
        };
        for session in workspace.sessions {
            let is_stopped = matches!(session.state, lazybox_core::SessionRunState::Stopped);
            match index.get(&session.worktree_path) {
                Some(&i) => out[i].is_stopped &= is_stopped,
                None => {
                    let raw = session.id.to_string();
                    let session_id = raw.get(..8).unwrap_or(&raw).to_string();
                    index.insert(session.worktree_path.clone(), out.len());
                    out.push(TrackedSession {
                        session_id,
                        worktree_path: session.worktree_path,
                        is_stopped,
                    });
                }
            }
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
    fn no_bare_clone_orphan_is_not_auto_reclaimable() {
        // is_safe_to_delete=true but no backing bare clone:
        // delete_inspected(force=false) refuses these when they hold
        // content, so gc must not count them reclaimable, must not tag
        // them "safe-reclaim", yet must still count them on the total.
        let mut row = inspection("ghost", 500, vec![OrphanReason::Untracked], true);
        row.bare_path = None;
        let rows = std::slice::from_ref(&row);
        assert!(!is_reclaimable(&row));
        assert!(reap_set(rows).is_empty());
        assert_eq!(reclaimable_bytes(rows), 0);
        assert_eq!(total_bytes(rows), 500);
        assert!(!format_row(&row).contains("safe-reclaim"));
    }

    #[test]
    fn review_set_is_the_orphans_gc_wont_reap() {
        let mut ghost = inspection("ghost", 1000, vec![OrphanReason::Untracked], true);
        ghost.bare_path = None; // no bare clone → needs review, not auto-reap
        let mut dirty = inspection("dirty", 200, vec![OrphanReason::SessionStopped], false);
        dirty.has_uncommitted_changes = true;
        let safe = inspection("gone", 300, vec![OrphanReason::BranchDeletedUpstream], true);
        let healthy = inspection("live", 100, vec![], false); // not orphaned
        let rows = vec![ghost, dirty, safe, healthy];

        let review: Vec<_> = review_set(&rows)
            .iter()
            .map(|r| r.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        // The unreclaimable orphans, in input order; the reap-safe orphan
        // (`gone`) and the healthy worktree (`live`) are excluded.
        assert_eq!(review, vec!["ghost", "dirty"]);
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
