//! # lazybox-git-ops
//!
//! Git worktree management. Maintains a base directory with bare clones,
//! creates worktrees per-branch for parallel work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::process::Command;

mod inspect;
pub use inspect::{
    DiscoveredCheckout, OrphanReason, TrackedSession, WorktreeInspection, scan_external_checkouts,
};

/// Process-wide per-repo serialization. Keyed by the bare-clone path:
/// two concurrent cold spawns for the same repo would otherwise race
/// `git clone --bare` into one directory, and concurrent fetch /
/// `worktree add` invocations can collide on ref locks inside the
/// shared bare clone. Every mutating `WorktreeManager` operation
/// acquires the repo's async mutex first; distinct repos proceed in
/// parallel.
///
/// The outer `std::sync::Mutex` only guards the map lookup/insert —
/// never held across an `.await`. The returned `Arc<tokio::Mutex>`
/// is what callers hold for the duration of the git work.
fn repo_lock(bare_path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(bare_path.to_path_buf())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git command failed: {0}")]
    Command(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A handle to a created worktree.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
}

/// Where `link_at` is relative to the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Resolve `link_at` inside the worktree's root. The link looks
    /// like part of the repo from a process running inside.
    Inside,
    /// Resolve `link_at` one level above the worktree, in the sibling
    /// space shared with every other worktree of every repo. Use for
    /// caches and other things genuinely shared across checkouts.
    Above,
}

/// One mount. `source` is an absolute path on the host; `link_at` is
/// the name/path we symlink it to, interpreted per `placement`.
#[derive(Debug, Clone)]
pub struct Mount {
    pub source: PathBuf,
    pub link_at: PathBuf,
    pub placement: Placement,
}

/// An executable script to materialize inside the worktree at
/// `_lazybox/scripts/<name>`. The user can then run it as
/// `./_lazybox/scripts/<name>` from a shell lazybox spawns in the
/// worktree, or wire `_lazybox/scripts` onto `PATH` to call by name.
///
/// Two source kinds — pick one per script:
/// - `Inline(body)` — the body is written verbatim. A `#!/usr/bin/env bash`
///   shebang is prepended if the body doesn't already start with one,
///   so the file is directly executable.
/// - `Linked(path)` — the path is symlinked into the worktree. Edits
///   to the source file flow through without re-running
///   `apply_scripts`. The source path must exist at apply time.
#[derive(Debug, Clone)]
pub struct Script {
    /// Filename inside `_lazybox/scripts/`. See `validate_script_name`
    /// for the accept rules.
    pub name: String,
    pub body: ScriptBody,
}

#[derive(Debug, Clone)]
pub enum ScriptBody {
    Inline(String),
    Linked(PathBuf),
}

/// Coarse phase boundary inside a (cold) worktree provision, reported
/// through [`WorktreeManager`]'s optional progress sink. Lets a caller
/// animate sub-progress during the otherwise-opaque clone instead of a
/// single spinner that jumps straight to done. git-ops stays free of any
/// wire types — the daemon maps these onto `lazybox_ipc::WorktreeStep`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutPhase {
    /// About to run `git clone --bare` (only fired on a real cold clone;
    /// a cached healthy bare clone is reused without this).
    Cloning,
    /// About to refresh the remote-tracking ref.
    Fetching,
    /// The base-ref fetch failed (offline / auth / transient network),
    /// so the worktree was branched off a possibly-stale local ref
    /// instead of latest origin. Carries a short human note (`<sha>,
    /// <relative age>`-style) so the caller can surface the degradation
    /// in the UI rather than burying it in a log warning (issue #320).
    BaseRefStale(String),
    /// About to run `git worktree add`.
    AddingWorktree,
}

/// Sink the [`WorktreeManager`] calls at each [`CheckoutPhase`] boundary.
pub type ProgressSink = dyn Fn(CheckoutPhase) + Send + Sync;

/// Manages git worktrees under a base directory.
///
/// Layout:
/// ```text
/// base_dir/
///   repos/
///     owner/repo.git          (bare clone)
///   worktrees/
///     owner-repo-branch/      (worktree checkout)
/// ```
pub struct WorktreeManager {
    base_dir: PathBuf,
    progress: Option<Arc<ProgressSink>>,
}

impl WorktreeManager {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            progress: None,
        }
    }

    /// Attach a progress sink invoked at each [`CheckoutPhase`] boundary
    /// during provisioning. Builder-style so callers that don't care
    /// (tests, the inspector) ignore it entirely.
    pub fn with_progress(mut self, sink: Arc<ProgressSink>) -> Self {
        self.progress = Some(sink);
        self
    }

    fn report(&self, phase: CheckoutPhase) {
        if let Some(sink) = &self.progress {
            sink(phase);
        }
    }

    /// The directory this manager was constructed against — typically
    /// `<state_root>/`, with `repos/` (bare clones) and `worktrees/`
    /// (per-task checkouts) as siblings underneath. Crate-private so
    /// the inspector can compose paths without leaking the layout to
    /// every downstream caller.
    pub(crate) fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Default base dir: `<LAZYBOX_HOME>/v2/` (default `~/.lazybox/v2/`).
    ///
    /// v2-rooted so all of lazybox's on-disk state — `state.db`, the
    /// bare-clone cache, every worktree — sits under one directory.
    /// One `rm -rf <LAZYBOX_HOME>/v2/` wipes lazybox completely.
    /// Profile-aware via `lazybox_core::paths::state_root`.
    pub fn default_base() -> Self {
        Self::new(lazybox_core::paths::state_root())
    }

    fn bare_clone_path(&self, owner: &str, repo: &str) -> PathBuf {
        self.base_dir
            .join("repos")
            .join(owner)
            .join(format!("{repo}.git"))
    }

    fn worktree_path(&self, owner: &str, repo: &str, branch: &str) -> PathBuf {
        let safe_branch = branch.replace('/', "-");
        self.base_dir
            .join("worktrees")
            .join(format!("{owner}-{repo}-{safe_branch}"))
    }

    /// Ensure a healthy bare clone exists at the canonical path,
    /// cloning if needed. Caller must hold the repo lock.
    ///
    /// Crash-safety: the clone lands in `<path>.partial` and is
    /// renamed into place only on success. A killed / timed-out
    /// clone therefore never leaves a half-populated directory at
    /// the final path — which used to poison the cache forever,
    /// because every later provision saw `exists() == true`, skipped
    /// the clone, and failed on the broken repo. Existing directories
    /// that fail validation (interrupted clones from before this
    /// scheme, manual tampering) are deleted and re-cloned; stale
    /// `.partial` leftovers are cleared before cloning.
    async fn ensure_bare_clone(&self, owner: &str, repo: &str) -> Result<PathBuf, GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        // Re-clone from the same remote the previous clone used when
        // its config survived (covers rewritten origins — enterprise
        // hosts, local mirrors); fall back to the canonical GitHub
        // URL otherwise.
        let mut url = format!("git@github.com:{owner}/{repo}.git");
        if bare_path.exists() {
            // Deleting the bare clone orphans EVERY worktree hanging
            // off it (their gitdir metadata lives under
            // `<bare>/worktrees/`), so only git itself may condemn it:
            // a probe that couldn't even run (git binary missing, spawn
            // failure, resource exhaustion) proves nothing about the
            // repo and must propagate as an error instead of nuking a
            // possibly-healthy cache.
            match bare_repo_health(&bare_path).await {
                Ok(true) => return Ok(bare_path),
                Ok(false) => {
                    // git ran and confirmed the repo is unusable
                    // (interrupted clone, tampering) — delete + reclone.
                }
                Err(e) => {
                    tracing::warn!(
                        owner,
                        repo,
                        path = %bare_path.display(),
                        error = %e,
                        "bare-clone health probe could not run — keeping the \
                         existing clone (NOT deleting it) and failing this provision"
                    );
                    return Err(e);
                }
            }
            if let Some(prev) = configured_origin_url(&bare_path).await {
                url = prev;
            }
            tracing::warn!(
                owner,
                repo,
                path = %bare_path.display(),
                "bare clone failed validation (interrupted clone?); deleting and re-cloning"
            );
            tokio::fs::remove_dir_all(&bare_path).await?;
        }
        if let Some(parent) = bare_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let partial = partial_clone_path(&bare_path);
        if partial.exists() {
            tracing::warn!(
                owner,
                repo,
                path = %partial.display(),
                "removing stale partial clone before re-cloning"
            );
            tokio::fs::remove_dir_all(&partial).await?;
        }
        self.report(CheckoutPhase::Cloning);
        run_git(&["clone", "--bare", &url, &partial.to_string_lossy()]).await?;
        tokio::fs::rename(&partial, &bare_path).await?;
        Ok(bare_path)
    }

    /// Ensure a bare clone exists, then create a worktree for the branch.
    /// Idempotent: returns existing worktree if already checked out.
    /// Picks the path for you (`<base>/worktrees/<owner>-<repo>-<branch>`).
    pub async fn checkout(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Worktree, GitError> {
        let wt_path = self.worktree_path(owner, repo, branch);
        self.checkout_at(&wt_path, owner, repo, branch).await
    }

    /// Same as [`Self::checkout`] but with an explicit target path. Used by
    /// lazybox's session model where the worktree path is derived from a
    /// stable session UUID — `<state_root>/worktrees/<uuid>` — and
    /// must never depend on owner/repo/branch (so renames + branch
    /// changes don't relocate the on-disk folder).
    pub async fn checkout_at(
        &self,
        wt_path: &Path,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Worktree, GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        let lock = repo_lock(&bare_path);
        let _guard = lock.lock().await;

        // Return early if a *valid* worktree already exists.
        // Idempotent — lazybox calls this on every session bring-up.
        // The directory must actually be a worktree of this bare
        // clone: an empty dir left behind by a failed provision used
        // to "succeed" here forever, dumping sessions into a non-git
        // folder.
        if wt_path.exists()
            && validate_worktree_dir(wt_path, &bare_path).await? == WorktreeDirState::Valid
        {
            let name = wt_path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| branch.to_string());
            return Ok(Worktree {
                name,
                path: wt_path.to_path_buf(),
                branch: branch.into(),
            });
        }

        // Ensure a healthy bare clone exists.
        let bare_path = self.ensure_bare_clone(owner, repo).await?;

        // Refresh the remote-tracking ref (not refs/heads/*: that
        // would collide with a worktree currently holding the same
        // branch). Common reasons fetch can fail and that we tolerate:
        // remote branch was deleted post-merge, offline, auth issue.
        // In all cases the start_point lookup below falls back to the
        // local ref. `fetch_origin_ref` logs a warning so the
        // degradation isn't silent; a network/auth failure (as opposed
        // to a deleted remote branch) also surfaces in the provisioning
        // checklist via a `BaseRefStale` report (issue #320).
        self.report(CheckoutPhase::Fetching);
        if fetch_origin_ref(&bare_path, owner, repo, branch)
            .await
            .is_err()
            && let Some(note) = stale_base_note(&bare_path, branch).await
        {
            self.report(CheckoutPhase::BaseRefStale(note));
        }

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        self.report(CheckoutPhase::AddingWorktree);
        // Prefer the fresh remote-tracking ref; fall back to the local
        // ref when the remote branch was deleted (e.g. auto-delete after
        // merge). Worst case, `-B` uses whichever commit we have.
        let start_point = if ref_exists(&bare_path, &format!("refs/remotes/origin/{branch}")).await
        {
            format!("refs/remotes/origin/{branch}")
        } else if ref_exists(&bare_path, &format!("refs/heads/{branch}")).await {
            format!("refs/heads/{branch}")
        } else {
            return Err(GitError::Command(format!(
                "branch '{branch}' not found locally or on origin"
            )));
        };
        run_git_in(
            &bare_path,
            &[
                "worktree",
                "add",
                &wt_path.to_string_lossy(),
                "-B",
                branch,
                &start_point,
            ],
        )
        .await?;

        // Record the upstream when we branched off the remote-tracking
        // ref. `git worktree add -B` doesn't set it, so without this
        // `@{u}` never resolves: unpushed-commit detection silently
        // degrades and the inspector can't tell a once-pushed branch
        // (whose remote ref later vanished — PR merged + auto-delete)
        // from a never-pushed local one. Best-effort: a failure here
        // must not fail the checkout.
        if start_point.starts_with("refs/remotes/origin/") {
            let _ = run_git_in(
                &bare_path,
                &["config", &format!("branch.{branch}.remote"), "origin"],
            )
            .await;
            let _ = run_git_in(
                &bare_path,
                &[
                    "config",
                    &format!("branch.{branch}.merge"),
                    &format!("refs/heads/{branch}"),
                ],
            )
            .await;
        }

        let name = wt_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| branch.to_string());
        Ok(Worktree {
            name,
            path: wt_path.to_path_buf(),
            branch: branch.into(),
        })
    }

    /// Create a worktree on a *new* branch off `base_branch`.
    /// Used when the user spins up a local task with no PR yet.
    /// Idempotent: returns the existing worktree if it's already there.
    pub async fn checkout_new_branch(
        &self,
        owner: &str,
        repo: &str,
        new_branch: &str,
        base_branch: &str,
    ) -> Result<Worktree, GitError> {
        let wt_path = self.worktree_path(owner, repo, new_branch);
        self.checkout_new_branch_at(&wt_path, owner, repo, new_branch, base_branch)
            .await
    }

    /// Same as [`Self::checkout_new_branch`] but with an explicit target path.
    /// Used by lazybox's session model where the worktree path is derived
    /// from a stable session UUID and must not depend on branch names
    /// (so a branch rename inside the worktree doesn't relocate the
    /// on-disk folder).
    pub async fn checkout_new_branch_at(
        &self,
        wt_path: &Path,
        owner: &str,
        repo: &str,
        new_branch: &str,
        base_branch: &str,
    ) -> Result<Worktree, GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        let lock = repo_lock(&bare_path);
        let _guard = lock.lock().await;

        // Same validation as `checkout_at`: only a real worktree of
        // this bare clone short-circuits; an empty leftover dir is
        // cleared and re-provisioned.
        if wt_path.exists()
            && validate_worktree_dir(wt_path, &bare_path).await? == WorktreeDirState::Valid
        {
            let name = wt_path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| new_branch.to_string());
            return Ok(Worktree {
                name,
                path: wt_path.to_path_buf(),
                branch: new_branch.into(),
            });
        }

        let bare_path = self.ensure_bare_clone(owner, repo).await?;

        // Refresh origin/<base_branch> AND force-update the bare
        // clone's local `refs/heads/<base_branch>` to match — without
        // this the local ref can be arbitrarily stale and offline-mode
        // worktrees start from old commits (issue #35).
        //
        // Done as fetch + update-ref (not a two-refspec fetch) because
        // git treats stacked `<src>:<dst>` pairs with the same source
        // ambiguously and fails to update the heads/ ref even when the
        // remote-tracking one succeeds.
        //
        // Force-update (no FF check) is safe: the bare clone has no
        // working tree of its own and never commits locally, so
        // `refs/heads/<base_branch>` is only ever a mirror of origin.
        // The new worktree will be on `new_branch`, not base_branch,
        // so updating the base ref can't collide with a checked-out
        // worktree.
        //
        // Tolerate fetch failure (offline / auth): warn and proceed
        // from whatever local ref we have. Per the issue's acceptance
        // criteria, worktree creation must not block on the network — but
        // a failed refresh is surfaced in the provisioning checklist via
        // a `BaseRefStale` report so the "branched off latest main"
        // guarantee degrading to "branched off a stale local ref" is
        // visible, not buried in the log (issue #320).
        self.report(CheckoutPhase::Fetching);
        match fetch_origin_ref(&bare_path, owner, repo, base_branch).await {
            Ok(()) => {
                if let Err(e) = run_git_in(
                    &bare_path,
                    &[
                        "update-ref",
                        &format!("refs/heads/{base_branch}"),
                        &format!("refs/remotes/origin/{base_branch}"),
                    ],
                )
                .await
                {
                    tracing::warn!(
                        owner,
                        repo,
                        base_branch,
                        error = %e,
                        "could not force-update local base branch to origin; remote-tracking ref still refreshed"
                    );
                }
            }
            Err(_) => {
                if let Some(note) = stale_base_note(&bare_path, base_branch).await {
                    self.report(CheckoutPhase::BaseRefStale(note));
                }
            }
        }

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        self.report(CheckoutPhase::AddingWorktree);
        let start_point =
            if ref_exists(&bare_path, &format!("refs/remotes/origin/{base_branch}")).await {
                format!("refs/remotes/origin/{base_branch}")
            } else if ref_exists(&bare_path, &format!("refs/heads/{base_branch}")).await {
                format!("refs/heads/{base_branch}")
            } else {
                return Err(GitError::Command(format!(
                    "base branch '{base_branch}' not found locally or on origin"
                )));
            };

        // `-B` (not `-b`) so a stale local branch with the same name —
        // left behind from a previous spawn that failed mid-mounts —
        // is reset to `start_point` rather than failing the worktree
        // add. Symptom this prevents: user presses `c` on an issue,
        // mount fails, fixes config, presses `c` again, get "branch
        // already exists" and the spawn falls through to empty dir.
        run_git_in(
            &bare_path,
            &[
                "worktree",
                "add",
                "-B",
                new_branch,
                &wt_path.to_string_lossy(),
                &start_point,
            ],
        )
        .await?;

        let name = wt_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| new_branch.to_string());
        Ok(Worktree {
            name,
            path: wt_path.to_path_buf(),
            branch: new_branch.into(),
        })
    }

    /// Initialize a *standalone* git repository at `wt_path`, checked
    /// out on `branch`. Unlike [`Self::checkout_new_branch_at`] there is
    /// no upstream to clone or fetch from — this is for workspaces with
    /// no `owner/repo` to track: a blank workspace under a local
    /// project, or a task from a repo-less source (Slack, some Linear
    /// tickets). The session still lands in a real git worktree on its
    /// own branch (`lazybox/<key>`) rather than a bare, non-git
    /// directory.
    ///
    /// Idempotent: an existing git repo at `wt_path` is left untouched
    /// and returned as-is, so repeated spawns on the same workspace
    /// never wipe the user's work.
    pub async fn init_standalone_at(
        &self,
        wt_path: &Path,
        branch: &str,
    ) -> Result<Worktree, GitError> {
        // handle_spawn's singleton guard already keeps two spawns from
        // racing one workspace, but the per-path lock keeps this method
        // correct on its own.
        let lock = repo_lock(wt_path);
        let _guard = lock.lock().await;

        let name = wt_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| branch.to_string());

        // Already a git repo (a prior standalone init) — reuse it.
        // Reading HEAD keeps the returned branch honest if the user
        // renamed it inside the worktree.
        if is_git_repo(wt_path).await {
            let current = run_git_in(wt_path, &["symbolic-ref", "--short", "HEAD"])
                .await
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| branch.to_string());
            return Ok(Worktree {
                name,
                path: wt_path.to_path_buf(),
                branch: current,
            });
        }

        self.report(CheckoutPhase::AddingWorktree);
        tokio::fs::create_dir_all(wt_path).await?;
        run_git_in(wt_path, &["init", "-q"]).await?;
        // Point HEAD at lazybox's branch as an unborn ref. `symbolic-ref`
        // works on every git version (no `git init -b` dependency) and
        // needs no commit — `git status` reads "On branch <branch>, no
        // commits yet" and the user's first commit lands there.
        run_git_in(
            wt_path,
            &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        )
        .await?;

        Ok(Worktree {
            name,
            path: wt_path.to_path_buf(),
            branch: branch.to_string(),
        })
    }

    /// Resolve the repo's default branch (e.g. `main`, `master`,
    /// `develop`) by consulting the bare clone's `origin/HEAD`
    /// symbolic ref. Falls back to fetching once if the ref isn't
    /// present locally yet. Used by the "spawn from issue" path
    /// where the task has no PR branch — we cut a fresh branch
    /// off the default.
    pub async fn default_branch(&self, owner: &str, repo: &str) -> Result<String, GitError> {
        let lock = repo_lock(&self.bare_clone_path(owner, repo));
        let _guard = lock.lock().await;
        let bare_path = self.ensure_bare_clone(owner, repo).await?;

        // Pull origin/HEAD if we don't already have it. Tolerate
        // failure — we still try the local symbolic-ref lookup
        // afterwards, and only fail if that also can't resolve.
        let _ = run_git_in(&bare_path, &["remote", "set-head", "origin", "--auto"]).await;

        let out = run_git_in(&bare_path, &["symbolic-ref", "refs/remotes/origin/HEAD"]).await;
        if let Ok(ref_str) = out {
            let trimmed = ref_str.trim();
            if let Some(branch) = trimmed.strip_prefix("refs/remotes/origin/") {
                return Ok(branch.to_string());
            }
        }

        // Last resort: probe common defaults before giving up.
        for guess in ["main", "master"] {
            if ref_exists(&bare_path, &format!("refs/remotes/origin/{guess}")).await
                || ref_exists(&bare_path, &format!("refs/heads/{guess}")).await
            {
                return Ok(guess.to_string());
            }
        }

        Err(GitError::Command(format!(
            "could not resolve default branch for {owner}/{repo}"
        )))
    }

    /// Apply configured mount points to a worktree. Each mount creates
    /// a symlink from `source` to `link_at`, where `link_at` is either
    /// a path relative to the worktree root (`Placement::Inside`) or
    /// one level above the worktree (`Placement::Above`).
    ///
    /// Why:
    /// - `Inside` is for things that should LOOK like they're part of
    ///   the repo: shared configs, test fixtures, credential dirs the
    ///   code inside the worktree can read.
    /// - `Above` is for things shared ACROSS all worktrees: a single
    ///   `node_modules`, a shared cargo target, a mounted doc set.
    ///
    /// Idempotent. If `link_at` already exists and points to the same
    /// `source`, the call is a no-op. If it exists but points elsewhere,
    /// we error — we won't silently replace the user's symlinks.
    ///
    /// Parent directories for `link_at` are created as needed.
    pub async fn apply_mounts(
        &self,
        worktree: &Worktree,
        mounts: &[Mount],
    ) -> Result<(), GitError> {
        for mount in mounts {
            let target = match mount.placement {
                Placement::Inside => worktree.path.join(&mount.link_at),
                Placement::Above => {
                    let parent = worktree.path.parent().ok_or_else(|| {
                        GitError::Command("worktree has no parent directory".into())
                    })?;
                    parent.join(&mount.link_at)
                }
            };

            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            // Idempotent path: if the link already points where we
            // want, nothing to do. If it points elsewhere, refuse.
            if target.exists() || target.is_symlink() {
                match tokio::fs::read_link(&target).await {
                    Ok(existing) if existing == mount.source => continue,
                    Ok(other) => {
                        return Err(GitError::Command(format!(
                            "mount {} already exists but points to {} (expected {})",
                            target.display(),
                            other.display(),
                            mount.source.display()
                        )));
                    }
                    Err(_) => {
                        return Err(GitError::Command(format!(
                            "mount target {} exists and is not a symlink",
                            target.display()
                        )));
                    }
                }
            }

            // Symlink. Use async-safe std::os path since tokio doesn't
            // expose a Unix-specific symlink helper.
            tokio::task::spawn_blocking({
                let source = mount.source.clone();
                let target = target.clone();
                move || {
                    #[cfg(unix)]
                    {
                        std::os::unix::fs::symlink(&source, &target)
                    }
                    #[cfg(not(unix))]
                    {
                        // v2.0 targets Unix; windows support is out of scope.
                        Err(std::io::Error::other("mount points require Unix symlinks"))
                    }
                }
            })
            .await
            .map_err(|e| GitError::Command(format!("symlink task: {e}")))?
            .map_err(|e| {
                GitError::Command(format!(
                    "symlink {} -> {}: {e}",
                    target.display(),
                    mount.source.display()
                ))
            })?;
        }
        Ok(())
    }

    /// Materialize a list of [`Script`]s under `<worktree>/_lazybox/scripts/`.
    /// Each entry becomes either a symlink (`ScriptBody::Linked`) or
    /// a freshly-written file (`ScriptBody::Inline`); both end up
    /// chmod 0o755 so the user can invoke them directly.
    ///
    /// Idempotent for inline scripts (re-run with matching content
    /// is a no-op; differing content rewrites). For linked scripts
    /// re-applying a matching symlink is a no-op; a conflicting one
    /// errors — same contract as [`Self::apply_mounts`].
    ///
    /// Returns the first failure (rest are skipped). Best-effort
    /// retry is the caller's job.
    pub async fn apply_scripts(
        &self,
        worktree: &Worktree,
        scripts: &[Script],
    ) -> Result<(), GitError> {
        if scripts.is_empty() {
            return Ok(());
        }
        let scripts_dir = worktree.path.join("_lazybox").join("scripts");
        tokio::fs::create_dir_all(&scripts_dir).await?;

        for script in scripts {
            validate_script_name(&script.name)?;
            let target = scripts_dir.join(&script.name);
            match &script.body {
                ScriptBody::Linked(source) => {
                    apply_linked_script(&target, source).await?;
                }
                ScriptBody::Inline(body) => {
                    apply_inline_script(&target, body).await?;
                }
            }
        }
        Ok(())
    }

    /// List all active worktrees.
    pub async fn list(&self) -> Result<Vec<Worktree>, GitError> {
        let wt_dir = self.base_dir.join("worktrees");
        let mut result = Vec::new();
        if !wt_dir.exists() {
            return Ok(result);
        }
        let mut entries = tokio::fs::read_dir(&wt_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                result.push(Worktree {
                    path: entry.path(),
                    branch: name
                        .rsplit_once('-')
                        .map(|(_, b)| b)
                        .unwrap_or(&name)
                        .into(),
                    name,
                });
            }
        }
        Ok(result)
    }

    /// The bare-clone path for `owner/repo` under this manager's base.
    /// Public so callers (lazybox-server) can locate a repo's bare clone
    /// — e.g. the merged-worktree cleanup inspector.
    pub fn bare_path(&self, owner: &str, repo: &str) -> PathBuf {
        self.bare_clone_path(owner, repo)
    }

    pub async fn remove(&self, owner: &str, repo: &str, branch: &str) -> Result<(), GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        let lock = repo_lock(&bare_path);
        let _guard = lock.lock().await;
        let wt_path = self.worktree_path(owner, repo, branch);
        if wt_path.exists() {
            run_git_in(
                &bare_path,
                &["worktree", "remove", &wt_path.to_string_lossy(), "--force"],
            )
            .await?;
        }
        Ok(())
    }

    /// Remove a worktree by its absolute path. Used when the
    /// caller knows the path but not the branch name (e.g. the
    /// `CleanWorktrees` admin op, which iterates session records
    /// where the `worktree_path` is authoritative). Falls back to
    /// `rm -rf` + `git worktree prune` when `git worktree remove`
    /// refuses — older git versions, or worktrees whose metadata
    /// got desynced from disk, can hit "is not a working tree"
    /// errors that we'd rather just power through than surface to
    /// the user.
    pub async fn remove_by_path(
        &self,
        bare_path: &Path,
        worktree_path: &Path,
    ) -> Result<(), GitError> {
        let lock = repo_lock(bare_path);
        let _guard = lock.lock().await;
        if worktree_path.exists() {
            let result = run_git_in(
                bare_path,
                &[
                    "worktree",
                    "remove",
                    &worktree_path.to_string_lossy(),
                    "--force",
                ],
            )
            .await;
            if result.is_err() {
                // Best-effort: nuke the dir, then prune so the
                // bare repo's `worktrees/` index drops the stale
                // entry. Errors here are swallowed — if `rm -rf`
                // fails (permissions, FS in use), there's nothing
                // the caller can do that lazybox can't.
                let _ = tokio::fs::remove_dir_all(worktree_path).await;
                let _ = run_git_in(bare_path, &["worktree", "prune"]).await;
            }
        } else {
            // Directory's already gone — just prune so git's
            // metadata catches up.
            let _ = run_git_in(bare_path, &["worktree", "prune"]).await;
        }
        Ok(())
    }
}

/// Sibling staging path for an in-flight bare clone:
/// `<bare>.partial`. The clone writes here and is renamed into place
/// atomically on success, so the canonical path either holds a
/// complete clone or nothing.
fn partial_clone_path(bare: &Path) -> PathBuf {
    let mut os = bare.as_os_str().to_os_string();
    os.push(".partial");
    PathBuf::from(os)
}

/// Validation gate for an existing bare-clone directory. An
/// interrupted clone (pre-`.partial` scheme, or a tampered dir) can
/// leave something `exists()` accepts but git can't use. Two probes:
/// `rev-parse --is-bare-repository` must print `true`, and `HEAD`
/// must resolve to a commit (a half-fetched clone has a HEAD symref
/// but no refs behind it).
///
/// Tri-state result — the distinction is load-bearing for the caller:
/// * `Ok(true)`  — git ran and the repo is usable.
/// * `Ok(false)` — git ran and CONFIRMED the repo is unusable; only
///   this verdict authorizes delete + reclone.
/// * `Err(_)`    — the probe itself couldn't run (git missing, spawn
///   failure). Says nothing about the repo; the caller must NOT
///   delete on it. Conflating this with `false` used to nuke a
///   healthy bare clone — orphaning every worktree whose gitdir
///   metadata lived under `<bare>/worktrees/` — whenever spawning
///   git hiccuped.
async fn bare_repo_health(bare: &Path) -> Result<bool, GitError> {
    // Quiet probes (no error-level logging): a failing probe is an
    // expected, recoverable state, not a git invocation bug. A probe
    // that can't SPAWN, however, is an environment error we surface.
    async fn probe(bare: &Path, args: &[&str]) -> Result<Option<String>, GitError> {
        let out = apply_git_env(Command::new("git").current_dir(bare).args(args))
            .output()
            .await
            .map_err(|e| {
                GitError::Command(format!(
                    "could not run `git {}` in {}: {e}",
                    args.join(" "),
                    bare.display()
                ))
            })?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned()))
    }
    match probe(bare, &["rev-parse", "--is-bare-repository"]).await? {
        Some(out) if out.trim() == "true" => {}
        _ => return Ok(false),
    }
    Ok(probe(bare, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await?
        .is_some())
}

/// Read `remote.origin.url` straight from the config file of a (possibly
/// broken) bare clone. Uses `git config --file` so it works even when
/// the directory is too damaged for normal repo commands.
async fn configured_origin_url(bare: &Path) -> Option<String> {
    let config = bare.join("config");
    let out = apply_git_env(Command::new("git").args([
        "config",
        "--file",
        &config.to_string_lossy(),
        "--get",
        "remote.origin.url",
    ]))
    .output()
    .await
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Verdict for a directory that already exists at a worktree path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorktreeDirState {
    /// Genuine worktree of the expected bare clone — reuse it.
    Valid,
    /// Not a worktree, but empty enough that it was safely removed;
    /// the caller should proceed with a fresh provision.
    Reprovision,
}

/// Decide whether an existing directory at `wt_path` is a real
/// worktree of `bare_path`. A worktree's `.git` is a *file* whose
/// `gitdir:` line points into `<bare>/worktrees/<name>`. Anything
/// else (empty dir from a failed provision fallback, a dir whose
/// `.git` points at some other repo, a plain folder) must not pass —
/// the old bare `exists()` check let every later spawn "succeed"
/// into a non-git directory.
///
/// Invalid + effectively empty (only a stray `.git` at most) → the
/// dir is removed and `Reprovision` returned. Invalid with real
/// content → loud error; we never delete user data.
async fn validate_worktree_dir(
    wt_path: &Path,
    bare_path: &Path,
) -> Result<WorktreeDirState, GitError> {
    let dot_git = wt_path.join(".git");
    if let Ok(contents) = tokio::fs::read_to_string(&dot_git).await {
        let gitdir = contents
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:"))
            .map(|p| PathBuf::from(p.trim()));
        if let Some(gitdir) = gitdir {
            let expected = canonical_or_self(&bare_path.join("worktrees"));
            if canonical_or_self(&gitdir).starts_with(&expected) {
                // The gitdir must actually EXIST: after the bare clone
                // (or just its `worktrees/` metadata) is deleted, the
                // `.git` file still points into it and this used to
                // report `Valid` — sessions then landed in a checkout
                // every git command fails in. A dangling target is not
                // a usable worktree; fall through to the content check
                // below (real content → loud refusal, empty-ish →
                // reprovision).
                if tokio::fs::metadata(&gitdir).await.is_ok() {
                    return Ok(WorktreeDirState::Valid);
                }
                tracing::warn!(
                    path = %wt_path.display(),
                    gitdir = %gitdir.display(),
                    "worktree .git points at a missing gitdir (bare clone deleted?) — not valid"
                );
            }
        }
    }
    // Not a worktree of our bare clone. Empty-ish dirs (at most a
    // stray `.git` entry) are leftovers from a failed provision —
    // clear and re-provision. Anything with real content is refused.
    let mut has_real_content = false;
    let mut entries = tokio::fs::read_dir(wt_path).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name() != ".git" {
            has_real_content = true;
            break;
        }
    }
    if has_real_content {
        return Err(GitError::Command(format!(
            "{} exists but is not a worktree of {} — refusing to reuse or overwrite it; \
             move the directory aside and retry",
            wt_path.display(),
            bare_path.display()
        )));
    }
    tracing::warn!(
        path = %wt_path.display(),
        "removing invalid empty worktree directory (failed earlier provision?) before re-provisioning"
    );
    tokio::fs::remove_dir_all(wt_path).await?;
    Ok(WorktreeDirState::Reprovision)
}

/// Canonicalize when possible (resolves macOS `/var` → `/private/var`
/// and friends), fall back to the literal path for non-existent ones.
fn canonical_or_self(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Fetch a single branch from origin into the bare clone, updating
/// `refs/remotes/origin/<branch>`. On failure, log a warning and
/// return the error — callers decide whether to propagate or fall
/// back to a local ref. Centralized so both `checkout_at` and
/// `checkout_new_branch_at` get identical diagnostics (issue #35).
async fn fetch_origin_ref(
    bare_path: &Path,
    owner: &str,
    repo: &str,
    branch: &str,
) -> Result<(), GitError> {
    run_git_in(
        bare_path,
        &[
            "fetch",
            "origin",
            &format!("+{branch}:refs/remotes/origin/{branch}"),
        ],
    )
    .await
    .map(|_| ())
    .inspect_err(|e| {
        tracing::warn!(
            owner,
            repo,
            branch,
            error = %e,
            "could not fetch branch from origin; falling back to local ref"
        );
    })
}

/// Build a human-readable note describing the local ref a worktree will
/// be branched from after an origin fetch failed — the commit lazybox
/// fell back to instead of latest origin. Mirrors the `start_point`
/// precedence used at branch time (remote-tracking ref first, then the
/// local head) so the note names the commit actually checked out.
/// `None` when no usable ref exists (the checkout is about to error
/// anyway) or the describe probe fails. Best-effort, read-only diagnostics.
async fn stale_base_note(bare_path: &Path, branch: &str) -> Option<String> {
    let start = if ref_exists(bare_path, &format!("refs/remotes/origin/{branch}")).await {
        format!("refs/remotes/origin/{branch}")
    } else if ref_exists(bare_path, &format!("refs/heads/{branch}")).await {
        format!("refs/heads/{branch}")
    } else {
        return None;
    };
    // `%h <short sha>` + `%cr <committer date, relative>` → e.g.
    // "a1b2c3d, 3 days ago". One `git show` for both fields.
    let desc = run_git_in(bare_path, &["show", "-s", "--format=%h, %cr", &start])
        .await
        .ok()?;
    let desc = desc.trim();
    if desc.is_empty() {
        return None;
    }
    Some(format!(
        "could not refresh {branch} — branched from local ref ({desc})"
    ))
}

/// Whether `path` is the root of a git repository. Cheap: probes for
/// the `.git` entry, which a standalone repo has as a directory and a
/// linked worktree as a file.
async fn is_git_repo(path: &Path) -> bool {
    tokio::fs::metadata(path.join(".git")).await.is_ok()
}

/// Cheap existence check for a git ref. Uses `show-ref --verify --quiet`;
/// exit 0 = ref exists, non-zero = missing or ambiguous.
async fn ref_exists(bare_path: &Path, ref_name: &str) -> bool {
    Command::new("git")
        .current_dir(bare_path)
        .args(["show-ref", "--verify", "--quiet", ref_name])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Apply lazybox's standard env overrides to a `git` Command.
///
/// Sets:
/// - `GIT_TERMINAL_PROMPT=0` — without this, a locked SSH key or
///   HTTPS-without-auth prompts the user, but lazybox's in alternate-
///   screen mode so the prompt is invisible and the subprocess
///   hangs forever, freezing whatever async task awaited it
///   (worktree migration, session restore, etc.). Disabling makes
///   git fail fast with a clean error.
/// - `GIT_FLUSH=1` — suppress git's progress bar so `.output()`
///   doesn't accumulate huge stderr buffers on slow clones.
///
/// Removes any inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`
/// / `GIT_COMMON_DIR`. Those override `current_dir(cwd)` silently — if
/// lazybox is ever launched from inside another git worktree (or
/// `cargo test` from one), the subprocess would operate on the
/// inherited repo instead of the bare clone we're targeting.
///
/// `pub(crate)` so the inspector module can apply the same hygienic
/// env to its read-only probes.
pub(crate) fn apply_git_env(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_FLUSH", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
}

async fn run_git(args: &[&str]) -> Result<String, GitError> {
    // Wall-clock cap. `run_git` is the no-cwd variant used for
    // `git clone --bare` — slow by nature (a big repo over a slow
    // link takes minutes), but it must still be FINITE: a clone
    // wedged on a dead network or a silent credential prompt would
    // otherwise hang its caller forever. 10 minutes is generous for
    // any real clone; `run_git_in` keeps its tighter 30s cap for
    // the cheap in-repo operations. `kill_on_drop` so the timed-out
    // child is actually reaped instead of cloning on in the
    // background.
    const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
    let started = std::time::Instant::now();
    tracing::info!("git {}", args.join(" "));
    let fut = apply_git_env(Command::new("git").args(args))
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(GIT_TIMEOUT, fut).await {
        Ok(res) => res?,
        Err(_) => {
            let elapsed = started.elapsed();
            tracing::error!("git {} TIMED OUT after {elapsed:?}", args.join(" "));
            return Err(GitError::Command(format!(
                "`git {}` exceeded {}s wall-clock",
                args.join(" "),
                GIT_TIMEOUT.as_secs()
            )));
        }
    };
    let elapsed = started.elapsed();
    if output.status.success() {
        tracing::info!("git {} ok ({elapsed:?})", args.join(" "));
        Ok(String::from_utf8_lossy(&output.stdout).into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        tracing::error!(
            "git {} failed ({elapsed:?}): {}",
            args.join(" "),
            stderr.trim()
        );
        Err(GitError::Command(stderr))
    }
}

/// Reject script names that would escape `_lazybox/scripts/`, name a
/// hidden file, or run on Windows where the path separator differs.
/// Called by `apply_scripts` before any I/O so a bad name doesn't
/// leave a partial install behind.
fn validate_script_name(name: &str) -> Result<(), GitError> {
    if name.is_empty() {
        return Err(GitError::Command("script name must not be empty".into()));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(GitError::Command(format!(
            "script name {name:?} must not contain path separators"
        )));
    }
    if name == "." || name == ".." {
        return Err(GitError::Command(format!(
            "script name {name:?} is reserved"
        )));
    }
    if name.starts_with('.') {
        return Err(GitError::Command(format!(
            "script name {name:?} must not start with '.'"
        )));
    }
    Ok(())
}

/// Set the executable bit on `path`. Unix-only (the project is
/// Unix-first; a Windows port would replace this with a no-op or a
/// `.cmd` shim).
#[cfg(unix)]
fn chmod_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn chmod_executable(_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "script materialization requires Unix permissions",
    ))
}

/// Materialize a `Linked` script — symlink `target` → `source`.
/// Same idempotency / conflict rules as `apply_mounts`: identical
/// symlink is a no-op; different target errors. Source file must
/// exist at apply time.
async fn apply_linked_script(target: &Path, source: &Path) -> Result<(), GitError> {
    if !source.exists() {
        return Err(GitError::Command(format!(
            "script source does not exist: {}",
            source.display()
        )));
    }
    if target.exists() || target.is_symlink() {
        match tokio::fs::read_link(target).await {
            Ok(existing) if existing == source => return Ok(()),
            Ok(other) => {
                return Err(GitError::Command(format!(
                    "script {} already exists but points to {} (expected {})",
                    target.display(),
                    other.display(),
                    source.display()
                )));
            }
            Err(_) => {
                return Err(GitError::Command(format!(
                    "script target {} exists and is not a symlink",
                    target.display()
                )));
            }
        }
    }
    let source_owned = source.to_path_buf();
    let target_owned = target.to_path_buf();
    let source_for_err = source_owned.clone();
    let target_for_err = target_owned.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&source_owned, &target_owned)
        }
        #[cfg(not(unix))]
        {
            Err(std::io::Error::other("scripts require Unix symlinks"))
        }
    })
    .await
    .map_err(|e| GitError::Command(format!("symlink task: {e}")))?
    .map_err(|e| {
        GitError::Command(format!(
            "symlink {} -> {}: {e}",
            target_for_err.display(),
            source_for_err.display()
        ))
    })
}

/// Materialize an `Inline` script — write `body` to `target` with
/// chmod 0o755. Prepends `#!/usr/bin/env bash` if the body doesn't
/// already start with a shebang so the file is directly executable.
///
/// Idempotent: if the file exists and content matches (after
/// shebang injection), no I/O happens beyond the read. If content
/// differs the file is rewritten — body changes propagate without
/// the caller having to detect them.
async fn apply_inline_script(target: &Path, body: &str) -> Result<(), GitError> {
    let final_body = if body.starts_with("#!") {
        body.to_string()
    } else {
        format!("#!/usr/bin/env bash\n{body}")
    };
    // Check if existing content matches — skip write to preserve
    // mtime (build systems sometimes key off it).
    if let Ok(existing) = tokio::fs::read_to_string(target).await
        && existing == final_body
    {
        // Still re-chmod in case the bit got cleared. Cheap.
        let p = target.to_path_buf();
        tokio::task::spawn_blocking(move || chmod_executable(&p))
            .await
            .map_err(|e| GitError::Command(format!("chmod task: {e}")))?
            .map_err(GitError::Io)?;
        return Ok(());
    }
    tokio::fs::write(target, &final_body).await?;
    let p = target.to_path_buf();
    tokio::task::spawn_blocking(move || chmod_executable(&p))
        .await
        .map_err(|e| GitError::Command(format!("chmod task: {e}")))?
        .map_err(GitError::Io)?;
    Ok(())
}

async fn run_git_in(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    // Wall-clock cap on every git invocation. Without this, a single
    // hung `git worktree move` (waiting on credentials, an fs lock,
    // a stalled network connection to the remote) wedged the daemon
    // poll loop forever — the symptom was "poll succeeded" logged
    // but no `tick #N done`, no further polls, no panic. 30s is long
    // enough that a real `git fetch` over a slow network can still
    // complete; short enough that a hung process surfaces as an
    // error rather than silent paralysis.
    const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let started = std::time::Instant::now();
    tracing::info!("git (in {}) {}", cwd.display(), args.join(" "));
    let fut = apply_git_env(Command::new("git").current_dir(cwd).args(args))
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(GIT_TIMEOUT, fut).await {
        Ok(res) => res?,
        Err(_) => {
            let elapsed = started.elapsed();
            tracing::error!(
                "git (in {}) {} TIMED OUT after {elapsed:?}",
                cwd.display(),
                args.join(" ")
            );
            return Err(GitError::Command(format!(
                "`git {}` exceeded {}s wall-clock",
                args.join(" "),
                GIT_TIMEOUT.as_secs()
            )));
        }
    };
    let elapsed = started.elapsed();
    if output.status.success() {
        tracing::info!(
            "git (in {}) {} ok ({elapsed:?})",
            cwd.display(),
            args.join(" ")
        );
        Ok(String::from_utf8_lossy(&output.stdout).into())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        tracing::error!(
            "git (in {}) {} failed ({elapsed:?}): {}",
            cwd.display(),
            args.join(" "),
            stderr.trim()
        );
        Err(GitError::Command(stderr))
    }
}

#[cfg(test)]
mod health_probe_tests {
    use super::*;

    /// Test-side git runner (std, blocking) for fixture setup only.
    fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-c")
            .arg("user.email=test@example.com")
            .arg("-c")
            .arg("user.name=test")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("tag.gpgsign=false")
            .current_dir(cwd)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git must be runnable in tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `<tmp>/src` repo with one committed file, cloned to
    /// `<tmp>/bare.git`. Returns (tmp guard, bare path).
    fn local_bare_clone() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir src");
        git(&src, &["init", "-q"]);
        std::fs::write(src.join("f.txt"), "content\n").expect("write f.txt");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "init"]);
        git(tmp.path(), &["clone", "-q", "--bare", "src", "bare.git"]);
        let bare = tmp.path().join("bare.git");
        (tmp, bare)
    }

    /// git ran and the repo is fine → `Ok(true)`.
    #[tokio::test]
    async fn healthy_bare_clone_probes_ok_true() {
        let (_tmp, bare) = local_bare_clone();
        assert!(bare_repo_health(&bare).await.unwrap());
    }

    /// git ran and CONFIRMED the directory is not a usable repo
    /// (interrupted-clone shape) → `Ok(false)` — the only verdict that
    /// authorizes delete + reclone.
    #[tokio::test]
    async fn non_repo_directory_probes_ok_false() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("not-a-repo.git");
        std::fs::create_dir(&dir).expect("mkdir");
        assert!(!bare_repo_health(&dir).await.unwrap());
    }

    /// Regression: the probe FAILING TO RUN (spawn error — here a cwd
    /// that isn't a directory, the same class as a missing git binary)
    /// must surface as `Err`, NOT as "repo corrupt". The old
    /// `output().await.ok()?` collapsed this into `false`, and
    /// `ensure_bare_clone` deleted a healthy bare clone over it,
    /// orphaning every worktree.
    #[tokio::test]
    async fn probe_spawn_failure_is_error_not_corruption() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("bare.git");
        std::fs::write(&file, "not a directory").expect("write file");
        assert!(
            bare_repo_health(&file).await.is_err(),
            "a probe that couldn't run must propagate an error, not condemn the repo"
        );
    }

    /// A real worktree of the bare clone validates `Valid`; after the
    /// bare clone's `worktrees/` metadata vanishes (bare deleted /
    /// re-cloned), the same directory must STOP reporting `Valid` —
    /// its `.git` gitdir pointer is dangling and every git command in
    /// it would fail. With real content present, validation refuses
    /// loudly instead of deleting user data.
    #[tokio::test]
    async fn dangling_gitdir_target_is_not_valid() {
        let (tmp, bare) = local_bare_clone();
        let wt = tmp.path().join("wt");
        git(
            &bare,
            &[
                "worktree",
                "add",
                wt.to_str().expect("utf8 path"),
                "-B",
                "wt-branch",
                "HEAD",
            ],
        );

        assert_eq!(
            validate_worktree_dir(&wt, &bare).await.unwrap(),
            WorktreeDirState::Valid,
            "sanity: a live worktree of this bare clone is Valid"
        );

        std::fs::remove_dir_all(bare.join("worktrees")).expect("nuke worktree metadata");

        let verdict = validate_worktree_dir(&wt, &bare).await;
        assert!(
            verdict.is_err(),
            "dangling gitdir + real content must refuse (got {verdict:?}), never report Valid"
        );
        assert!(
            wt.join("f.txt").exists(),
            "validation must not delete the user's files"
        );
    }
}
