//! # pilot-git-ops
//!
//! Git worktree management. Maintains a base directory with bare clones,
//! creates worktrees per-branch for parallel work.

use std::path::{Path, PathBuf};
use tokio::process::Command;

mod inspect;
pub use inspect::{OrphanReason, TrackedSession, WorktreeInspection};

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
/// `_pilot/scripts/<name>`. The user can then run it as
/// `./_pilot/scripts/<name>` from a shell pilot spawns in the
/// worktree, or wire `_pilot/scripts` onto `PATH` to call by name.
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
    /// Filename inside `_pilot/scripts/`. See `validate_script_name`
    /// for the accept rules.
    pub name: String,
    pub body: ScriptBody,
}

#[derive(Debug, Clone)]
pub enum ScriptBody {
    Inline(String),
    Linked(PathBuf),
}

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
}

impl WorktreeManager {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
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

    /// Default base dir: `<PILOT_HOME>/v2/` (default `~/.pilot/v2/`).
    ///
    /// v2-rooted so all of pilot's on-disk state — `state.db`, the
    /// bare-clone cache, every worktree — sits under one directory.
    /// One `rm -rf <PILOT_HOME>/v2/` wipes pilot completely.
    /// Profile-aware via `pilot_core::paths::state_root`.
    pub fn default_base() -> Self {
        Self::new(pilot_core::paths::state_root())
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

    /// Same as [`checkout`] but with an explicit target path. Used by
    /// pilot's session model where the worktree path is derived from a
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

        // Return early if worktree already exists. Idempotent — pilot
        // calls this on every session bring-up.
        if wt_path.exists() {
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

        // Ensure bare clone exists.
        if !bare_path.exists() {
            if let Some(parent) = bare_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let url = format!("git@github.com:{owner}/{repo}.git");
            run_git(&["clone", "--bare", &url, &bare_path.to_string_lossy()]).await?;
        }

        // Refresh the remote-tracking ref (not refs/heads/*: that
        // would collide with a worktree currently holding the same
        // branch). Common reasons fetch can fail and that we tolerate:
        // remote branch was deleted post-merge, offline, auth issue.
        // In all cases the start_point lookup below falls back to the
        // local ref. `fetch_origin_ref` logs a warning so the
        // degradation isn't silent.
        let _ = fetch_origin_ref(&bare_path, owner, repo, branch).await;

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
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

    /// Same as [`checkout_new_branch`] but with an explicit target path.
    /// Used by pilot's session model where the worktree path is derived
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

        if wt_path.exists() {
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

        if !bare_path.exists() {
            if let Some(parent) = bare_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let url = format!("git@github.com:{owner}/{repo}.git");
            run_git(&["clone", "--bare", &url, &bare_path.to_string_lossy()]).await?;
        }

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
        // criteria, worktree creation must not block on the network.
        if fetch_origin_ref(&bare_path, owner, repo, base_branch)
            .await
            .is_ok()
            && let Err(e) = run_git_in(
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

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

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

    /// Resolve the repo's default branch (e.g. `main`, `master`,
    /// `develop`) by consulting the bare clone's `origin/HEAD`
    /// symbolic ref. Falls back to fetching once if the ref isn't
    /// present locally yet. Used by the "spawn from issue" path
    /// where the task has no PR branch — we cut a fresh branch
    /// off the default.
    pub async fn default_branch(&self, owner: &str, repo: &str) -> Result<String, GitError> {
        let bare_path = self.bare_clone_path(owner, repo);

        if !bare_path.exists() {
            if let Some(parent) = bare_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let url = format!("git@github.com:{owner}/{repo}.git");
            run_git(&["clone", "--bare", &url, &bare_path.to_string_lossy()]).await?;
        }

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

    /// Materialize a list of [`Script`]s under `<worktree>/_pilot/scripts/`.
    /// Each entry becomes either a symlink (`ScriptBody::Linked`) or
    /// a freshly-written file (`ScriptBody::Inline`); both end up
    /// chmod 0o755 so the user can invoke them directly.
    ///
    /// Idempotent for inline scripts (re-run with matching content
    /// is a no-op; differing content rewrites). For linked scripts
    /// re-applying a matching symlink is a no-op; a conflicting one
    /// errors — same contract as [`apply_mounts`].
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
        let scripts_dir = worktree.path.join("_pilot").join("scripts");
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

    /// Remove a worktree.
    /// Move a worktree from `old` to `new`. Wraps `git worktree move`,
    /// which atomically renames the worktree directory and updates
    /// git's internal pointer in `<bare>/worktrees/<name>/gitdir`.
    /// Used by pilot's PR-attach migration: when a workspace gains a
    /// PR mid-flight, the slug changes from "fix-login" to
    /// "PR-1234-fix-login" and we need to relocate without reclone.
    ///
    /// `bare_path` is the bare repo (`<base>/repos/<owner>/<repo>.git`)
    /// the worktree belongs to — `git worktree move` operates from
    /// inside the bare clone's tree.
    pub async fn move_worktree(
        &self,
        bare_path: &Path,
        old: &Path,
        new: &Path,
    ) -> Result<(), GitError> {
        run_git_in(
            bare_path,
            &[
                "worktree",
                "move",
                &old.to_string_lossy(),
                &new.to_string_lossy(),
            ],
        )
        .await?;
        Ok(())
    }

    /// The bare-clone path for `owner/repo` under this manager's base.
    /// Public so callers (pilot-server) can pass it to `move_worktree`.
    pub fn bare_path(&self, owner: &str, repo: &str) -> PathBuf {
        self.bare_clone_path(owner, repo)
    }

    pub async fn remove(&self, owner: &str, repo: &str, branch: &str) -> Result<(), GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
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
                // the caller can do that pilot can't.
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

/// Apply pilot's standard env overrides to a `git` Command.
///
/// Sets:
/// - `GIT_TERMINAL_PROMPT=0` — without this, a locked SSH key or
///   HTTPS-without-auth prompts the user, but pilot's in alternate-
///   screen mode so the prompt is invisible and the subprocess
///   hangs forever, freezing whatever async task awaited it
///   (worktree migration, session restore, etc.). Disabling makes
///   git fail fast with a clean error.
/// - `GIT_FLUSH=1` — suppress git's progress bar so `.output()`
///   doesn't accumulate huge stderr buffers on slow clones.
///
/// Removes any inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`
/// / `GIT_COMMON_DIR`. Those override `current_dir(cwd)` silently — if
/// pilot is ever launched from inside another git worktree (or
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
    let started = std::time::Instant::now();
    tracing::info!("git {}", args.join(" "));
    let output = apply_git_env(Command::new("git").args(args))
        .output()
        .await?;
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

/// Reject script names that would escape `_pilot/scripts/`, name a
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
    let fut = apply_git_env(Command::new("git").current_dir(cwd).args(args)).output();
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
