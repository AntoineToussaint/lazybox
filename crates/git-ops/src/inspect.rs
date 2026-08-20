//! Worktree inspector — scans the worktrees directory + every bare
//! clone, reports each worktree's status (orphan reasons, size, last
//! modified, uncommitted/unpushed work), and exposes a safety-aware
//! delete helper.
//!
//! Decoupling: the inspector takes session metadata as plain
//! [`TrackedSession`] input rather than reaching into `lazybox-store` /
//! `lazybox-core`. That keeps `lazybox-git-ops` source-agnostic — the CLI
//! / TUI translates store records into `TrackedSession` and calls in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::{
    GitError, GitRunner, WorktreeManager, WorktreeReclaimBlocker, WorktreeReclaimOutcome,
    default_git_runner,
};

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
    /// Lazybox tracks a session pointing at this directory, but the
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
    /// lazybox-created worktrees, common for hand-managed ones).
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
    /// `git status --porcelain` completed successfully. A `false` value is
    /// distinct from a verified-clean checkout: destructive preflights must
    /// preserve the worktree when cleanliness could not be established.
    pub status_verified: bool,
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

/// A git checkout discovered on disk by [`scan_external_checkouts`] —
/// a normal clone or a linked `git worktree` living OUTSIDE lazybox's
/// managed layout, i.e. one lazybox did not create. Read-only: the
/// scan never mutates anything, and importing a checkout is a separate
/// step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCheckout {
    /// Absolute path to the checkout's working directory.
    pub path: PathBuf,
    /// Checked-out branch, or `None` on a detached HEAD.
    pub branch: Option<String>,
    /// `true` when this directory is a linked `git worktree` (its
    /// `.git` is a *file* pointing into another repo's `worktrees/`),
    /// `false` for a primary checkout or standalone clone.
    pub is_linked_worktree: bool,
    /// `origin` remote URL, when the checkout has one.
    pub remote_url: Option<String>,
    /// Commit time of `HEAD` — the checkout's "last activity", as
    /// seconds since the Unix epoch. `None` on an empty repo (no
    /// commits yet) or when the probe fails.
    pub last_commit_unix: Option<u64>,
    /// `git status --porcelain` reported at least one entry.
    pub has_uncommitted_changes: bool,
}

/// Combined status and `HEAD` diff for one checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeDiff {
    pub status: Vec<String>,
    pub stat: Vec<String>,
    pub files: Vec<DiffFile>,
    pub truncated: bool,
}

/// One file in a worktree diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub old_path: Option<String>,
    pub path: String,
    pub headers: Vec<String>,
    pub hunks: Vec<DiffHunk>,
}

/// One unified-diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

/// One line inside a unified-diff hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Meta,
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
    /// Return live worktrees for `branch` that belong to this manager's
    /// bare clone and sit under its managed `worktrees/` root.
    ///
    /// This is narrower than [`Self::inspect_worktrees`]: callers that
    /// need to claim an existing checkout can inspect one repo/branch
    /// without walking every cached repository or probing worktree
    /// cleanliness. Paths outside the managed root are deliberately
    /// excluded even when they are registered against the same bare clone.
    pub async fn managed_worktrees_for_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Vec<PathBuf>, GitError> {
        let bare = self.bare_path(owner, repo);
        if !bare.exists() {
            return Ok(Vec::new());
        }

        let lock = crate::repo_lock(&bare);
        let _guard = lock.lock().await;
        let managed_root = canonical_or_self(&self.base_dir().join("worktrees"));
        let mut paths = Vec::new();
        for entry in list_porcelain(self.git_runner(), &bare).await? {
            if entry.prunable || entry.branch.as_deref() != Some(branch) {
                continue;
            }
            let path = canonical_or_self(&entry.path);
            if path.starts_with(&managed_root) && crate::worktree_dir_ready(&entry.path).await {
                paths.push(entry.path);
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Remove a non-live managed holder only when it has no local
    /// work to preserve. The caller owns session-liveness validation;
    /// this boundary validates that `path` is a registered worktree for
    /// `branch`, lives under lazybox's managed root, is unlocked, clean,
    /// and has no unpushed commits before asking git to remove it.
    ///
    /// The local branch ref is deliberately retained so the caller can
    /// immediately check it out at the intended workspace path.
    pub async fn reclaim_managed_worktree_if_safe(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        path: &Path,
    ) -> Result<WorktreeReclaimOutcome, GitError> {
        let bare = self.bare_path(owner, repo);
        if !bare.exists() {
            return Ok(WorktreeReclaimOutcome::NotManaged);
        }

        let lock = crate::repo_lock(&bare);
        let _guard = lock.lock().await;
        let managed_root = canonical_or_self(&self.base_dir().join("worktrees"));
        let key = canonical_or_self(path);
        if !key.starts_with(&managed_root) {
            return Ok(WorktreeReclaimOutcome::NotManaged);
        }

        let entries = list_porcelain(self.git_runner(), &bare).await?;
        let Some(entry) = entries.into_iter().find(|entry| {
            !entry.prunable
                && entry.branch.as_deref() == Some(branch)
                && canonical_or_self(&entry.path) == key
        }) else {
            return Ok(WorktreeReclaimOutcome::NotManaged);
        };
        if entry.locked {
            return Ok(WorktreeReclaimOutcome::Blocked(
                WorktreeReclaimBlocker::Locked,
            ));
        }

        let status = crate::run_git_in(
            self.git_runner(),
            path,
            &[
                "status",
                "--porcelain=v1",
                "--ignored",
                "--untracked-files=all",
            ],
        )
        .await?;
        if status.lines().any(|line| line.starts_with("!! ")) {
            return Ok(WorktreeReclaimOutcome::Blocked(
                WorktreeReclaimBlocker::IgnoredFiles,
            ));
        }
        if !status.trim().is_empty() {
            return Ok(WorktreeReclaimOutcome::Blocked(
                WorktreeReclaimBlocker::UncommittedChanges,
            ));
        }
        if unpushed(self.git_runner(), path, Some(&bare), Some(branch)).await {
            return Ok(WorktreeReclaimOutcome::Blocked(
                WorktreeReclaimBlocker::UnpushedCommits,
            ));
        }

        crate::run_git_in(
            self.git_runner(),
            &bare,
            &["worktree", "remove", &path.to_string_lossy()],
        )
        .await?;
        Ok(WorktreeReclaimOutcome::Reclaimed)
    }

    /// Scan the worktrees directory + every bare clone under
    /// `base/repos/**/*.git` and report each worktree's health.
    ///
    /// `tracked` is the set of sessions lazybox currently knows about
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
            if let Ok(entries) = list_porcelain(self.git_runner(), bare).await {
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

                let inspection =
                    inspect_one(self.git_runner(), &path, &key, &porcelain, &tracked_by_path).await;
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
                status_verified: false,
                has_unpushed_commits: false,
                is_safe_to_delete: true,
            });
        }

        // Stable order — caller-friendly + deterministic for tests.
        inspections.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(inspections)
    }

    /// Delete a worktree the inspector flagged. A non-force call treats the
    /// inspection as advisory and freshly re-checks locked, dirty, and
    /// unpushed state under the repo lock before removal. `force` remains an
    /// explicit administrative override for the orphan-cleanup UI; workspace
    /// lifecycle deletion never uses it.
    pub async fn delete_inspected(
        &self,
        inspection: &WorktreeInspection,
        force: bool,
    ) -> Result<(), GitError> {
        self.delete_inspected_if(inspection, force, || true)
            .await
            .map(|_| ())
    }

    /// Delete an inspected worktree only if the caller's ownership guard
    /// still holds. A non-force delete re-runs lock, status, and unpushed
    /// probes while holding the per-repo lock, then invokes `git worktree
    /// remove` without `--force` and without an `rm -rf` fallback. Thus a
    /// stale inspection can inform a prompt but can never authorize deletion.
    pub async fn delete_inspected_if<F>(
        &self,
        inspection: &WorktreeInspection,
        force: bool,
        still_removable: F,
    ) -> Result<bool, GitError>
    where
        F: FnOnce() -> bool + Send,
    {
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

        // No bare clone known → fall back to a plain `rm -rf`. This path
        // is for "ghost on disk" entries where the matching bare clone was
        // deleted manually; git has no metadata to clean up either.
        //
        // But the `has_uncommitted_changes` / `has_unpushed_commits` guards
        // above are hollow here: both probes run git *inside* the worktree,
        // which fails on a severed `.git` (its gitdir is gone) and defaults
        // to "clean" — so `is_safe_to_delete` can be true for a directory
        // that still holds real, possibly uncommitted, work. With no bare
        // clone to verify the checkout against (the way
        // `leftover_is_pristine_checkout` does), we cannot confirm the
        // content is disposable. So without `force`, only empty-ish debris
        // (at most a `.git`) is cleared outright; a content-bearing ghost
        // is refused rather than `rm -rf`'d blind.
        let Some(bare) = inspection.bare_path.as_ref() else {
            if !still_removable() {
                return Ok(false);
            }
            if !force && directory_has_real_content(&inspection.path).await {
                return Err(GitError::Command(format!(
                    "worktree {} has no bare clone to verify against and holds files — \
                     refusing to delete unverifiable content; pass force to override",
                    inspection.path.display()
                )));
            }
            if inspection.path.exists() {
                tokio::fs::remove_dir_all(&inspection.path).await?;
            }
            return Ok(true);
        };

        if force {
            let removed = self
                .remove_by_path_if(bare, &inspection.path, still_removable)
                .await?;
            if removed && let Some(branch) = inspection.branch.as_deref() {
                delete_local_branch(self.git_runner(), bare, branch).await;
            }
            return Ok(removed);
        }

        // Re-run every destructive safety probe while holding the same
        // per-repo lock as the removal. The earlier inspection is evidence
        // for UI copy only; it is never the final delete authority.
        let lock = crate::repo_lock(bare);
        let _guard = lock.lock().await;
        if !still_removable() {
            return Ok(false);
        }
        if !inspection.path.exists() {
            let _ = crate::run_git_in(self.git_runner(), bare, &["worktree", "prune"]).await;
            return Ok(true);
        }

        let path_key = canonical_or_self(&inspection.path);
        let entries = list_porcelain(self.git_runner(), bare).await?;
        let Some(entry) = entries
            .iter()
            .find(|entry| canonical_or_self(&entry.path) == path_key)
        else {
            return Err(GitError::Command(format!(
                "worktree {} is no longer registered with {}; refusing an unverifiable delete",
                inspection.path.display(),
                bare.display(),
            )));
        };
        if entry.locked {
            return Err(GitError::Command(format!(
                "worktree {} is locked — refusing to delete",
                inspection.path.display()
            )));
        }
        if uncommitted(self.git_runner(), &inspection.path).await != Some(false) {
            return Err(GitError::Command(format!(
                "worktree {} has uncommitted or unverifiable changes — refusing to delete",
                inspection.path.display()
            )));
        }
        if unpushed(
            self.git_runner(),
            &inspection.path,
            Some(bare),
            inspection.branch.as_deref(),
        )
        .await
        {
            return Err(GitError::Command(format!(
                "worktree {} has unpushed or unverifiable commits — refusing to delete",
                inspection.path.display()
            )));
        }

        crate::run_git_in(
            self.git_runner(),
            bare,
            &["worktree", "remove", &inspection.path.to_string_lossy()],
        )
        .await?;

        // `git worktree remove` only drops the working tree — the
        // `refs/heads/<branch>` ref survives. Now that the worktree is
        // gone, reap the local branch too, so merged / abandoned
        // feature branches don't pile up in the bare clone (issue #160).
        if let Some(branch) = inspection.branch.as_deref() {
            delete_local_branch(self.git_runner(), bare, branch).await;
        }
        Ok(true)
    }
}

/// Walk `roots` (each up to `max_depth` directory levels deep) and
/// report every git checkout found — normal clones and linked
/// `git worktree`s alike — that the user created outside lazybox.
///
/// Read-only and daemon-free: a plain filesystem walk plus cheap
/// read-only `git` probes per checkout, each capped by a short
/// per-probe timeout so a checkout on a dead network mount can't
/// wedge the scan. A directory that is itself a git checkout is
/// reported and NOT descended into — its subdirectories belong to that
/// repo, not to separate ones — which also keeps the walk bounded on
/// large trees.
///
/// Directory symlinks are followed (so repos reached through a
/// symlinked parent are still found), with a visited-set guard that
/// makes symlink cycles terminate. `include_hidden` controls whether
/// `.`-prefixed directories are descended into: `false` (the default)
/// skips them — dotdirs are overwhelmingly caches/config and dominate
/// a home-directory scan — while a root passed explicitly is always
/// walked regardless. `true` includes them.
///
/// `exclude_under` drops any checkout inside lazybox's own managed
/// base (`repos/`, `worktrees/`) so the scan surfaces only external
/// work. Results are deduplicated by canonical path and sorted by
/// path for determinism.
pub async fn scan_external_checkouts(
    roots: &[PathBuf],
    max_depth: usize,
    include_hidden: bool,
    exclude_under: &Path,
) -> Vec<DiscoveredCheckout> {
    let exclude = canonical_or_self(exclude_under);
    let mut paths: Vec<PathBuf> = Vec::new();
    // Canonical paths of directories already walked — shared across
    // roots so overlapping roots and symlink cycles never re-descend
    // (nor double-report a checkout).
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for root in roots {
        collect_checkouts(
            root,
            max_depth,
            include_hidden,
            &exclude,
            &mut paths,
            &mut visited,
        )
        .await;
    }

    let mut out: Vec<DiscoveredCheckout> = Vec::with_capacity(paths.len());
    for path in paths {
        out.push(describe_checkout(default_git_runner(), path).await);
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Depth-bounded walk under `dir`, pushing the path of each git
/// checkout into `found`. Every popped directory is recorded in
/// `visited` by canonical path and skipped if already there, so a
/// symlink cycle (or a directory reachable from two roots) terminates
/// and is reported at most once. A found checkout is not descended
/// into. `dir` sits at depth 0, so a checkout can be reported at any
/// depth in `0..=max_depth`.
async fn collect_checkouts(
    dir: &Path,
    max_depth: usize,
    include_hidden: bool,
    exclude: &Path,
    found: &mut Vec<PathBuf>,
    visited: &mut std::collections::HashSet<PathBuf>,
) {
    let mut stack: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    while let Some((path, depth)) = stack.pop() {
        let canon = canonical_or_self(&path);
        if canon.starts_with(exclude) {
            continue;
        }
        if !visited.insert(canon) {
            continue;
        }
        if is_git_checkout(&path).await {
            found.push(path);
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(mut entries) = tokio::fs::read_dir(&path).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !include_hidden && entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            // Follow directory symlinks: `file_type()` from readdir
            // never follows, so a symlinked repo dir would otherwise be
            // invisible. Only pay for the extra `metadata()` stat (which
            // does follow) on entries that are actually symlinks.
            let is_dir = match entry.file_type().await {
                Ok(t) if t.is_dir() => true,
                Ok(t) if t.is_symlink() => tokio::fs::metadata(entry.path())
                    .await
                    .map(|m| m.is_dir())
                    .unwrap_or(false),
                _ => false,
            };
            if is_dir {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
}

/// Whether `path` is the root of a git checkout — has a `.git` entry,
/// which a standalone repo / primary checkout carries as a directory
/// and a linked worktree as a file. `symlink_metadata` so a `.git`
/// symlink still counts.
async fn is_git_checkout(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path.join(".git")).await.is_ok()
}

/// Wall-clock cap for a single read-only scan probe. These are all
/// local git reads (no network), but a checkout on a stale/dead
/// network mount could still block indefinitely — and the `scan` CLI
/// has no daemon-side timeout to fall back on, so it must bound them
/// itself. Generous: a slow local `git status` on a huge tree still
/// fits.
const SCAN_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run one read-only `git` probe in `path`, capped by
/// [`SCAN_PROBE_TIMEOUT`]. `None` on spawn failure, non-zero exit,
/// or timeout — every caller already treats "no answer" as a
/// defaulted value, so a wedged probe degrades one field instead of
/// hanging the scan. `kill_on_drop` reaps the timed-out child.
async fn scan_git(git: &dyn GitRunner, path: &Path, args: &[&str]) -> Option<std::process::Output> {
    match tokio::time::timeout(SCAN_PROBE_TIMEOUT, git.run(Some(path), args, &[])).await {
        Ok(Ok(out)) if out.status.success() => Some(out),
        _ => None,
    }
}

/// Describe a single on-disk path the way [`scan_external_checkouts`]
/// describes each entry it finds. `None` when `path` is not the root
/// of a git checkout (no `.git`), so the import handler can reject a
/// path that has been moved or deleted since it was scanned. Read-only:
/// runs the same cheap `git` probes as the scan, each under its own
/// timeout.
pub async fn describe_checkout_at(path: PathBuf) -> Option<DiscoveredCheckout> {
    if !is_git_checkout(&path).await {
        return None;
    }
    Some(describe_checkout(default_git_runner(), path).await)
}

/// Gather the read-only facts one discovered checkout carries.
async fn describe_checkout(git: &dyn GitRunner, path: PathBuf) -> DiscoveredCheckout {
    let is_linked_worktree = tokio::fs::metadata(path.join(".git"))
        .await
        .map(|m| m.is_file())
        .unwrap_or(false);
    let (branch, remote_url, last_commit_unix, has_uncommitted_changes) = tokio::join!(
        checkout_branch(git, &path),
        checkout_remote_url(git, &path),
        checkout_last_commit_unix(git, &path),
        checkout_dirty(git, &path),
    );
    DiscoveredCheckout {
        path,
        branch,
        is_linked_worktree,
        remote_url,
        last_commit_unix,
        has_uncommitted_changes,
    }
}

/// The checkout's current branch. `None` on a detached HEAD.
async fn checkout_branch(git: &dyn GitRunner, path: &Path) -> Option<String> {
    let out = scan_git(git, path, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The `origin` remote URL, when the checkout has one configured.
async fn checkout_remote_url(git: &dyn GitRunner, path: &Path) -> Option<String> {
    let out = scan_git(git, path, &["remote", "get-url", "origin"]).await?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Committer time of `HEAD`, in seconds since the Unix epoch. `None`
/// on an empty repo (no commits) or any git failure.
async fn checkout_last_commit_unix(git: &dyn GitRunner, path: &Path) -> Option<u64> {
    let out = scan_git(git, path, &["log", "-1", "--format=%ct"]).await?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

/// Whether `git status --porcelain` reports any entry. A scan-local
/// twin of [`uncommitted`] that carries the probe timeout; the
/// inspector's own `uncommitted` runs under `WorktreeManager`'s
/// timeouts and is left untouched.
async fn checkout_dirty(git: &dyn GitRunner, path: &Path) -> bool {
    match scan_git(git, path, &["status", "--porcelain"]).await {
        Some(out) => !out.stdout.is_empty(),
        None => false,
    }
}

/// Best-effort removal of the local branch ref left behind after a
/// worktree is deleted. Two guards keep it safe:
///   - never delete the bare clone's HEAD branch (`main`/`master`/…),
///     which every other worktree shares — deleting it would orphan
///     them all;
///   - lean on git's own refusal to delete a branch still checked out
///     in another worktree.
///
/// Failures are logged and swallowed: a leftover branch is cosmetic and
/// must never fail the worktree removal it follows.
async fn delete_local_branch(git: &dyn GitRunner, bare: &Path, branch: &str) {
    if bare_head_branch(git, bare).await.as_deref() == Some(branch) {
        return;
    }
    let result = git.run(Some(bare), &["branch", "-D", branch], &[]).await;
    match result {
        Ok(o) if !o.status.success() => tracing::debug!(
            branch,
            stderr = %String::from_utf8_lossy(&o.stderr).trim(),
            "leftover local branch not deleted after worktree removal",
        ),
        Err(e) => tracing::debug!(
            branch,
            error = %e,
            "git branch -D failed after worktree removal",
        ),
        _ => {}
    }
}

/// The branch the bare clone's HEAD points at — the default branch
/// (`main`/`master`/…). `None` on detached HEAD or any git failure.
async fn bare_head_branch(git: &dyn GitRunner, bare: &Path) -> Option<String> {
    let output = git
        .run(Some(bare), &["symbolic-ref", "--short", "HEAD"], &[])
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!name.is_empty()).then_some(name)
}

async fn inspect_one(
    git: &dyn GitRunner,
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
    // `git rev-list @{u}..HEAD` against the worktree (each ~5-15ms, and
    // only when the worktree's git dir is still live — see below), and
    // a recursive disk walk. They share no inputs, so fan them out
    // together — wall-clock drops from sum to max. Each helper already
    // converts failures to a defaulted value, so `join!` never fails
    // the surrounding future.
    let branch_refs: Option<(String, String)> = branch.as_ref().map(|b| {
        (
            format!("refs/heads/{b}"),
            format!("refs/remotes/origin/{b}"),
        )
    });
    let (local_exists, remote_exists, size_pair, (uncommitted_state, has_unpushed_commits)) = tokio::join!(
        async {
            match (bare_path.as_ref(), branch_refs.as_ref()) {
                (Some(bare), Some((local_ref, _))) => ref_exists(git, bare, local_ref).await,
                _ => false,
            }
        },
        async {
            match (bare_path.as_ref(), branch_refs.as_ref()) {
                (Some(bare), Some((_, remote_ref))) => ref_exists(git, bare, remote_ref).await,
                _ => false,
            }
        },
        size_and_mtime(path),
        // The two in-worktree probes, gated on the worktree's git dir
        // still being usable. A managed worktree whose bare clone was
        // deleted keeps a `.git` file pointing at a now-missing gitdir;
        // every `git` command in it then fails with "not a git
        // repository", so each sweep re-spawned `git status` / `rev-list`
        // only to fail-and-log before defaulting. `worktree_dir_ready`
        // detects that from disk (no git spawn) — reusing the same helper
        // the spawn fast path uses, which already handles symlinked and
        // relative `.git` pointers correctly. Skipping is safe in the
        // delete direction: `is_safe_to_delete` below requires
        // `uncommitted_state == Some(false)`, which a skipped `git status`
        // (→ `None`) can never satisfy — a skip can only ever hold that
        // flag at false, never flip it to true. The liveness check rides
        // this join arm so it overlaps the bare-clone lookups + disk walk
        // rather than serializing ahead of the fan-out.
        async {
            if crate::worktree_dir_ready(path).await {
                tokio::join!(
                    uncommitted(git, path),
                    unpushed(git, path, bare_path.as_deref(), branch.as_deref())
                )
            } else {
                (None, false)
            }
        },
    );
    let (size_bytes, last_modified) = size_pair;
    let has_uncommitted_changes = uncommitted_state.unwrap_or(false);

    // Branch-existence reasons only fire when we knew the branch +
    // bare in the first place; otherwise the lookups defaulted to
    // `false` and would spuriously flag detached HEADs.
    if let (Some(bare), Some(branch_name)) = (bare_path.as_deref(), branch.as_deref()) {
        if !remote_exists && local_exists {
            // Only classify "deleted upstream" when the remote ref
            // plausibly existed: the branch has tracking config, or
            // its tip is reachable from a remote-tracking ref. A
            // never-pushed local branch (`lazybox/issue-N` cut off
            // origin/main) never had a remote ref to delete — calling
            // it BranchDeletedUpstream made `is_safe_to_delete` true
            // and the rescope reaper deleted committed local work.
            // (Both probes are sequential + lazy: this arm is rare.)
            if branch_has_upstream_config(git, bare, branch_name).await
                || branch_tip_on_remote(git, bare, branch_name).await
            {
                reasons.push(OrphanReason::BranchDeletedUpstream);
            }
        }
        if !local_exists {
            reasons.push(OrphanReason::BranchMissingLocally);
        }
    }

    let is_safe_to_delete = !locked
        && uncommitted_state == Some(false)
        && !has_unpushed_commits
        && reasons.iter().any(|r| {
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
        status_verified: uncommitted_state.is_some(),
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
                && repo.file_type().await.map(|t| t.is_dir()).unwrap_or(false)
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
async fn list_porcelain(git: &dyn GitRunner, bare: &Path) -> Result<Vec<PorcelainEntry>, GitError> {
    let output = git
        .run(Some(bare), &["worktree", "list", "--porcelain"], &[])
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

async fn ref_exists(git: &dyn GitRunner, bare: &Path, ref_name: &str) -> bool {
    git.run(
        Some(bare),
        &["show-ref", "--verify", "--quiet", ref_name],
        &[],
    )
    .await
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// True when the worktree holds nothing that exists only locally — no
/// uncommitted changes and no unpushed commits. `bare` and `branch`
/// feed the upstream fallbacks of the unpushed-commit probe (`branch`
/// unlocks the branch-own-remote-ref tier of the private `unpushed`
/// helper); when
/// nothing resolves an upstream-less checkout is conservatively
/// reported non-pristine.
pub async fn worktree_is_pristine(
    worktree: &Path,
    bare: Option<&Path>,
    branch: Option<&str>,
) -> bool {
    worktree_is_pristine_with(default_git_runner(), worktree, bare, branch).await
}

/// Read the checkout's status and combined staged/unstaged diff against
/// `HEAD`. A throwaway index marks untracked paths intent-to-add so one
/// bounded diff includes them without mutating the checkout's real index.
pub async fn inspect_worktree_diff(worktree: &Path) -> Result<WorktreeDiff, GitError> {
    inspect_worktree_diff_with(default_git_runner(), worktree).await
}

const DIFF_STATUS_BYTES: usize = 512 * 1024;
const DIFF_PATCH_BYTES: usize = 4 * 1024 * 1024;
const DIFF_STAT_BYTES: usize = 512 * 1024;

struct ReviewIndex(PathBuf);

impl ReviewIndex {
    fn new() -> Self {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "lazybox-review-index-{}-{created}-{sequence}",
            std::process::id(),
        )))
    }

    fn env(&self) -> Vec<(String, String)> {
        vec![(
            "GIT_INDEX_FILE".to_string(),
            self.0.to_string_lossy().into_owned(),
        )]
    }
}

impl Drop for ReviewIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.as_os_str().to_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

async fn inspect_worktree_diff_with(
    git: &dyn GitRunner,
    worktree: &Path,
) -> Result<WorktreeDiff, GitError> {
    let (mut status_output, status_truncated) = git
        .run_with_stdout_limit(
            Some(worktree),
            &["status", "--porcelain=v1", "--untracked-files=all", "-z"],
            &[],
            DIFF_STATUS_BYTES,
        )
        .await?;
    if !status_truncated {
        require_success(status_output.status.success(), &status_output.stderr)?;
    }
    if status_truncated {
        truncate_after_last(&mut status_output.stdout, 0);
    }
    let status = parse_status_porcelain(&status_output.stdout);
    let head = git
        .run(
            Some(worktree),
            &["rev-parse", "--verify", "--quiet", "HEAD"],
            &[],
        )
        .await?;
    let has_head = head.status.success();

    let index = ReviewIndex::new();
    let index_env = index.env();
    let index_output = if has_head {
        git.run(Some(worktree), &["read-tree", "HEAD"], &index_env)
            .await?
    } else {
        git.run(Some(worktree), &["read-tree", "--empty"], &index_env)
            .await?
    };
    require_success(index_output.status.success(), &index_output.stderr)?;
    let intent_output = git
        .run(Some(worktree), &["add", "-N", "-A", "--"], &index_env)
        .await?;
    require_success(intent_output.status.success(), &intent_output.stderr)?;

    let patch_args = if has_head {
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--find-copies",
            "HEAD",
            "--",
        ][..]
    } else {
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--find-renames",
            "--find-copies",
            "--",
        ][..]
    };
    let (mut patch_output, patch_truncated) = git
        .run_with_stdout_limit(Some(worktree), patch_args, &index_env, DIFF_PATCH_BYTES)
        .await?;
    if !patch_truncated {
        require_success(patch_output.status.success(), &patch_output.stderr)?;
    } else {
        truncate_after_last(&mut patch_output.stdout, b'\n');
    }

    let stat_args = if has_head {
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--stat",
            "HEAD",
            "--",
        ][..]
    } else {
        &["diff", "--no-ext-diff", "--no-color", "--stat", "--"][..]
    };
    let (mut stat_output, stat_truncated) = git
        .run_with_stdout_limit(Some(worktree), stat_args, &index_env, DIFF_STAT_BYTES)
        .await?;
    if !stat_truncated {
        require_success(stat_output.status.success(), &stat_output.stderr)?;
    } else {
        truncate_after_last(&mut stat_output.stdout, b'\n');
    }

    Ok(WorktreeDiff {
        status,
        stat: text_lines(&stat_output.stdout),
        files: parse_unified_diff(&String::from_utf8_lossy(&patch_output.stdout)),
        truncated: status_truncated || patch_truncated || stat_truncated,
    })
}

fn require_success(success: bool, stderr: &[u8]) -> Result<(), GitError> {
    if success {
        Ok(())
    } else {
        Err(GitError::Command(
            String::from_utf8_lossy(stderr).trim().to_string(),
        ))
    }
}

fn text_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

fn truncate_after_last(bytes: &mut Vec<u8>, delimiter: u8) {
    let length = bytes
        .iter()
        .rposition(|byte| *byte == delimiter)
        .map_or(0, |index| index + 1);
    bytes.truncate(length);
}

fn parse_status_porcelain(bytes: &[u8]) -> Vec<String> {
    let mut fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut rows = Vec::new();
    while let Some(field) = fields.next() {
        let text = String::from_utf8_lossy(field);
        if text.len() < 3 {
            continue;
        }
        let code = &text[..2];
        let path = text[3..].to_string();
        if (code.contains('R') || code.contains('C'))
            && let Some(old) = fields.next()
        {
            rows.push(format!("{code} {} -> {path}", String::from_utf8_lossy(old)));
            continue;
        }
        rows.push(format!("{code} {path}"));
    }
    rows
}

fn parse_unified_diff(source: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut file: Option<DiffFile> = None;
    let mut hunk: Option<DiffHunk> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;

    for line in source.lines() {
        if line.starts_with("diff --git ") {
            finish_hunk(&mut file, &mut hunk);
            if let Some(previous) = file.take() {
                files.push(previous);
            }
            file = Some(DiffFile {
                old_path: None,
                path: diff_header_path(line).unwrap_or_default(),
                headers: vec![line.to_string()],
                hunks: Vec::new(),
            });
            continue;
        }

        let Some(current_file) = file.as_mut() else {
            continue;
        };
        if line.starts_with("@@ ") {
            finish_hunk(&mut file, &mut hunk);
            let (parsed_old, parsed_new) = parse_hunk_starts(line);
            old_line = parsed_old;
            new_line = parsed_new;
            hunk = Some(DiffHunk {
                header: line.to_string(),
                old_start: old_line,
                new_start: new_line,
                lines: Vec::new(),
            });
            continue;
        }
        if let Some(current_hunk) = hunk.as_mut() {
            let (kind, old, new) = match line.as_bytes().first() {
                Some(b'+') => {
                    let current = new_line;
                    new_line = new_line.saturating_add(1);
                    (DiffLineKind::Addition, None, Some(current))
                }
                Some(b'-') => {
                    let current = old_line;
                    old_line = old_line.saturating_add(1);
                    (DiffLineKind::Deletion, Some(current), None)
                }
                Some(b' ') => {
                    let old = old_line;
                    let new = new_line;
                    old_line = old_line.saturating_add(1);
                    new_line = new_line.saturating_add(1);
                    (DiffLineKind::Context, Some(old), Some(new))
                }
                _ => (DiffLineKind::Meta, None, None),
            };
            current_hunk.lines.push(DiffLine {
                kind,
                text: line.to_string(),
                old_line: old,
                new_line: new,
            });
            continue;
        }

        if let Some(path) = line.strip_prefix("--- ") {
            current_file.old_path = diff_marker_path(path);
        } else if let Some(path) = line.strip_prefix("+++ ")
            && let Some(path) = diff_marker_path(path)
        {
            current_file.path = path;
        }
        current_file.headers.push(line.to_string());
    }

    finish_hunk(&mut file, &mut hunk);
    if let Some(file) = file {
        files.push(file);
    }
    files
}

fn finish_hunk(file: &mut Option<DiffFile>, hunk: &mut Option<DiffHunk>) {
    if let (Some(file), Some(hunk)) = (file.as_mut(), hunk.take()) {
        file.hunks.push(hunk);
    }
}

fn diff_header_path(header: &str) -> Option<String> {
    let paths = header.strip_prefix("diff --git ")?;
    if paths.starts_with('"') {
        let (_, rest) = take_quoted_token(paths)?;
        let (new_path, _) = take_quoted_token(rest.trim_start())?;
        normalize_diff_path(new_path)
    } else {
        let (_, path) = paths.rsplit_once(" b/")?;
        Some(path.to_string())
    }
}

fn diff_marker_path(source: &str) -> Option<String> {
    let token = if source.starts_with('"') {
        take_quoted_token(source)?.0
    } else {
        source.split_once('\t').map_or(source, |(path, _)| path)
    };
    normalize_diff_path(token)
}

fn normalize_diff_path(path: &str) -> Option<String> {
    let path = decode_git_path(path)?;
    if path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(&path)
            .to_string(),
    )
}

fn take_quoted_token(source: &str) -> Option<(&str, &str)> {
    let bytes = source.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some((&source[..=index], &source[index + 1..]));
        }
    }
    None
}

fn decode_git_path(source: &str) -> Option<String> {
    if !source.starts_with('"') {
        return Some(source.to_string());
    }
    let inner = source.strip_prefix('"')?.strip_suffix('"')?;
    let bytes = inner.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        let escaped = *bytes.get(index)?;
        let value = match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'\\' => b'\\',
            b'"' => b'"',
            b'0'..=b'7' => {
                let mut value = escaped - b'0';
                let mut digits = 1;
                while digits < 3
                    && index + 1 < bytes.len()
                    && matches!(bytes[index + 1], b'0'..=b'7')
                {
                    index += 1;
                    value = value * 8 + (bytes[index] - b'0');
                    digits += 1;
                }
                value
            }
            _ => return None,
        };
        decoded.push(value);
        index += 1;
    }
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

fn parse_hunk_starts(header: &str) -> (u32, u32) {
    let mut ranges = header.split_whitespace().skip(1);
    let old = ranges.next().and_then(parse_range_start).unwrap_or(0);
    let new = ranges.next().and_then(parse_range_start).unwrap_or(0);
    (old, new)
}

fn parse_range_start(range: &str) -> Option<u32> {
    range.get(1..)?.split(',').next()?.parse::<u32>().ok()
}

async fn worktree_is_pristine_with(
    git: &dyn GitRunner,
    worktree: &Path,
    bare: Option<&Path>,
    branch: Option<&str>,
) -> bool {
    let (dirty, ahead) = tokio::join!(
        uncommitted(git, worktree),
        unpushed(git, worktree, bare, branch)
    );
    dirty == Some(false) && !ahead
}

/// Whether `path` holds anything other than a `.git` entry — the same
/// "real work vs. disposable debris" test `ensure_worktree_present` uses
/// before reclaiming a leftover. A `NotFound` reports `false`: the
/// directory is already gone, so there is nothing to refuse and the
/// caller can no-op cleanly. Any *other* read failure (permissions, a
/// mid-walk IO error) reports `true`: a delete guard must never read an
/// unreadable directory as "empty" and green-light an `rm -rf`.
async fn directory_has_real_content(path: &Path) -> bool {
    let mut entries = match tokio::fs::read_dir(path).await {
        Ok(entries) => entries,
        Err(e) => return e.kind() != std::io::ErrorKind::NotFound,
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                if entry.file_name() != ".git" {
                    return true;
                }
            }
            Ok(None) => return false,
            Err(_) => return true,
        }
    }
}

async fn uncommitted(git: &dyn GitRunner, worktree: &Path) -> Option<bool> {
    let output = git
        .run(Some(worktree), &["status", "--porcelain"], &[])
        .await
        .ok()?;
    output.status.success().then_some(!output.stdout.is_empty())
}

/// `git rev-list --count <range>` in `worktree`. `None` when the
/// range doesn't resolve (missing ref, no upstream, not a repo).
async fn rev_list_count(git: &dyn GitRunner, worktree: &Path, range: &str) -> Option<u64> {
    let output = git
        .run(Some(worktree), &["rev-list", "--count", range], &[])
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

/// Does the worktree hold commits that never made it upstream?
///
/// Tiered, most precise first (#1253):
/// 1. `refs/remotes/origin/<branch>..HEAD` — the branch's OWN
///    remote-tracking ref. This tier outranks `@{u}` deliberately:
///    `git worktree add -B <new> refs/remotes/origin/<base>` (the
///    issue-spawn shape) auto-sets the new branch's upstream to the
///    BASE branch (`branch.autoSetupMerge` default), so `@{u}..HEAD`
///    counts the whole feature diff as "unpushed" even after every
///    commit reached `origin/<branch>`. Comparing against the branch's
///    own remote ref is immune — and when tracking IS the branch's own
///    counterpart (the PR shape `checkout_at` records), `@{u}` maps
///    through the refspec to this very ref, so the two tiers agree.
/// 2. [`branch_tip_on_remote`] — the branch's remote ref is gone, but
///    the tip is reachable from *some* remote-tracking ref, proving the
///    commits reached the remote (merged + branch auto-deleted
///    upstream): pushed.
/// 3. `@{u}..HEAD` — the configured upstream, for worktrees whose
///    branch the caller doesn't know (detached HEAD, callers without a
///    branch column). Needs the bare clone's `remote.origin.fetch`
///    refspec to resolve.
/// 4. Commits ahead of the remote's default branch (`origin/HEAD`,
///    then the bare clone's HEAD branch) — the coarse legacy fallback.
///    Only reached when the branch is genuinely unknown upstream, where
///    it gives the right answer for both extremes: a never-pushed
///    `lazybox/issue-N` counts its local commits, a stub sitting
///    exactly on the default counts zero.
///
/// If nothing resolves, report `true` — conservative: never call work
/// "pushed" without evidence.
///
/// `bare == None` means git doesn't even list this directory as a
/// worktree ("ghost on disk"); there's no branch state to lose, so
/// the legacy `false` stands and the Untracked cleanup path keeps
/// working.
async fn unpushed(
    git: &dyn GitRunner,
    worktree: &Path,
    bare: Option<&Path>,
    branch: Option<&str>,
) -> bool {
    if let (Some(bare), Some(branch)) = (bare, branch) {
        let remote_ref = format!("refs/remotes/origin/{branch}");
        if ref_exists(git, bare, &remote_ref).await {
            if let Some(n) = rev_list_count(git, worktree, &format!("{remote_ref}..HEAD")).await {
                return n > 0;
            }
        } else if branch_tip_on_remote(git, bare, branch).await {
            return false;
        }
    }
    if let Some(n) = rev_list_count(git, worktree, "@{u}..HEAD").await {
        return n > 0;
    }
    let Some(bare) = bare else {
        return false;
    };
    if let Some(n) = rev_list_count(git, worktree, "refs/remotes/origin/HEAD..HEAD").await {
        return n > 0;
    }
    if let Some(default_branch) = bare_head_branch(git, bare).await {
        for base in [
            format!("refs/remotes/origin/{default_branch}..HEAD"),
            format!("refs/heads/{default_branch}..HEAD"),
        ] {
            if let Some(n) = rev_list_count(git, worktree, &base).await {
                return n > 0;
            }
        }
    }
    true
}

/// Whether `branch` has tracking configuration recorded in the bare
/// clone (`branch.<name>.remote`). Lazybox's `checkout_at` records it
/// when the worktree was created off a remote-tracking ref, so its
/// presence is evidence the branch genuinely existed upstream.
async fn branch_has_upstream_config(git: &dyn GitRunner, bare: &Path, branch: &str) -> bool {
    git.run(
        Some(bare),
        &["config", "--get", &format!("branch.{branch}.remote")],
        &[],
    )
    .await
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Whether the branch tip is reachable from ANY remote-tracking ref —
/// i.e. its commits made it to the remote at some point (covers
/// worktrees created before upstream config was recorded, as long as
/// the merge wasn't a squash).
async fn branch_tip_on_remote(git: &dyn GitRunner, bare: &Path, branch: &str) -> bool {
    let Ok(output) = git
        .run(
            Some(bare),
            &[
                "branch",
                "-r",
                "--contains",
                &format!("refs/heads/{branch}"),
            ],
            &[],
        )
        .await
    else {
        return false;
    };
    output.status.success() && !output.stdout.iter().all(|b| b.is_ascii_whitespace())
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::future::Future;
    use std::os::unix::process::ExitStatusExt;
    use std::pin::Pin;
    use std::process::{ExitStatus, Output};
    use std::sync::{Arc, Mutex};

    #[derive(Debug)]
    struct Call {
        cwd: Option<PathBuf>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    }

    struct ScriptedGit {
        bare: PathBuf,
        worktree: PathBuf,
        fail_status: bool,
        calls: Mutex<Vec<Call>>,
    }

    impl ScriptedGit {
        fn output(success: bool, stdout: impl Into<Vec<u8>>) -> Output {
            Output {
                status: ExitStatus::from_raw(if success { 0 } else { 1 << 8 }),
                stdout: stdout.into(),
                stderr: Vec::new(),
            }
        }
    }

    impl GitRunner for ScriptedGit {
        fn run<'a>(
            &'a self,
            cwd: Option<&'a Path>,
            args: &'a [&'a str],
            env: &'a [(String, String)],
        ) -> Pin<Box<dyn Future<Output = Result<Output, GitError>> + Send + 'a>> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Call {
                    cwd: cwd.map(Path::to_path_buf),
                    args: args.iter().map(|arg| (*arg).to_string()).collect(),
                    env: env.to_vec(),
                });
            let response = match args {
                ["worktree", "list", "--porcelain"] => Ok(Self::output(
                    true,
                    format!(
                        "worktree {}\nbare\n\nworktree {}\nHEAD 1234\n\
                         branch refs/heads/feature\n\n",
                        self.bare.display(),
                        self.worktree.display(),
                    ),
                )),
                ["show-ref", "--verify", "--quiet", "refs/heads/feature"] => {
                    Ok(Self::output(true, Vec::new()))
                }
                [
                    "show-ref",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/feature",
                ] => Ok(Self::output(false, Vec::new())),
                ["status", "--porcelain"] if self.fail_status => {
                    Err(GitError::Command("status probe failed".into()))
                }
                ["status", "--porcelain"] => Ok(Self::output(true, Vec::new())),
                ["rev-list", "--count", "@{u}..HEAD"] => Ok(Self::output(true, b"0\n".to_vec())),
                ["config", "--get", "branch.feature.remote"] => {
                    Ok(Self::output(true, b"origin\n".to_vec()))
                }
                _ => Err(GitError::Command(format!(
                    "unexpected scripted git command: {}",
                    args.join(" ")
                ))),
            };
            Box::pin(async move { response })
        }
    }

    #[derive(Default)]
    struct LocalRecordingGit {
        calls: Mutex<Vec<Vec<String>>>,
        fail_intent_add: bool,
    }

    impl GitRunner for LocalRecordingGit {
        fn run<'a>(
            &'a self,
            cwd: Option<&'a Path>,
            args: &'a [&'a str],
            env: &'a [(String, String)],
        ) -> Pin<Box<dyn Future<Output = Result<Output, GitError>> + Send + 'a>> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            if self.fail_intent_add && matches!(args, ["add", "-N", "-A", "--"]) {
                return Box::pin(async { Ok(ScriptedGit::output(false, Vec::new())) });
            }
            let mut command = std::process::Command::new("git");
            if let Some(cwd) = cwd {
                command.current_dir(cwd);
            }
            command
                .args(args)
                .envs(env.iter().map(|(key, value)| (key, value)));
            let output = command.output();
            Box::pin(async move { output.map_err(GitError::Io) })
        }
    }

    #[tokio::test]
    async fn inspection_decisions_run_through_the_injected_runner() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repos/acme/widget.git");
        let worktree = tmp.path().join("worktrees/session");
        std::fs::create_dir_all(&bare).expect("bare dir");
        std::fs::create_dir_all(&worktree).expect("worktree dir");
        // Mark the checkout as a live git worktree so the disk-only
        // liveness gate (`worktree_dir_ready`) lets the in-worktree
        // probes run through the injected runner. A `.git` *directory*
        // is the standalone-clone shape the gate accepts outright — the
        // scripted runner supplies (or fails) the status/rev-list answers.
        std::fs::create_dir_all(worktree.join(".git")).expect("worktree .git");
        let git = Arc::new(ScriptedGit {
            bare: bare.clone(),
            worktree: worktree.clone(),
            fail_status: false,
            calls: Mutex::new(Vec::new()),
        });
        let manager = WorktreeManager::new(tmp.path())
            .with_git_runner(Arc::clone(&git) as Arc<dyn GitRunner>);
        let tracked = [TrackedSession {
            session_id: "session".into(),
            worktree_path: worktree.clone(),
            is_stopped: false,
        }];

        let inspections = manager
            .inspect_worktrees(&tracked)
            .await
            .expect("scripted inspection");

        assert_eq!(inspections.len(), 1);
        assert!(inspections[0].status_verified);
        assert_eq!(
            inspections[0].reasons,
            vec![OrphanReason::BranchDeletedUpstream]
        );
        assert!(inspections[0].is_safe_to_delete);

        let calls = git.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert!(calls.iter().all(|call| call.env.is_empty()));
        assert!(calls.iter().all(|call| {
            call.cwd.as_deref() == Some(bare.as_path())
                || call.cwd.as_deref() == Some(worktree.as_path())
        }));
        let mut commands: Vec<_> = calls.iter().map(|call| call.args.join(" ")).collect();
        commands.sort();
        assert_eq!(
            commands,
            [
                // `unpushed` tier 2 (#1253): the branch's remote ref is
                // absent, so the tip-on-remote proof runs.
                "branch -r --contains refs/heads/feature",
                "config --get branch.feature.remote",
                "rev-list --count @{u}..HEAD",
                "show-ref --verify --quiet refs/heads/feature",
                // Twice: the inspector's remote-ref existence probe and
                // `unpushed` tier 1 (#1253) each check it.
                "show-ref --verify --quiet refs/remotes/origin/feature",
                "show-ref --verify --quiet refs/remotes/origin/feature",
                "status --porcelain",
                "worktree list --porcelain",
            ]
        );
    }

    #[tokio::test]
    async fn failed_cleanliness_probe_is_not_safe_to_delete() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repos/acme/widget.git");
        let worktree = tmp.path().join("worktrees/session");
        std::fs::create_dir_all(&bare).expect("bare dir");
        std::fs::create_dir_all(&worktree).expect("worktree dir");
        // Mark the checkout as a live git worktree so the disk-only
        // liveness gate (`worktree_dir_ready`) lets the in-worktree
        // probes run through the injected runner. A `.git` *directory*
        // is the standalone-clone shape the gate accepts outright — the
        // scripted runner supplies (or fails) the status/rev-list answers.
        std::fs::create_dir_all(worktree.join(".git")).expect("worktree .git");
        let git = Arc::new(ScriptedGit {
            bare: bare.clone(),
            worktree: worktree.clone(),
            fail_status: true,
            calls: Mutex::new(Vec::new()),
        });
        let manager = WorktreeManager::new(tmp.path())
            .with_git_runner(Arc::clone(&git) as Arc<dyn GitRunner>);

        let inspections = manager
            .inspect_worktrees(&[])
            .await
            .expect("scripted inspection");

        assert_eq!(inspections.len(), 1);
        assert!(!inspections[0].has_uncommitted_changes);
        assert!(!inspections[0].status_verified);
        assert!(!inspections[0].is_safe_to_delete);
        assert!(!worktree_is_pristine_with(git.as_ref(), &worktree, Some(&bare), None).await);
    }

    #[tokio::test]
    async fn worktree_diff_combines_staged_unstaged_and_untracked_changes() {
        fn git(repo: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(repo)
                .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.name", "Lazybox Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "lazybox@example.invalid"],
        );
        std::fs::write(tmp.path().join("tracked.txt"), "one\ntwo\n").expect("seed tracked");
        git(tmp.path(), &["add", "tracked.txt"]);
        git(tmp.path(), &["commit", "-qm", "seed"]);

        std::fs::write(tmp.path().join("tracked.txt"), "one\nstaged\n").expect("stage edit");
        git(tmp.path(), &["add", "tracked.txt"]);
        std::fs::write(tmp.path().join("tracked.txt"), "one\nworking\n").expect("working edit");
        std::fs::write(tmp.path().join("new.rs"), "fn added() {}\n").expect("untracked file");
        let staged_before = std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["diff", "--cached"])
            .output()
            .expect("read staged diff")
            .stdout;

        let diff = inspect_worktree_diff(tmp.path())
            .await
            .expect("inspect diff");
        let staged_after = std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["diff", "--cached"])
            .output()
            .expect("read staged diff")
            .stdout;

        assert!(!diff.truncated);
        assert_eq!(staged_after, staged_before);
        assert!(diff.status.iter().any(|line| line == "MM tracked.txt"));
        assert!(diff.status.iter().any(|line| line == "?? new.rs"));
        let tracked = diff
            .files
            .iter()
            .find(|file| file.path == "tracked.txt")
            .expect("tracked diff");
        assert!(
            tracked
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.kind == DiffLineKind::Addition && line.text == "+working")
        );
        let untracked = diff
            .files
            .iter()
            .find(|file| file.path == "new.rs")
            .expect("untracked diff");
        assert!(
            untracked
                .hunks
                .iter()
                .flat_map(|hunk| &hunk.lines)
                .any(|line| line.kind == DiffLineKind::Addition && line.text == "+fn added() {}")
        );
        assert!(diff.stat.iter().any(|line| line.contains("tracked.txt")));
        assert!(diff.stat.iter().any(|line| line.contains("new.rs")));
    }

    #[tokio::test]
    async fn worktree_diff_reads_files_before_the_first_commit() {
        fn git(repo: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(repo)
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        git(tmp.path(), &["init", "-q"]);
        std::fs::write(tmp.path().join("staged.txt"), "staged\n").expect("staged file");
        git(tmp.path(), &["add", "staged.txt"]);
        std::fs::write(tmp.path().join("staged.txt"), "working\n").expect("working edit");
        std::fs::write(tmp.path().join("new.txt"), "untracked\n").expect("untracked file");

        let diff = inspect_worktree_diff(tmp.path())
            .await
            .expect("inspect unborn diff");

        assert!(diff.status.iter().any(|line| line == "AM staged.txt"));
        assert!(diff.status.iter().any(|line| line == "?? new.txt"));
        assert!(diff.files.iter().any(|file| {
            file.path == "staged.txt"
                && file
                    .hunks
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .any(|line| line.text == "+working")
        }));
        assert!(diff.files.iter().any(|file| {
            file.path == "new.txt"
                && file
                    .hunks
                    .iter()
                    .flat_map(|hunk| &hunk.lines)
                    .any(|line| line.text == "+untracked")
        }));
    }

    #[tokio::test]
    async fn worktree_diff_decodes_quoted_and_space_separated_paths() {
        fn git(repo: &Path, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(repo)
                .args(["-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false"])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        git(tmp.path(), &["init", "-q"]);
        git(tmp.path(), &["config", "user.name", "Lazybox Test"]);
        git(
            tmp.path(),
            &["config", "user.email", "lazybox@example.invalid"],
        );
        std::fs::write(tmp.path().join("café.rs"), "old\n").expect("unicode path");
        std::fs::create_dir(tmp.path().join("foo b")).expect("space directory");
        std::fs::write(tmp.path().join("foo b/bar.rs"), "old\n").expect("space path");
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-qm", "seed"]);
        std::fs::write(tmp.path().join("café.rs"), "new\n").expect("unicode edit");
        std::fs::write(tmp.path().join("foo b/bar.rs"), "new\n").expect("space edit");

        let diff = inspect_worktree_diff(tmp.path())
            .await
            .expect("inspect paths");
        let paths = diff
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();

        assert!(paths.contains(&"café.rs"), "{paths:?}");
        assert!(paths.contains(&"foo b/bar.rs"), "{paths:?}");
    }

    #[tokio::test]
    async fn worktree_diff_batches_many_untracked_files_into_one_patch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let init = std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["init", "-q"])
            .output()
            .expect("init");
        assert!(init.status.success());
        for index in 0..25 {
            std::fs::write(
                tmp.path().join(format!("new-{index:02}.txt")),
                format!("file {index}\n"),
            )
            .expect("untracked file");
        }
        let git = LocalRecordingGit::default();

        let diff = inspect_worktree_diff_with(&git, tmp.path())
            .await
            .expect("inspect many files");
        let calls = git.calls.lock().unwrap_or_else(|error| error.into_inner());
        let patch_calls = calls
            .iter()
            .filter(|args| args.first().is_some_and(|arg| arg == "diff"))
            .count();

        assert_eq!(diff.files.len(), 25);
        assert_eq!(patch_calls, 2, "one patch and one stat command: {calls:?}");
        assert!(
            calls
                .iter()
                .all(|args| !args.iter().any(|arg| arg == "--no-index")),
            "per-file no-index commands must not be used: {calls:?}"
        );
    }

    #[tokio::test]
    async fn worktree_diff_propagates_failure_to_prepare_untracked_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let init = std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["init", "-q"])
            .output()
            .expect("init");
        assert!(init.status.success());
        std::fs::write(tmp.path().join("new.txt"), "new\n").expect("untracked file");
        let git = LocalRecordingGit {
            fail_intent_add: true,
            ..LocalRecordingGit::default()
        };

        let error = inspect_worktree_diff_with(&git, tmp.path())
            .await
            .expect_err("intent-to-add failure must fail inspection");

        assert!(matches!(error, GitError::Command(_)));
    }

    #[tokio::test]
    async fn worktree_diff_bounds_large_patch_output() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let init = std::process::Command::new("git")
            .current_dir(tmp.path())
            .args(["init", "-q"])
            .output()
            .expect("init");
        assert!(init.status.success());
        let line = format!("{}\n", "x".repeat(1000));
        std::fs::write(tmp.path().join("large.txt"), line.repeat(5_000)).expect("large file");

        let diff = inspect_worktree_diff(tmp.path())
            .await
            .expect("inspect large diff");
        let retained_patch_bytes = diff
            .files
            .iter()
            .map(|file| {
                file.path.len()
                    + file.headers.iter().map(String::len).sum::<usize>()
                    + file
                        .hunks
                        .iter()
                        .map(|hunk| {
                            hunk.header.len()
                                + hunk.lines.iter().map(|line| line.text.len()).sum::<usize>()
                        })
                        .sum::<usize>()
            })
            .sum::<usize>();

        assert!(diff.truncated);
        assert!(retained_patch_bytes <= DIFF_PATCH_BYTES + 1024);
    }
}
