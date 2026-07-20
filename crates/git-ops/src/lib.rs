//! # lazybox-git-ops
//!
//! Git worktree management. Maintains a base directory with bare clones,
//! creates worktrees per-branch for parallel work.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
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
    /// About to run the one-time bare-clone transfer (only fired on a
    /// real cold clone; a cached healthy bare clone is reused without
    /// this).
    Cloning,
    /// A raw stderr progress line from the in-flight clone transfer
    /// (`Receiving objects: 42% (1200/2900), 12.00 MiB | 1.20 MiB/s`),
    /// throttled to a few per second. Lets the caller surface
    /// bytes/percent under the otherwise-opaque cloning step instead
    /// of a bare spinner for a multi-hundred-MB repo.
    CloneProgress(String),
    /// About to refresh the remote-tracking ref.
    Fetching,
    /// The base-ref fetch failed (offline / auth / transient network),
    /// so the worktree was branched off a possibly-stale local ref
    /// instead of latest origin. Carries a short human note (`<sha>,
    /// <relative age>`-style, plus the fetch failure cause) so the
    /// caller can surface the degradation in the UI rather than
    /// burying it in a log warning (issue #320).
    BaseRefStale(String),
    /// Like [`Self::BaseRefStale`], but the degradation is not a blip:
    /// the bare clone hasn't successfully talked to origin in over a
    /// day (`PERSISTENT_STALE_AFTER`), so every recent worktree
    /// branched from an aging ref. Callers should escalate beyond the
    /// one-off provisioning-checklist note (issue #394).
    BaseRefStalePersistent(String),
    /// About to run `git worktree add`.
    AddingWorktree,
}

/// Sink the [`WorktreeManager`] calls at each [`CheckoutPhase`] boundary.
pub type ProgressSink = dyn Fn(CheckoutPhase) + Send + Sync;

/// Async source of a GitHub token for authenticated network git
/// operations (clone / fetch / remote set-head). Resolved lazily per
/// operation so a rotated token is picked up without rebuilding the
/// manager, and so callers can plug in their own credential chain —
/// git-ops itself stays auth-agnostic.
pub type GithubTokenSource =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

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
    github_token: Option<GithubTokenSource>,
}

impl WorktreeManager {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            progress: None,
            github_token: None,
        }
    }

    /// Attach a progress sink invoked at each [`CheckoutPhase`] boundary
    /// during provisioning. Builder-style so callers that don't care
    /// (tests, the inspector) ignore it entirely.
    pub fn with_progress(mut self, sink: Arc<ProgressSink>) -> Self {
        self.progress = Some(sink);
        self
    }

    /// Attach a GitHub token source. When it yields a token, network
    /// git operations rewrite `github.com` SSH remotes to HTTPS and
    /// authenticate with the token — the daemon usually has no usable
    /// SSH agent, so an SSH-origin bare clone would otherwise fail
    /// every base-ref refresh with `Permission denied (publickey)`
    /// while the GitHub API (same token) works fine (issue #394).
    pub fn with_github_token(mut self, source: GithubTokenSource) -> Self {
        self.github_token = Some(source);
        self
    }

    /// Extra child-process env for network git operations: the
    /// HTTPS-rewrite + auth-header config when a token resolves,
    /// empty (native SSH behavior, unchanged) otherwise.
    async fn network_env(&self) -> Vec<(String, String)> {
        match &self.github_token {
            Some(source) => match source().await {
                Some(token) if !token.is_empty() => github_auth_env(&token),
                _ => Vec::new(),
            },
            None => Vec::new(),
        }
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
    /// scheme, manual tampering) are deleted and re-cloned.
    ///
    /// The clone itself is `git init --bare` + a blobless fetch
    /// (`--filter=blob:none`), not `git clone --bare`: all commits and
    /// trees transfer up front (log / blame / merge-base / rebase in
    /// worktrees keep working) while file contents are fetched lazily
    /// at checkout time, so a multi-hundred-MB repo clones in seconds
    /// instead of minutes (issue #405). Because the staging dir is a
    /// valid repo from the first moment, a `.partial` left by an
    /// interrupted attempt is *resumed* — re-fetched into — rather
    /// than thrown away, so retries accumulate progress.
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
        // The URL a from-scratch attempt would use, captured before a
        // resumed staging repo can substitute its own origin below —
        // the escape hatch when that adopted origin turns out dead.
        let fresh_start_url = url.clone();
        let mut resuming = false;
        if partial.exists() {
            // Resume when the staging dir is a usable repo: it records
            // the remote the interrupted attempt was cloning (adopted
            // for the same rewritten-origin reasons as above), and
            // fetching into it keeps every object already transferred
            // instead of restarting from zero on each retry.
            match resumable_partial_origin(&partial).await {
                Some(prev) => {
                    url = prev;
                    resuming = true;
                    tracing::info!(
                        owner,
                        repo,
                        path = %partial.display(),
                        "resuming interrupted bare clone"
                    );
                }
                None => {
                    tracing::warn!(
                        owner,
                        repo,
                        path = %partial.display(),
                        "removing unusable partial clone before re-cloning"
                    );
                    tokio::fs::remove_dir_all(&partial).await?;
                }
            }
        }
        if !resuming {
            run_git(
                &["init", "--quiet", "--bare", &partial.to_string_lossy()],
                &[],
            )
            .await?;
            // `config remote.origin.url` rather than `remote add`: the
            // latter also writes a fetch refspec `git clone --bare`
            // never had, silently diverging new cache entries from old
            // ones (a plain `git fetch` in a worktree would start
            // materializing remote-tracking refs for every branch).
            run_git_in(&partial, &["config", "remote.origin.url", &url]).await?;
        }
        self.report(CheckoutPhase::Cloning);
        let auth = self.network_env().await;
        let progress = |line: &str| self.report(CheckoutPhase::CloneProgress(line.to_string()));
        // Blobless partial clone: `--filter=blob:none` transfers every
        // commit and tree but defers blobs until a checkout / diff
        // actually reads them. A server without filter support ignores
        // the flag (git warns and sends everything), so this degrades
        // to a full clone rather than failing.
        if let Err(e) = run_git_transfer(
            &partial,
            &[
                "fetch",
                "--progress",
                "--filter=blob:none",
                "origin",
                "+refs/heads/*:refs/heads/*",
            ],
            &auth,
            Some(&progress),
        )
        .await
        {
            // Nothing else ever discards a resumable `.partial`, so a
            // staging repo aimed at a remote a fresh attempt would not
            // use must not survive its own failure — it would wedge
            // every retry forever with no path back to the canonical
            // URL. When the origins match, resume and restart are
            // equivalent: keep the accumulated objects.
            if resuming && url != fresh_start_url {
                tracing::warn!(
                    owner,
                    repo,
                    path = %partial.display(),
                    adopted = %url,
                    canonical = %fresh_start_url,
                    "resumed clone failed against its adopted origin; \
                     discarding the partial so the next attempt re-clones \
                     from the canonical remote"
                );
                let _ = tokio::fs::remove_dir_all(&partial).await;
            }
            return Err(e);
        }
        set_head_to_remote_default(&partial, &auth).await?;
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
        // local ref. Note the fallback covers *refs* only: from a
        // blobless clone the worktree add below still needs origin
        // reachable to download file contents, so a fully offline
        // provision only succeeds when the tree's blobs are already
        // local (a legacy full clone, or a tree checked out before). `fetch_origin_ref` logs a warning so the
        // degradation isn't silent; a network/auth failure (as opposed
        // to a deleted remote branch) also surfaces in the provisioning
        // checklist via a `BaseRefStale` report (issue #320).
        self.report(CheckoutPhase::Fetching);
        let auth = self.network_env().await;
        if let Err(e) = fetch_origin_ref(&bare_path, owner, repo, branch, &auth).await
            && let Some(phase) = stale_base_phase(&bare_path, branch, &e, !auth.is_empty()).await
        {
            self.report(phase);
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
        // From a blobless clone, `worktree add` downloads the checked-
        // out tree's blobs on demand — a real network transfer, so it
        // gets the auth env and the transfer-class timeout instead of
        // the 30s in-repo cap.
        run_git_transfer(
            &bare_path,
            &[
                "worktree",
                "add",
                &wt_path.to_string_lossy(),
                "-B",
                branch,
                &start_point,
            ],
            &auth,
            None,
        )
        .await
        .map_err(explain_promisor_failure)?;

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
        // from whatever local ref we have, so a stale base never blocks
        // branching (issue #35) — but a failed refresh is surfaced in
        // the provisioning checklist via a `BaseRefStale` report so the
        // "branched off latest main" guarantee degrading to "branched
        // off a stale local ref" is visible, not buried in the log
        // (issue #320). The tolerance covers *refs* only: from a
        // blobless clone the worktree add below still needs origin
        // reachable for file contents, so a fully offline provision
        // only succeeds when the tree's blobs are already local (a
        // legacy full clone, or a tree checked out before).
        self.report(CheckoutPhase::Fetching);
        let auth = self.network_env().await;
        match fetch_origin_ref(&bare_path, owner, repo, base_branch, &auth).await {
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
            Err(e) => {
                if let Some(phase) =
                    stale_base_phase(&bare_path, base_branch, &e, !auth.is_empty()).await
                {
                    self.report(phase);
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
        // See `checkout_at`: blob download on demand from a blobless
        // clone makes this a network transfer.
        run_git_transfer(
            &bare_path,
            &[
                "worktree",
                "add",
                "-B",
                new_branch,
                &wt_path.to_string_lossy(),
                &start_point,
            ],
            &auth,
            None,
        )
        .await
        .map_err(explain_promisor_failure)?;

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
        let auth = self.network_env().await;
        let _ = run_git_in_env(
            &bare_path,
            &["remote", "set-head", "origin", "--auto"],
            &auth,
        )
        .await;

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

/// Whether an existing `.partial` staging directory can be fetched
/// into instead of discarded: it must be a bare repo with a configured
/// origin, whose URL is returned so the resume targets the remote the
/// interrupted attempt was actually cloning. Probe failures count as
/// not-resumable — unlike the final bare clone, deleting a staging dir
/// orphans nothing.
async fn resumable_partial_origin(partial: &Path) -> Option<String> {
    let is_bare = apply_git_env(
        Command::new("git")
            .current_dir(partial)
            .args(["rev-parse", "--is-bare-repository"]),
    )
    .output()
    .await
    .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
    .unwrap_or(false);
    if !is_bare {
        return None;
    }
    configured_origin_url(partial).await
}

/// Point the staged clone's HEAD at the remote's default branch.
/// `git init` leaves HEAD on the local `init.defaultBranch` name,
/// which only matches the remote's by coincidence — a mismatch would
/// leave HEAD dangling, which the bare-repo health gate reads as a
/// broken clone. Resolution order: the remote's advertised symref,
/// then common defaults, then any fetched head. A remote with zero
/// branches resolves nothing and keeps the init-time HEAD (the clone
/// stays unusable for worktrees, same as before).
async fn set_head_to_remote_default(
    partial: &Path,
    envs: &[(String, String)],
) -> Result<(), GitError> {
    // `ls-remote --symref origin HEAD` prints e.g.
    // "ref: refs/heads/main\tHEAD" when the server advertises it.
    let advertised = run_git_in_env(partial, &["ls-remote", "--symref", "origin", "HEAD"], envs)
        .await
        .ok()
        .and_then(|out| {
            out.lines().find_map(|l| {
                l.strip_prefix("ref: ")?
                    .split_whitespace()
                    .next()
                    .filter(|r| r.starts_with("refs/heads/"))
                    .map(str::to_string)
            })
        });
    let mut head = advertised;
    if head.is_none() {
        for guess in ["refs/heads/main", "refs/heads/master"] {
            if ref_exists(partial, guess).await {
                head = Some(guess.to_string());
                break;
            }
        }
    }
    if head.is_none() {
        head = run_git_in(
            partial,
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/heads/",
            ],
        )
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    }
    if let Some(head) = head {
        run_git_in(partial, &["symbolic-ref", "HEAD", &head]).await?;
    }
    Ok(())
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

/// Per-invocation git config, encoded as `GIT_CONFIG_*` environment
/// entries (git ≥ 2.31), that routes `github.com` remotes over HTTPS
/// with `token` as the credential:
/// - `url.<https>.insteadOf` rewrites both SSH URL forms at network
///   time, so existing SSH-origin bare clones work without rewriting
///   their stored remote (and keep using SSH in environments where
///   only SSH works and no token resolves);
/// - `http.<https>.extraheader` carries the token as a Basic auth
///   header, the same scheme `gh` and Actions use.
///
/// Everything is scoped to `github.com` — enterprise hosts, local
/// mirrors and `file://` origins are untouched. The token rides in
/// the child environment only, never argv, so it can't leak through
/// process listings or lazybox's own command logging.
fn github_auth_env(token: &str) -> Vec<(String, String)> {
    let basic = base64_std(format!("x-access-token:{token}").as_bytes());
    git_config_env(&[
        ("url.https://github.com/.insteadOf", "git@github.com:"),
        ("url.https://github.com/.insteadOf", "ssh://git@github.com/"),
        (
            "http.https://github.com/.extraheader",
            &format!("AUTHORIZATION: basic {basic}"),
        ),
    ])
}

/// Encode config `pairs` as the `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n`
/// / `GIT_CONFIG_VALUE_n` environment scheme. Repeated keys append,
/// like repeated lines in a config file (needed for multi-valued
/// `insteadOf`).
fn git_config_env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), pairs.len().to_string())];
    for (i, (key, value)) in pairs.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), (*key).to_string()));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), (*value).to_string()));
    }
    env
}

/// Standard-alphabet base64 with padding. Hand-rolled to keep this
/// crate dependency-free — one header value is the only use.
fn base64_std(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Fetch a single branch from origin into the bare clone, updating
/// `refs/remotes/origin/<branch>`. `envs` carries the per-invocation
/// auth config from [`WorktreeManager::network_env`]. On failure, log
/// a warning and return the error — callers decide whether to
/// propagate or fall back to a local ref. Centralized so both
/// `checkout_at` and `checkout_new_branch_at` get identical
/// diagnostics (issue #35).
async fn fetch_origin_ref(
    bare_path: &Path,
    owner: &str,
    repo: &str,
    branch: &str,
    envs: &[(String, String)],
) -> Result<(), GitError> {
    run_git_in_env(
        bare_path,
        &[
            "fetch",
            "origin",
            &format!("+{branch}:refs/remotes/origin/{branch}"),
        ],
        envs,
    )
    .await
    .map(|_| ())
    .inspect(|()| stamp_refresh_ok(bare_path))
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

/// Marker file recording the last SUCCESSFUL origin fetch. git's own
/// `FETCH_HEAD` can't serve here: a *failed* fetch still truncates and
/// touches it, so its mtime records attempts, not contact. Best-effort
/// (mtime is the datum; the epoch-seconds content is for humans
/// debugging a bare clone by hand).
const REFRESH_STAMP: &str = "lazybox-fetch-ok";

fn stamp_refresh_ok(bare: &Path) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(bare.join(REFRESH_STAMP), format!("{secs}\n"));
}

/// A base-ref refresh failure older than this is no longer a blip —
/// it gets reported as [`CheckoutPhase::BaseRefStalePersistent`] so
/// callers escalate it instead of re-showing a dismissable note.
const PERSISTENT_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Build the [`CheckoutPhase`] describing the local ref a worktree
/// will be branched from after an origin fetch failed — the commit
/// lazybox fell back to instead of latest origin, plus why the fetch
/// failed. Mirrors the `start_point` precedence used at branch time
/// (remote-tracking ref first, then the local head) so the note names
/// the commit actually checked out. Escalates to
/// [`CheckoutPhase::BaseRefStalePersistent`] when the clone hasn't
/// successfully refreshed in over [`PERSISTENT_STALE_AFTER`].
/// `None` when no usable ref exists (the checkout is about to error
/// anyway) or the describe probe fails. Best-effort, read-only diagnostics.
async fn stale_base_phase(
    bare_path: &Path,
    branch: &str,
    err: &GitError,
    authed: bool,
) -> Option<CheckoutPhase> {
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
    let mut note = format!("could not refresh {branch} — branched from local ref ({desc}); ");
    note.push_str(&fetch_failure_reason(err, authed));
    match last_refresh_age(bare_path) {
        Some(age) if age >= PERSISTENT_STALE_AFTER => {
            note.push_str(&format!(
                "; origin has not refreshed in {}",
                format_age(age)
            ));
            Some(CheckoutPhase::BaseRefStalePersistent(note))
        }
        _ => Some(CheckoutPhase::BaseRefStale(note)),
    }
}

/// One-line cause for a failed base-ref fetch, so the stale note says
/// *why* instead of a bare "could not refresh" (issue #394). A
/// publickey failure without a token gets the actionable hint — that
/// exact state means lazybox fell back to SSH it can't authenticate.
fn fetch_failure_reason(err: &GitError, authed: bool) -> String {
    let raw = match err {
        GitError::Command(stderr) => stderr.clone(),
        GitError::Io(e) => e.to_string(),
    };
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown error")
        .trim_start_matches("fatal:")
        .trim();
    let line: String = line.chars().take(120).collect();
    if !authed && line.contains("Permission denied") {
        format!(
            "fetch failed: {line} — lazybox cannot reach an SSH agent; \
             run `gh auth login` so it can fetch over HTTPS"
        )
    } else {
        format!("fetch failed: {line}")
    }
}

/// Time since the bare clone last successfully talked to origin: the
/// [`REFRESH_STAMP`]'s mtime, falling back to `HEAD`'s (written at
/// clone time, then never again in a bare clone) when no fetch has
/// succeeded since the stamp was introduced. `None` when neither is
/// readable.
fn last_refresh_age(bare: &Path) -> Option<std::time::Duration> {
    let mtime = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let t = mtime(bare.join(REFRESH_STAMP)).or_else(|| mtime(bare.join("HEAD")))?;
    std::time::SystemTime::now().duration_since(t).ok()
}

fn format_age(age: std::time::Duration) -> String {
    let hours = age.as_secs() / 3600;
    if hours >= 48 {
        format!("{} days", hours / 24)
    } else {
        format!("{hours} hours")
    }
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

/// A transfer-progress stderr line, as opposed to a ref listing,
/// remote banner, or fatal error. Gates both the `on_progress` sink in
/// [`run_git_transfer`] and the noise filter on its error tail.
fn is_transfer_progress(line: &str) -> bool {
    line.contains('%') || line.ends_with("done.")
}

/// A blobless clone materializes file contents through origin at
/// checkout time, so `worktree add` can fail on a *network* problem
/// even though every ref it needs is local. Reword git's opaque
/// promisor error ("could not fetch <oid> from promisor remote") into
/// the actual cause; anything else passes through untouched.
fn explain_promisor_failure(err: GitError) -> GitError {
    match err {
        GitError::Command(msg) if msg.contains("promisor remote") => GitError::Command(format!(
            "could not download file contents from origin — worktrees \
                 from a blobless clone need the remote reachable to \
                 populate files: {msg}"
        )),
        other => other,
    }
}

/// Run a long, network-heavy git transfer (the initial clone fetch, a
/// blob-materializing `worktree add`) with the same env hygiene as
/// [`run_git`] but stderr streamed instead of buffered: git rewrites
/// progress lines with `\r`, so the stream is split on both `\r` and
/// `\n`, progress-looking lines (`Receiving objects: 42% …`) are
/// forwarded to `on_progress` throttled to ~10/s, and a short tail is
/// kept for error reporting. Shares [`run_git`]'s generous wall-clock
/// cap — transfers are slow by nature but must stay finite;
/// `kill_on_drop` reaps a timed-out child. Callers hold the repo lock
/// throughout, so this cap is also how long a hung transfer can stall
/// every other operation on the same repo — adaptive/timeout tuning is
/// issue #403's territory.
async fn run_git_transfer(
    cwd: &Path,
    args: &[&str],
    envs: &[(String, String)],
    on_progress: Option<&(dyn Fn(&str) + Sync)>,
) -> Result<(), GitError> {
    use tokio::io::AsyncReadExt;
    const TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
    const PROGRESS_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
    const TAIL_LINES: usize = 8;
    let started = std::time::Instant::now();
    tracing::info!("git (in {}) {}", cwd.display(), args.join(" "));
    let mut child = apply_git_env(Command::new("git").current_dir(cwd).args(args))
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| GitError::Command("git transfer: no stderr pipe".into()))?;

    let drain = async {
        let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let mut push_tail = |text: String| {
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(text);
        };
        let mut line = Vec::new();
        let mut buf = [0u8; 8192];
        let mut last_emit: Option<std::time::Instant> = None;
        loop {
            let n = match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                if b != b'\r' && b != b'\n' {
                    line.push(b);
                    continue;
                }
                let text = String::from_utf8_lossy(&line).trim().to_string();
                line.clear();
                if text.is_empty() {
                    continue;
                }
                push_tail(text.clone());
                // Only transfer-progress lines reach the sink — ref
                // listings and remote banners stay in the log tail.
                if let Some(cb) = on_progress
                    && is_transfer_progress(&text)
                    && last_emit.is_none_or(|t| t.elapsed() >= PROGRESS_MIN_INTERVAL)
                {
                    last_emit = Some(std::time::Instant::now());
                    cb(&text);
                }
            }
        }
        let text = String::from_utf8_lossy(&line).trim().to_string();
        if !text.is_empty() {
            push_tail(text);
        }
        tail
    };

    let result = tokio::time::timeout(TRANSFER_TIMEOUT, async {
        let tail = drain.await;
        (tail, child.wait().await)
    })
    .await;
    let elapsed = started.elapsed();
    match result {
        Ok((_, Ok(status))) if status.success() => {
            tracing::info!(
                "git (in {}) {} ok ({elapsed:?})",
                cwd.display(),
                args.join(" ")
            );
            Ok(())
        }
        Ok((tail, Ok(_))) => {
            // Progress fragments in the tail bury the fatal line —
            // drop them from the surfaced error unless they're all
            // there is.
            let causes: Vec<String> = tail
                .iter()
                .filter(|l| !is_transfer_progress(l))
                .cloned()
                .collect();
            let lines = if causes.is_empty() {
                tail.into_iter().collect()
            } else {
                causes
            };
            let stderr_tail = lines.join("\n");
            tracing::error!(
                "git (in {}) {} failed ({elapsed:?}): {}",
                cwd.display(),
                args.join(" "),
                stderr_tail.trim()
            );
            Err(GitError::Command(stderr_tail))
        }
        Ok((_, Err(e))) => Err(e.into()),
        Err(_) => {
            tracing::error!(
                "git (in {}) {} TIMED OUT after {elapsed:?}",
                cwd.display(),
                args.join(" ")
            );
            Err(GitError::Command(format!(
                "`git {}` exceeded {}s wall-clock",
                args.join(" "),
                TRANSFER_TIMEOUT.as_secs()
            )))
        }
    }
}

async fn run_git(args: &[&str], envs: &[(String, String)]) -> Result<String, GitError> {
    // Wall-clock cap. `run_git` is the no-cwd variant (today only
    // `git init --bare` staging a clone); network transfers stream
    // through `run_git_transfer` instead. Still FINITE: a git process
    // wedged on a silent credential prompt would otherwise hang its
    // caller forever. `kill_on_drop` so a timed-out child is actually
    // reaped instead of running on in the background.
    const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);
    let started = std::time::Instant::now();
    tracing::info!("git {}", args.join(" "));
    let fut = apply_git_env(Command::new("git").args(args))
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
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
    run_git_in_env(cwd, args, &[]).await
}

async fn run_git_in_env(
    cwd: &Path,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<String, GitError> {
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
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
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
mod auth_env_tests {
    use super::*;

    /// RFC 4648 test vectors — the encoder is hand-rolled, so pin it
    /// to the spec rather than trusting it by inspection.
    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_std(input.as_bytes()), expected, "input {input:?}");
        }
    }

    /// The auth env must rewrite both github.com SSH URL forms to
    /// HTTPS and carry the token base64ed in a header value — never
    /// in plaintext anywhere in the environment.
    #[test]
    fn github_auth_env_shape() {
        let env = github_auth_env("sekret-token");
        let lookup = |key: &str| -> Vec<&str> {
            env.iter()
                .enumerate()
                .filter(|(i, (k, _))| {
                    k.starts_with("GIT_CONFIG_KEY_") && env[*i].1 == key && *i % 2 == 1 // keys sit at odd indices after COUNT
                })
                .map(|(i, _)| env[i + 1].1.as_str())
                .collect()
        };
        assert_eq!(env[0], ("GIT_CONFIG_COUNT".to_string(), "3".to_string()));
        let instead_of = lookup("url.https://github.com/.insteadOf");
        assert_eq!(instead_of, ["git@github.com:", "ssh://git@github.com/"]);
        let headers = lookup("http.https://github.com/.extraheader");
        assert_eq!(headers.len(), 1);
        assert!(headers[0].starts_with("AUTHORIZATION: basic "));
        assert!(
            !env.iter().any(|(_, v)| v.contains("sekret-token")),
            "raw token must never appear in the env values: {env:?}"
        );
    }

    /// The env pairs must actually reach the spawned git process:
    /// with an origin whose SSH-style URL can't resolve at all, an
    /// env-injected `insteadOf` rewrite to a local path makes the
    /// base-ref fetch succeed — the same mechanism that routes
    /// github.com remotes over HTTPS with the gh token (issue #394's
    /// acceptance shape: SSH unavailable, token path still fetches).
    #[tokio::test]
    async fn fetch_succeeds_via_env_injected_rewrite_when_ssh_url_is_dead() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(cwd)
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir src");
        git(&src, &["init", "-q", "-b", "main"]);
        git(&src, &["config", "user.email", "t@example.com"]);
        git(&src, &["config", "user.name", "t"]);
        git(&src, &["commit", "--allow-empty", "-q", "-m", "init"]);
        // Mirror the on-disk layout the rewrite must produce:
        // git@invalid.example:acme/widgets.git → <tmp>/hub/acme/widgets.git
        let hub_repo = tmp.path().join("hub").join("acme").join("widgets.git");
        std::fs::create_dir_all(hub_repo.parent().unwrap()).expect("mkdir hub");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                &src.to_string_lossy(),
                &hub_repo.to_string_lossy(),
            ],
        );
        let bare = tmp.path().join("bare.git");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                &src.to_string_lossy(),
                &bare.to_string_lossy(),
            ],
        );
        git(
            &bare,
            &[
                "remote",
                "set-url",
                "origin",
                "git@invalid.example:acme/widgets.git",
            ],
        );

        assert!(
            fetch_origin_ref(&bare, "acme", "widgets", "main", &[])
                .await
                .is_err(),
            "sanity: the dead SSH-style URL must not fetch on its own"
        );

        let hub_base = format!("{}/", tmp.path().join("hub").display());
        let envs =
            git_config_env(&[(&format!("url.{hub_base}.insteadOf"), "git@invalid.example:")]);
        fetch_origin_ref(&bare, "acme", "widgets", "main", &envs)
            .await
            .expect("env-injected rewrite makes the same fetch succeed");
        assert!(
            ref_exists(&bare, "refs/remotes/origin/main").await,
            "fetch updated the remote-tracking ref"
        );
    }

    /// A publickey failure with no token in play gets the actionable
    /// HTTPS hint; with a token (HTTPS path already taken) the raw
    /// cause is reported verbatim.
    #[test]
    fn fetch_failure_reason_classifies_publickey() {
        let err = GitError::Command(
            "git@github.com: Permission denied (publickey).\r\n\
             fatal: Could not read from remote repository.\n"
                .into(),
        );
        let unauthed = fetch_failure_reason(&err, false);
        assert!(
            unauthed.contains("Permission denied (publickey)"),
            "{unauthed}"
        );
        assert!(unauthed.contains("gh auth login"), "{unauthed}");
        let authed = fetch_failure_reason(&err, true);
        assert!(authed.contains("Permission denied (publickey)"), "{authed}");
        assert!(!authed.contains("gh auth login"), "{authed}");

        let other = GitError::Command("fatal: unable to access 'x': timed out\n".into());
        assert_eq!(
            fetch_failure_reason(&other, false),
            "fetch failed: unable to access 'x': timed out"
        );
    }

    #[test]
    fn format_age_hours_then_days() {
        use std::time::Duration;
        assert_eq!(format_age(Duration::from_secs(3 * 3600)), "3 hours");
        assert_eq!(format_age(Duration::from_secs(30 * 3600)), "30 hours");
        assert_eq!(format_age(Duration::from_secs(4 * 24 * 3600)), "4 days");
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

    /// Resume decision for a `.partial` staging dir: a bare repo with
    /// a configured origin resumes (adopting that origin); a directory
    /// of junk — or a repo that never got its origin — restarts from
    /// scratch. This is what keeps a retry from discarding an
    /// interrupted attempt's already-fetched objects.
    #[tokio::test]
    async fn partial_resumability_requires_bare_repo_with_origin() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let junk = tmp.path().join("junk.partial");
        std::fs::create_dir(&junk).expect("mkdir");
        std::fs::write(junk.join("half-written"), "x").expect("write");
        assert_eq!(resumable_partial_origin(&junk).await, None);

        let no_origin = tmp.path().join("no-origin.partial");
        git(tmp.path(), &["init", "-q", "--bare", "no-origin.partial"]);
        assert_eq!(resumable_partial_origin(&no_origin).await, None);

        let staged = tmp.path().join("staged.partial");
        git(tmp.path(), &["init", "-q", "--bare", "staged.partial"]);
        git(
            &staged,
            &["remote", "add", "origin", "git@github.com:acme/widgets.git"],
        );
        assert_eq!(
            resumable_partial_origin(&staged).await,
            Some("git@github.com:acme/widgets.git".to_string())
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
