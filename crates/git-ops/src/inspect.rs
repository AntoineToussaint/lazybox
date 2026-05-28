//! Worktree inspector — scans the worktrees directory + every bare
//! clone, reports each worktree's status (orphan reasons, size, last
//! modified, uncommitted/unpushed work), and exposes a safety-aware
//! delete helper.
//!
//! Decoupling: the inspector takes session metadata as plain
//! [`TrackedSession`] input rather than reaching into `pilot-store` /
//! `pilot-core`. That keeps `pilot-git-ops` source-agnostic — the CLI
//! / TUI translates store records into `TrackedSession` and calls in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::process::Command;

use crate::{GitError, WorktreeManager, apply_git_env};

// `base_dir` is reachable via a crate-private accessor on
// `WorktreeManager`; see `crates/git-ops/src/lib.rs`. Inspector code
// composes paths off that accessor instead of duplicating layout
// knowledge.

/// Caller-supplied projection of a session record. The inspector
/// looks each on-disk worktree path up in this list to attach a
/// session id and flag sessions whose state means the directory is
/// safe to reap.
#[derive(Debug, Clone)]
pub struct TrackedSession {
    pub session_id: String,
    pub worktree_path: PathBuf,
    /// `true` when the session's process is no longer running (e.g.
    /// `SessionRunState::Stopped`). Surfaces the "tracked session has
    /// been ended/closed but its worktree directory wasn't removed"
    /// orphan category from the cleanup spec.
    pub is_stopped: bool,
}

/// Why an inspector entry is flagged as orphaned. A worktree can be
/// tagged with multiple reasons; the report deduplicates them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanReason {
    /// On-disk worktree directory has no matching session record.
    Untracked,
    /// Pilot tracks a session pointing at this directory, but the
    /// session's process exited / was closed.
    SessionStopped,
    /// Worktree's checked-out branch is missing from the bare repo's
    /// remote-tracking refs (PR merged + branch auto-deleted, manual
    /// force-delete upstream, etc.).
    BranchDeletedUpstream,
    /// Worktree's checked-out branch is missing from the bare repo's
    /// local refs.
    BranchMissingLocally,
    /// `git worktree list --porcelain` reports the entry as prunable
    /// (gitdir exists in the bare clone's index but the directory on
    /// disk is gone / unreadable).
    Prunable,
    /// `git worktree list --porcelain` reports the entry as locked —
    /// the user explicitly froze it; we surface but never auto-delete.
    Locked,
}

impl OrphanReason {
    /// Short, user-facing tag for tabular output.
    pub fn tag(&self) -> &'static str {
        match self {
            OrphanReason::Untracked => "untracked",
            OrphanReason::SessionStopped => "session-stopped",
            OrphanReason::BranchDeletedUpstream => "branch-deleted-upstream",
            OrphanReason::BranchMissingLocally => "branch-missing-locally",
            OrphanReason::Prunable => "prunable",
            OrphanReason::Locked => "locked",
        }
    }
}

/// One row of the inspector report.
#[derive(Debug, Clone)]
pub struct WorktreeInspection {
    /// Absolute path to the worktree directory.
    pub path: PathBuf,
    /// Bare-clone path the worktree belongs to. `None` when no bare
    /// clone in `base/repos/**/*.git` lists this directory — happens
    /// for orphaned dirs left behind after the bare clone was deleted
    /// manually, or sandbox-style scratch dirs the inspector still
    /// surfaces for completeness.
    pub bare_path: Option<PathBuf>,
    /// Branch reported by `git worktree list --porcelain`. `None`
    /// when the HEAD is detached or git couldn't tell us (rare for
    /// pilot-created worktrees, common for hand-managed ones).
    pub branch: Option<String>,
    /// Session id from the tracked list, if any. None means the dir
    /// is untracked.
    pub session_id: Option<String>,
    /// Reasons this entry is flagged. Empty Vec ⇒ healthy worktree.
    pub reasons: Vec<OrphanReason>,
    /// Total bytes on disk (recursive). Best-effort — unreadable
    /// entries contribute 0.
    pub size_bytes: u64,
    /// Most-recent mtime of any file in the worktree, or the dir
    /// itself when the walk found nothing. `None` for vanished dirs
    /// (prunable entries).
    pub last_modified: Option<SystemTime>,
    /// `git status --porcelain` reported at least one entry.
    pub has_uncommitted_changes: bool,
    /// HEAD is ahead of its upstream remote-tracking branch.
    pub has_unpushed_commits: bool,
    /// Convenience: no uncommitted changes AND no unpushed commits AND
    /// not locked. Bulk "delete safe" actions key off this.
    pub is_safe_to_delete: bool,
}

impl WorktreeInspection {
    pub fn is_orphaned(&self) -> bool {
        !self.reasons.is_empty()
    }
}

/// Internal: one row from `git worktree list --porcelain`. We only
/// parse the fields that drive orphan detection.
#[derive(Debug, Default, Clone)]
struct PorcelainEntry {
    path: PathBuf,
    branch: Option<String>,
    locked: bool,
    prunable: bool,
    /// `bare` keyword — the entry IS the bare clone itself, not a
    /// worktree we should report. Skipped after parsing.
    is_bare: bool,
}

impl WorktreeManager {
    /// Scan the worktrees directory + every bare clone under
    /// `base/repos/**/*.git` and report each worktree's health.
    ///
    /// `tracked` is the set of sessions pilot currently knows about
    /// — typically derived from `Store::list_workspaces`. Worktrees
    /// whose on-disk path doesn't appear in this list are tagged
    /// [`OrphanReason::Untracked`]; tracked sessions whose process
    /// is no longer running are tagged [`OrphanReason::SessionStopped`].
    ///
    /// The function is read-only — no `git worktree` mutations, no
    /// file deletes. Pair with [`WorktreeManager::delete_inspected`]
    /// to act on the report.
    pub async fn inspect_worktrees(
        &self,
        tracked: &[TrackedSession],
    ) -> Result<Vec<WorktreeInspection>, GitError> {
        let wt_root = self.base_dir().join("worktrees");

        // 1. Build the porcelain index across every bare clone — one
        //    entry per registered worktree. Map keyed by canonicalized
        //    on-disk path; values carry the bare-clone path so the
        //    delete helper can run `git worktree remove` from the
        //    right cwd.
        let bare_paths = discover_bare_clones(&self.base_dir().join("repos")).await;
        let mut porcelain: HashMap<PathBuf, (PathBuf, PorcelainEntry)> = HashMap::new();
        for bare in &bare_paths {
            if let Ok(entries) = list_porcelain(bare).await {
                for entry in entries {
                    let key = canonical_or_self(&entry.path);
                    porcelain.insert(key, (bare.clone(), entry));
                }
            }
        }

        // 2. Walk on-disk dirs under `worktrees/`. Anything here that
        //    isn't in `porcelain` is "ghost on disk" (git doesn't know
        //    about it); anything in `porcelain` whose dir is gone is
        //    "ghost in git metadata" — both cases get surfaced.
        let tracked_by_path: HashMap<PathBuf, &TrackedSession> = tracked
            .iter()
            .map(|t| (canonical_or_self(&t.worktree_path), t))
            .collect();

        let mut inspections: Vec<WorktreeInspection> = Vec::new();
        let mut seen_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

        if wt_root.exists() {
            let mut dir = tokio::fs::read_dir(&wt_root).await?;
            while let Some(entry) = dir.next_entry().await? {
                if !entry.file_type().await?.is_dir() {
                    continue;
                }
                let path = entry.path();
                let key = canonical_or_self(&path);
                seen_paths.insert(key.clone());

                let inspection = inspect_one(&path, &key, &porcelain, &tracked_by_path).await;
                inspections.push(inspection);
            }
        }

        // Worktrees git knows about whose on-disk dir vanished — these
        // never show up in the read_dir walk above. Surface them as
        // prunable so the user (or the bulk action) can clear the
        // bare clone's stale metadata.
        for (key, (bare, entry)) in &porcelain {
            if seen_paths.contains(key) {
                continue;
            }
            inspections.push(WorktreeInspection {
                path: entry.path.clone(),
                bare_path: Some(bare.clone()),
                branch: entry.branch.clone(),
                session_id: None,
                reasons: vec![OrphanReason::Prunable],
                size_bytes: 0,
                last_modified: None,
                has_uncommitted_changes: false,
                has_unpushed_commits: false,
                is_safe_to_delete: true,
            });
        }

        // Stable order — caller-friendly + deterministic for tests.
        inspections.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(inspections)
    }

    /// Delete a worktree the inspector flagged. Refuses if the entry
    /// is locked or has uncommitted/unpushed work, unless `force` is
    /// set — callers (CLI confirm prompt, bulk-safe action) decide
    /// whether to override.
    ///
    /// Uses `git worktree remove` first, falling back to
    /// `git worktree prune` when the directory has already vanished
    /// (mirrors [`WorktreeManager::remove_by_path`]'s contract).
    pub async fn delete_inspected(
        &self,
        inspection: &WorktreeInspection,
        force: bool,
    ) -> Result<(), GitError> {
        if !force {
            if inspection.reasons.contains(&OrphanReason::Locked) {
                return Err(GitError::Command(format!(
                    "worktree {} is locked — pass force to override",
                    inspection.path.display()
                )));
            }
            if inspection.has_uncommitted_changes {
                return Err(GitError::Command(format!(
                    "worktree {} has uncommitted changes — pass force to override",
                    inspection.path.display()
                )));
            }
            if inspection.has_unpushed_commits {
                return Err(GitError::Command(format!(
                    "worktree {} has unpushed commits — pass force to override",
                    inspection.path.display()
                )));
            }
        }

        // No bare clone known → fall back to a plain `rm -rf`. This
        // path is for "ghost on disk" entries where the matching
        // bare clone was deleted manually; git has no metadata to
        // clean up either.
        let Some(bare) = inspection.bare_path.as_ref() else {
            if inspection.path.exists() {
                tokio::fs::remove_dir_all(&inspection.path).await?;
            }
            return Ok(());
        };

        self.remove_by_path(bare, &inspection.path).await
    }
}

async fn inspect_one(
    path: &Path,
    canon: &Path,
    porcelain: &HashMap<PathBuf, (PathBuf, PorcelainEntry)>,
    tracked_by_path: &HashMap<PathBuf, &TrackedSession>,
) -> WorktreeInspection {
    let porcelain_entry = porcelain.get(canon);
    let bare_path = porcelain_entry.map(|(b, _)| b.clone());
    let branch = porcelain_entry.and_then(|(_, e)| e.branch.clone());
    let locked = porcelain_entry.is_some_and(|(_, e)| e.locked);
    let prunable = porcelain_entry.is_some_and(|(_, e)| e.prunable);

    let tracked = tracked_by_path.get(canon).copied();
    let session_id = tracked.map(|t| t.session_id.clone());

    let mut reasons: Vec<OrphanReason> = Vec::new();
    if locked {
        reasons.push(OrphanReason::Locked);
    }
    if prunable {
        reasons.push(OrphanReason::Prunable);
    }
    if tracked.is_none() {
        reasons.push(OrphanReason::Untracked);
    } else if tracked.is_some_and(|t| t.is_stopped) {
        reasons.push(OrphanReason::SessionStopped);
    }

    // Five independent probes per worktree: two ref lookups against
    // the bare clone (cheap stat calls), `git status --porcelain` and
    // `git rev-list @{u}..HEAD` against the worktree (each ~5-15ms),
    // and a recursive disk walk. They share no inputs, so fan them
    // out together — wall-clock drops from sum to max. Each helper
    // already converts failures to a defaulted value, so `join!`
    // never fails the surrounding future.
    let branch_refs: Option<(String, String)> = branch.as_ref().map(|b| {
        (
            format!("refs/heads/{b}"),
            format!("refs/remotes/origin/{b}"),
        )
    });
    let (local_exists, remote_exists, size_pair, has_uncommitted_changes, has_unpushed_commits) =
        tokio::join!(
            async {
                match (bare_path.as_ref(), branch_refs.as_ref()) {
                    (Some(bare), Some((local_ref, _))) => ref_exists(bare, local_ref).await,
                    _ => false,
                }
            },
            async {
                match (bare_path.as_ref(), branch_refs.as_ref()) {
                    (Some(bare), Some((_, remote_ref))) => ref_exists(bare, remote_ref).await,
                    _ => false,
                }
            },
            size_and_mtime(path),
            uncommitted(path),
            unpushed(path),
        );
    let (size_bytes, last_modified) = size_pair;

    // Branch-existence reasons only fire when we knew the branch +
    // bare in the first place; otherwise the lookups defaulted to
    // `false` and would spuriously flag detached HEADs.
    if bare_path.is_some() && branch.is_some() {
        if !remote_exists && local_exists {
            reasons.push(OrphanReason::BranchDeletedUpstream);
        }
        if !local_exists {
            reasons.push(OrphanReason::BranchMissingLocally);
        }
    }

    let is_safe_to_delete =
        !locked && !has_uncommitted_changes && !has_unpushed_commits && reasons.iter().any(|r| {
            matches!(
                r,
                OrphanReason::Untracked
                    | OrphanReason::SessionStopped
                    | OrphanReason::BranchDeletedUpstream
                    | OrphanReason::BranchMissingLocally
                    | OrphanReason::Prunable
            )
        });

    WorktreeInspection {
        path: path.to_path_buf(),
        bare_path,
        branch,
        session_id,
        reasons,
        size_bytes,
        last_modified,
        has_uncommitted_changes,
        has_unpushed_commits,
        is_safe_to_delete,
    }
}

/// Walk `<base>/repos/` two levels deep, returning every `*.git` dir.
async fn discover_bare_clones(repos_root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !repos_root.exists() {
        return out;
    }
    let Ok(mut owners) = tokio::fs::read_dir(repos_root).await else {
        return out;
    };
    while let Ok(Some(owner)) = owners.next_entry().await {
        if !owner.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(mut repos) = tokio::fs::read_dir(owner.path()).await else {
            continue;
        };
        while let Ok(Some(repo)) = repos.next_entry().await {
            let path = repo.path();
            if path.extension().and_then(|s| s.to_str()) == Some("git")
                && repo
                    .file_type()
                    .await
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
            {
                out.push(path);
            }
        }
    }
    out
}

/// Parse `git worktree list --porcelain` output. The format is
/// blank-line-separated stanzas of `key value` lines; we only consume
/// the keys we care about and ignore the rest so future git versions
/// adding new keys don't break parsing.
async fn list_porcelain(bare: &Path) -> Result<Vec<PorcelainEntry>, GitError> {
    let output = apply_git_env(
        Command::new("git")
            .current_dir(bare)
            .args(["worktree", "list", "--porcelain"]),
    )
    .output()
    .await?;
    if !output.status.success() {
        return Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut out: Vec<PorcelainEntry> = Vec::new();
    let mut cur: Option<PorcelainEntry> = None;
    for line in stdout.lines() {
        if line.is_empty() {
            if let Some(e) = cur.take() {
                out.push(e);
            }
            continue;
        }
        let entry = cur.get_or_insert_with(PorcelainEntry::default);
        if let Some(rest) = line.strip_prefix("worktree ") {
            entry.path = PathBuf::from(rest);
        } else if let Some(rest) = line.strip_prefix("branch ") {
            // git emits `refs/heads/<name>`; strip the prefix so the
            // surfaced name matches what users see.
            entry.branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        } else if line == "locked" || line.starts_with("locked ") {
            entry.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            entry.prunable = true;
        } else if line == "bare" {
            entry.is_bare = true;
        }
    }
    if let Some(e) = cur {
        out.push(e);
    }
    // Drop the bare clone's own row. Two checks because the path
    // form alone is unreliable: macOS canonicalizes `/var/folders/X`
    // to `/private/var/folders/X` inside `git worktree list`, but the
    // bare path we hold was never canonicalized. The `bare` keyword
    // in the porcelain output is what we actually want — `e.path !=
    // bare` is kept as a belt-and-suspenders for the (very rare)
    // case where git omits the keyword.
    out.retain(|e| !e.is_bare && e.path != bare && !e.path.as_os_str().is_empty());
    Ok(out)
}

async fn ref_exists(bare: &Path, ref_name: &str) -> bool {
    apply_git_env(
        Command::new("git")
            .current_dir(bare)
            .args(["show-ref", "--verify", "--quiet", ref_name]),
    )
    .output()
    .await
    .map(|o| o.status.success())
    .unwrap_or(false)
}

async fn uncommitted(worktree: &Path) -> bool {
    let Ok(output) = apply_git_env(
        Command::new("git")
            .current_dir(worktree)
            .args(["status", "--porcelain"]),
    )
    .output()
    .await
    else {
        return false;
    };
    output.status.success() && !output.stdout.is_empty()
}

async fn unpushed(worktree: &Path) -> bool {
    // `@{u}` resolves the configured upstream. If there's no upstream
    // set the command fails — we treat "no upstream" as "no unpushed
    // commits to worry about" because the cleanup path assumes a
    // pilot-created worktree where the branch either tracks origin or
    // was never published.
    let Ok(output) = apply_git_env(
        Command::new("git")
            .current_dir(worktree)
            .args(["rev-list", "--count", "@{u}..HEAD"]),
    )
    .output()
    .await
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().parse::<u64>().map(|n| n > 0).unwrap_or(false)
}

/// Recursive `du`-style size + max mtime walk. Best-effort: any entry
/// that errors (permission denied, symlink loop) contributes 0 bytes
/// and is skipped for mtime.
async fn size_and_mtime(root: &Path) -> (u64, Option<SystemTime>) {
    let root = root.to_path_buf();
    let res = tokio::task::spawn_blocking(move || walk_sync(&root)).await;
    res.unwrap_or((0, None))
}

fn walk_sync(root: &Path) -> (u64, Option<SystemTime>) {
    let mut total: u64 = 0;
    let mut newest: Option<SystemTime> = None;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if let Ok(mt) = meta.modified() {
            newest = Some(newest.map(|n| n.max(mt)).unwrap_or(mt));
        }
        if meta.is_dir() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                stack.push(entry.path());
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    (total, newest)
}

fn canonical_or_self(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}
