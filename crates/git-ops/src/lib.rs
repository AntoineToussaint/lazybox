//! # lazybox-git-ops
//!
//! Git worktree management. Maintains a base directory with bare clones,
//! creates worktrees per-branch for parallel work.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Output;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub mod remote;

mod inspect;
pub use inspect::{
    DiffFile, DiffHunk, DiffLine, DiffLineKind, DiscoveredCheckout, OrphanReason, TrackedSession,
    WorktreeDiff, WorktreeInspection, describe_checkout_at, inspect_worktree_diff,
    scan_external_checkouts, worktree_is_pristine,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Why a managed worktree cannot be reclaimed automatically.
pub enum WorktreeReclaimBlocker {
    /// Git marks the worktree as locked.
    Locked,
    /// Tracked or untracked files differ from `HEAD`.
    UncommittedChanges,
    /// Ignored files would be deleted with the worktree.
    IgnoredFiles,
    /// The branch contains commits that are not available remotely.
    UnpushedCommits,
    /// A safety probe failed, so the checkout could not be proven disposable.
    SafetyCheckFailed,
}

impl std::fmt::Display for WorktreeReclaimBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Locked => "the checkout is locked",
            Self::UncommittedChanges => "the checkout has uncommitted changes",
            Self::IgnoredFiles => "the checkout contains ignored local files",
            Self::UnpushedCommits => "the checkout has unpushed commits",
            Self::SafetyCheckFailed => "the checkout could not be verified safely",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Result of attempting to reclaim a non-live branch holder.
pub enum WorktreeReclaimOutcome {
    /// Git removed the worktree while retaining its branch.
    Reclaimed,
    /// The path is not the exact managed worktree holding the requested branch.
    NotManaged,
    /// The worktree is managed but contains state that must be preserved.
    Blocked(WorktreeReclaimBlocker),
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git command failed: {0}")]
    Command(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "branch '{branch}' is already checked out at {} — refusing to take it from another \
         live worktree; join the live session that owns that checkout, or free an external \
         checkout before retrying",
        .holder.display()
    )]
    BranchHeldLive { branch: String, holder: PathBuf },
    #[error(
        "branch '{branch}' is held by the non-live managed worktree at {} — automatic \
         reclaim blocked because {blocker}; preserve or remove that checkout, then retry",
        .holder.display()
    )]
    BranchHeldManaged {
        branch: String,
        holder: PathBuf,
        blocker: WorktreeReclaimBlocker,
    },
    #[error(
        "worktree {} is checked out on branch '{actual}', not the requested branch '{expected}' — \
         refusing to reuse it; preserve or switch that checkout, then retry",
        .path.display()
    )]
    WorktreeBranchMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

/// Boxed async result returned by [`GitRunner`] operations.
pub type GitRunFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, GitError>> + Send + 'a>>;

/// Executes one bounded git command. A non-zero exit is returned in
/// [`Output`]; runner failures are reserved for spawn, wait, and timeout
/// errors so callers can interpret git's exit status for probes.
pub trait GitRunner: Send + Sync {
    /// Run `git` with an optional working directory and extra environment.
    fn run<'a>(
        &'a self,
        cwd: Option<&'a Path>,
        args: &'a [&'a str],
        env: &'a [(String, String)],
    ) -> GitRunFuture<'a, Output>;

    /// Run `git` while retaining at most `stdout_limit` bytes of stdout.
    /// The boolean reports that the command produced additional bytes.
    fn run_with_stdout_limit<'a>(
        &'a self,
        cwd: Option<&'a Path>,
        args: &'a [&'a str],
        env: &'a [(String, String)],
        stdout_limit: usize,
    ) -> GitRunFuture<'a, (Output, bool)> {
        Box::pin(async move {
            let mut output = self.run(cwd, args, env).await?;
            let truncated = output.stdout.len() > stdout_limit;
            output.stdout.truncate(stdout_limit);
            Ok((output, truncated))
        })
    }

    /// Run a network-heavy git command, optionally streaming progress.
    /// The default delegates to [`Self::run`]; runners that can stream
    /// progress override it.
    fn run_transfer<'a>(
        &'a self,
        cwd: &'a Path,
        args: &'a [&'a str],
        env: &'a [(String, String)],
        _on_progress: Option<&'a (dyn Fn(&str) + Sync)>,
    ) -> GitRunFuture<'a, ()> {
        Box::pin(async move {
            let output = self.run(Some(cwd), args, env).await?;
            if output.status.success() {
                Ok(())
            } else {
                Err(GitError::Command(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ))
            }
        })
    }
}

struct BoundedGitRunner;

impl GitRunner for BoundedGitRunner {
    fn run<'a>(
        &'a self,
        cwd: Option<&'a Path>,
        args: &'a [&'a str],
        env: &'a [(String, String)],
    ) -> GitRunFuture<'a, Output> {
        let timeout = if cwd.is_some() {
            GIT_TIMEOUT
        } else {
            GIT_NO_CWD_TIMEOUT
        };
        Box::pin(exec_git_bounded(cwd, args, env, timeout))
    }

    fn run_with_stdout_limit<'a>(
        &'a self,
        cwd: Option<&'a Path>,
        args: &'a [&'a str],
        env: &'a [(String, String)],
        stdout_limit: usize,
    ) -> GitRunFuture<'a, (Output, bool)> {
        let timeout = if cwd.is_some() {
            GIT_TIMEOUT
        } else {
            GIT_NO_CWD_TIMEOUT
        };
        Box::pin(exec_git_bounded_with_stdout_limit(
            cwd,
            args,
            env,
            timeout,
            stdout_limit,
        ))
    }

    fn run_transfer<'a>(
        &'a self,
        cwd: &'a Path,
        args: &'a [&'a str],
        env: &'a [(String, String)],
        on_progress: Option<&'a (dyn Fn(&str) + Sync)>,
    ) -> GitRunFuture<'a, ()> {
        Box::pin(run_git_transfer(cwd, args, env, on_progress))
    }
}

fn default_git_runner() -> &'static dyn GitRunner {
    static RUNNER: BoundedGitRunner = BoundedGitRunner;
    &RUNNER
}

/// A handle to a created worktree.
#[derive(Debug, Clone)]
pub struct Worktree {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
}

/// Result of a "track main" fast-forward sync ([`WorktreeManager::fast_forward_to_base`],
/// issue #535). Every variant is non-destructive: the two `Skipped`
/// cases leave the tree exactly as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackSyncOutcome {
    /// Worktree tip already contains `origin/<base>` — nothing to do.
    UpToDate,
    /// Worktree branch was fast-forwarded onto `origin/<base>`.
    FastForwarded,
    /// Behind `origin/<base>` but the tree has uncommitted changes;
    /// skipped so a `git merge` can't clobber in-progress work.
    SkippedDirty,
    /// Behind `origin/<base>` but the branch also carries local commits
    /// not on the base (diverged); a fast-forward is impossible, so
    /// skipped rather than rebasing/resetting the user's commits away.
    SkippedDiverged,
    /// HEAD is detached or a bisect / rebase / merge / cherry-pick is
    /// in progress in the worktree — advancing the tree under an
    /// operation the user (or an agent shell) is mid-way through would
    /// yank state out from under it, so the sync refuses to touch it.
    SkippedUnsafeState,
}

impl TrackSyncOutcome {
    /// Whether the worktree could not be brought up to date
    /// automatically — drives the sidebar's "behind" badge and the
    /// surfaced hint. `SkippedUnsafeState` counts: like `SkippedDirty`
    /// (also returned before the behind-count is known), the truthful
    /// signal is "not synced", never a silent "up to date".
    pub fn is_behind(self) -> bool {
        matches!(
            self,
            Self::SkippedDirty | Self::SkippedDiverged | Self::SkippedUnsafeState
        )
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartPoint {
    Origin,
    PullRequest(u64),
    Local,
    Missing,
}

fn resolve_start_point(
    has_origin: bool,
    pr: Option<u64>,
    pr_fetched: bool,
    local_exists: bool,
    local_is_ancestor: bool,
) -> StartPoint {
    if has_origin {
        StartPoint::Origin
    } else if let Some(pr) = pr
        && pr_fetched
        && (!local_exists || local_is_ancestor)
    {
        StartPoint::PullRequest(pr)
    } else if local_exists {
        StartPoint::Local
    } else {
        StartPoint::Missing
    }
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
    git: Arc<dyn GitRunner>,
}

impl WorktreeManager {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            progress: None,
            github_token: None,
            git: Arc::new(BoundedGitRunner),
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

    /// Replace the git command executor.
    pub fn with_git_runner(mut self, runner: Arc<dyn GitRunner>) -> Self {
        self.git = runner;
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

    pub(crate) fn git_runner(&self) -> &dyn GitRunner {
        self.git.as_ref()
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
            match bare_repo_health(self.git_runner(), &bare_path).await {
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
            if let Some(prev) = configured_origin_url(self.git_runner(), &bare_path).await {
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
            match resumable_partial_origin(self.git_runner(), &partial).await {
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
                self.git_runner(),
                &["init", "--quiet", "--bare", &partial.to_string_lossy()],
                &[],
            )
            .await?;
            // `config remote.origin.url` rather than `remote add`: the
            // latter also writes a fetch refspec `git clone --bare`
            // never had, silently diverging new cache entries from old
            // ones (a plain `git fetch` in a worktree would start
            // materializing remote-tracking refs for every branch).
            run_git_in(
                self.git_runner(),
                &partial,
                &["config", "remote.origin.url", &url],
            )
            .await?;
        }
        self.report(CheckoutPhase::Cloning);
        let auth = self.network_env().await;
        let progress = |line: &str| self.report(CheckoutPhase::CloneProgress(line.to_string()));
        // Blobless partial clone: `--filter=blob:none` transfers every
        // commit and tree but defers blobs until a checkout / diff
        // actually reads them. A server without filter support ignores
        // the flag (git warns and sends everything), so this degrades
        // to a full clone rather than failing.
        if let Err(e) = self
            .git_runner()
            .run_transfer(
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
        set_head_to_remote_default(self.git_runner(), &partial, &auth).await?;
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
        self.checkout_at(&wt_path, owner, repo, branch, None).await
    }

    /// Same as [`Self::checkout`] but with an explicit target path. Used by
    /// lazybox's session model where the worktree path is derived from a
    /// stable session UUID — `<state_root>/worktrees/<uuid>` — and
    /// must never depend on owner/repo/branch (so renames + branch
    /// changes don't relocate the on-disk folder).
    ///
    /// `pr_number` is the head PR's number when this checkout targets a
    /// pull request, and enables the `refs/pull/<N>/head` fallback for a
    /// head branch that isn't a plain branch on `origin` (a fork PR, or a
    /// PR whose head branch was deleted — issue #550). `None` for non-PR /
    /// scratch checkouts, which keeps the origin-branch fast path unchanged.
    pub async fn checkout_at(
        &self,
        wt_path: &Path,
        owner: &str,
        repo: &str,
        branch: &str,
        pr_number: Option<u64>,
    ) -> Result<Worktree, GitError> {
        self.checkout_at_mode(wt_path, owner, repo, branch, pr_number, false)
            .await
    }

    /// Provision the head as a *detached* checkout: a worktree that checks
    /// out the resolved head commit without taking the branch. Used when
    /// the head branch is already checked out in another live worktree —
    /// an agent thrashing branches across a single worktree (#721) — so
    /// the branch name is exclusively owned yet the PR still needs a
    /// workspace. Detaching sidesteps the collision entirely: the holder
    /// keeps sole ownership of the branch and its files untouched, while
    /// the workspace still lands on the PR's head. `pr_number` enables the
    /// `refs/pull/<N>/head` fallback exactly as [`Self::checkout_at`].
    pub async fn checkout_pr_head_detached(
        &self,
        wt_path: &Path,
        owner: &str,
        repo: &str,
        branch: &str,
        pr_number: Option<u64>,
    ) -> Result<Worktree, GitError> {
        self.checkout_at_mode(wt_path, owner, repo, branch, pr_number, true)
            .await
    }

    async fn checkout_at_mode(
        &self,
        wt_path: &Path,
        owner: &str,
        repo: &str,
        branch: &str,
        pr_number: Option<u64>,
        detached: bool,
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
            && validate_worktree_dir(self.git_runner(), wt_path, &bare_path).await?
                == WorktreeDirState::Valid
        {
            ensure_worktree_branch(wt_path, branch).await?;
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
        // PR head or the local ref. Note the fallback covers *refs* only:
        // from a blobless clone the worktree add below still needs origin
        // reachable to download file contents, so a fully offline
        // provision only succeeds when the tree's blobs are already
        // local (a legacy full clone, or a tree checked out before).
        // `fetch_origin_ref` logs a warning so the degradation isn't
        // silent; a network/auth failure (as opposed to a deleted remote
        // branch) also surfaces in the provisioning checklist via a
        // `BaseRefStale` report (issue #320) — but only when we actually
        // branch from a non-fresh ref (see the gated report below).
        self.report(CheckoutPhase::Fetching);
        let auth = self.network_env().await;
        let origin_fetch =
            fetch_origin_ref(self.git_runner(), &bare_path, owner, repo, branch, &auth).await;

        // Prefer the fresh remote-tracking ref. When the head branch
        // isn't a plain branch on `origin` — a fork PR (head lives on the
        // contributor's fork), or a PR whose head branch was deleted — fall
        // back to GitHub's `refs/pull/<N>/head`, which the base repo exposes
        // for every PR regardless of where the branch lives (issue #550).
        // Worst case, a clear error naming what couldn't be reached. `-B`
        // cuts a local branch at whichever commit we resolve.
        //
        // A local `refs/heads/<branch>` is only ever created here by a prior
        // `worktree add -B`, so it can hold committed-but-unpushed work
        // (lazybox-created PRs). Resetting it to the PR head is safe only
        // when it loses nothing — i.e. the local branch is already contained
        // in the fetched head. When they diverge (real unpushed commits, or
        // a force-recreated head), keep the local ref rather than dropping
        // commits.
        let local_ref = format!("refs/heads/{branch}");
        let local_exists = ref_exists_with(self.git_runner(), &bare_path, &local_ref).await;
        let has_origin = ref_exists_with(
            self.git_runner(),
            &bare_path,
            &format!("refs/remotes/origin/{branch}"),
        )
        .await;
        let pr_fetched = if !has_origin
            && let Some(pr) = pr_number
            && fetch_pull_head(self.git_runner(), &bare_path, owner, repo, pr, &auth)
                .await
                .is_ok()
        {
            ref_exists_with(
                self.git_runner(),
                &bare_path,
                &format!("refs/lazybox/pr/{pr}"),
            )
            .await
        } else {
            false
        };
        let local_is_ancestor = if let Some(pr) = pr_number
            && pr_fetched
            && local_exists
        {
            is_ancestor(
                self.git_runner(),
                &bare_path,
                &local_ref,
                &format!("refs/lazybox/pr/{pr}"),
            )
            .await
        } else {
            false
        };
        let start_point = match resolve_start_point(
            has_origin,
            pr_number,
            pr_fetched,
            local_exists,
            local_is_ancestor,
        ) {
            StartPoint::Origin => format!("refs/remotes/origin/{branch}"),
            StartPoint::PullRequest(pr) => format!("refs/lazybox/pr/{pr}"),
            StartPoint::Local => local_ref,
            StartPoint::Missing => {
                return Err(GitError::Command(pr_number.map_or_else(
                    || format!("branch '{branch}' not found locally or on origin"),
                    |pr| {
                        format!(
                            "branch '{branch}' not found locally or on origin, and its \
                         pull-request head (refs/pull/{pr}/head) could not be fetched"
                        )
                    },
                )));
            }
        };

        // Surface a stale-base warning only when the origin refresh failed
        // AND we branched from a non-fresh ref (a stale origin mirror or the
        // local head). When the fresh `refs/pull/<N>/head` tier resolved the
        // checkout, there is nothing stale — reporting it would be a false
        // alarm naming a commit we didn't check out (issue #550 / #320).
        if let Err(e) = &origin_fetch
            && !start_point.starts_with("refs/lazybox/pr/")
            && let Some(phase) = stale_base_phase(
                self.git_runner(),
                &bare_path,
                branch,
                e,
                !auth.is_empty(),
                std::time::SystemTime::now(),
            )
            .await
        {
            self.report(phase);
        }

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        self.report(CheckoutPhase::AddingWorktree);
        // From a blobless clone, `worktree add` downloads the checked-
        // out tree's blobs on demand — a real network transfer, so it
        // gets the auth env and the transfer-class timeout instead of
        // the 30s in-repo cap. Resilient to a nested agent worktree
        // already holding the branch (issue #439).
        if detached {
            add_worktree_detached(self.git_runner(), &bare_path, wt_path, &start_point, &auth)
                .await?;
        } else {
            add_worktree_resilient(
                self.git_runner(),
                &bare_path,
                wt_path,
                branch,
                &start_point,
                &auth,
            )
            .await?;
        }

        // Record the upstream when we branched off the remote-tracking
        // ref. `git worktree add -B` doesn't set it, so without this
        // `@{u}` never resolves: unpushed-commit detection silently
        // degrades and the inspector can't tell a once-pushed branch
        // (whose remote ref later vanished — PR merged + auto-delete)
        // from a never-pushed local one. Best-effort: a failure here
        // must not fail the checkout.
        if !detached && start_point.starts_with("refs/remotes/origin/") {
            let _ = run_git_in(
                self.git_runner(),
                &bare_path,
                &["config", &format!("branch.{branch}.remote"), "origin"],
            )
            .await;
            let _ = run_git_in(
                self.git_runner(),
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
            && validate_worktree_dir(self.git_runner(), wt_path, &bare_path).await?
                == WorktreeDirState::Valid
        {
            ensure_worktree_branch(wt_path, new_branch).await?;
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
        match fetch_origin_ref(
            self.git_runner(),
            &bare_path,
            owner,
            repo,
            base_branch,
            &auth,
        )
        .await
        {
            Ok(()) => {
                if let Err(e) = run_git_in(
                    self.git_runner(),
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
                if let Some(phase) = stale_base_phase(
                    self.git_runner(),
                    &bare_path,
                    base_branch,
                    &e,
                    !auth.is_empty(),
                    std::time::SystemTime::now(),
                )
                .await
                {
                    self.report(phase);
                }
            }
        }

        if let Some(parent) = wt_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        self.report(CheckoutPhase::AddingWorktree);
        let start_point = if ref_exists_with(
            self.git_runner(),
            &bare_path,
            &format!("refs/remotes/origin/{base_branch}"),
        )
        .await
        {
            format!("refs/remotes/origin/{base_branch}")
        } else if ref_exists_with(
            self.git_runner(),
            &bare_path,
            &format!("refs/heads/{base_branch}"),
        )
        .await
        {
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
        // clone makes this a network transfer, and a nested agent
        // worktree already holding the branch is resolved rather than
        // fatal (issue #439).
        add_worktree_resilient(
            self.git_runner(),
            &bare_path,
            wt_path,
            new_branch,
            &start_point,
            &auth,
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
            let current = run_git_in(
                self.git_runner(),
                wt_path,
                &["symbolic-ref", "--short", "HEAD"],
            )
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
        run_git_in(self.git_runner(), wt_path, &["init", "-q"]).await?;
        // Point HEAD at lazybox's branch as an unborn ref. `symbolic-ref`
        // works on every git version (no `git init -b` dependency) and
        // needs no commit — `git status` reads "On branch <branch>, no
        // commits yet" and the user's first commit lands there.
        run_git_in(
            self.git_runner(),
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
            self.git_runner(),
            &bare_path,
            &["remote", "set-head", "origin", "--auto"],
            &auth,
        )
        .await;

        let out = run_git_in(
            self.git_runner(),
            &bare_path,
            &["symbolic-ref", "refs/remotes/origin/HEAD"],
        )
        .await;
        if let Ok(ref_str) = out {
            let trimmed = ref_str.trim();
            if let Some(branch) = trimmed.strip_prefix("refs/remotes/origin/") {
                return Ok(branch.to_string());
            }
        }

        // Last resort: probe common defaults before giving up. `develop`
        // and `trunk` cover the repos whose default is neither `main` nor
        // `master` yet still conventional — enough that a repo we *have*
        // a ref for locally resolves even when `origin/HEAD` never got
        // set (issue #557 B10).
        for guess in ["main", "master", "develop", "trunk"] {
            if ref_exists_with(
                self.git_runner(),
                &bare_path,
                &format!("refs/remotes/origin/{guess}"),
            )
            .await
                || ref_exists_with(
                    self.git_runner(),
                    &bare_path,
                    &format!("refs/heads/{guess}"),
                )
                .await
            {
                return Ok(guess.to_string());
            }
        }

        // Nothing resolved. Name the likely cause — a fresh bare clone
        // with no fetched refs is almost always offline or an auth/repo
        // problem — so the failure modal's guidance points somewhere.
        Err(GitError::Command(format!(
            "could not resolve default branch for {owner}/{repo} — the repo may be \
             unreachable (offline / auth) or use a non-standard default branch"
        )))
    }

    /// Keep a "track main" worktree (issue #535) fast-forwarded onto
    /// `origin/<base_branch>`. Fetches the base ref, then — only when the
    /// worktree is clean and its branch is a strict ancestor of the base
    /// — advances the checked-out branch to the base tip.
    ///
    /// **Fast-forward only, never destructive.** The two skip cases exist
    /// precisely because a scratch worktree is where in-progress work
    /// accumulates:
    /// - uncommitted changes → [`TrackSyncOutcome::SkippedDirty`];
    /// - local commits not on the base (diverged) →
    ///   [`TrackSyncOutcome::SkippedDiverged`].
    ///
    /// Neither runs `reset --hard` or `rebase`; the tree is left
    /// untouched and the caller surfaces a "behind main" hint instead.
    ///
    /// A failed fetch (offline / auth) is tolerated: the sync proceeds
    /// against whatever `origin/<base>` ref is already local, mirroring
    /// the offline tolerance the rest of provisioning has.
    pub async fn fast_forward_to_base(
        &self,
        wt_path: &Path,
        owner: &str,
        repo: &str,
        base_branch: &str,
    ) -> Result<TrackSyncOutcome, GitError> {
        let bare_path = self.bare_clone_path(owner, repo);
        let lock = repo_lock(&bare_path);
        let _guard = lock.lock().await;
        let bare_path = self.ensure_bare_clone(owner, repo).await?;

        // Refresh origin/<base>. Tolerate failure — fall back to the
        // last-known local remote-tracking ref, same as every other
        // network-optional path here.
        let auth = self.network_env().await;
        let _ = fetch_origin_ref(
            self.git_runner(),
            &bare_path,
            owner,
            repo,
            base_branch,
            &auth,
        )
        .await;

        let base_ref = format!("refs/remotes/origin/{base_branch}");
        if !ref_exists_with(self.git_runner(), &bare_path, &base_ref).await {
            return Err(GitError::Command(format!(
                "base ref '{base_ref}' not found for {owner}/{repo}"
            )));
        }

        // Detached HEAD / bisect / rebase / merge in progress → never
        // touch it. The sweep runs lock-free against whatever shell or
        // agent is working inside the worktree; `merge --ff-only` on a
        // detached HEAD would happily move HEAD (without advancing the
        // branch), pulling the checkout out from under a bisect or a
        // plain `git checkout <sha>` inspection. Checked before the
        // dirty probe because a mid-operation tree can be status-clean
        // (a clean detached HEAD, a bisect at a clean step).
        if let Some(reason) = worktree_unsafe_to_advance(self.git_runner(), wt_path).await? {
            tracing::debug!(
                worktree = %wt_path.display(),
                reason,
                "track-main: refusing fast-forward (unsafe HEAD state)"
            );
            return Ok(TrackSyncOutcome::SkippedUnsafeState);
        }

        // Tracked-file changes → never touch it: a fast-forward `git
        // merge` would still refuse, but checking first lets us report
        // *why* rather than surfacing a merge error.
        //
        // `--untracked-files=no` is deliberate: a scratch worktree is
        // where untracked debris (build output, logs, notes) piles up,
        // and counting it as dirty would leave the worktree permanently
        // "behind" and never synced — defeating the feature for its main
        // use case. A fast-forward can't clobber untracked files unless
        // it needs to *write over* one, and git's own `merge --ff-only`
        // refuses exactly that case (surfaced below as an `Err` that
        // leaves the tree untouched), so ignoring untracked files here is
        // safe.
        let status = run_git_in(
            self.git_runner(),
            wt_path,
            &["status", "--porcelain", "--untracked-files=no"],
        )
        .await?;
        if !status.trim().is_empty() {
            return Ok(TrackSyncOutcome::SkippedDirty);
        }

        // Behind count: commits in the base not yet in HEAD. Zero means
        // the base is already an ancestor of HEAD (up to date, or HEAD is
        // ahead with local work — either way, not behind).
        let behind = count_commits(self.git_runner(), wt_path, &format!("HEAD..{base_ref}")).await;
        if behind == 0 {
            return Ok(TrackSyncOutcome::UpToDate);
        }
        // Ahead count: local commits not on the base. Behind AND ahead =
        // diverged, so a fast-forward is impossible.
        let ahead = count_commits(self.git_runner(), wt_path, &format!("{base_ref}..HEAD")).await;
        if ahead > 0 {
            return Ok(TrackSyncOutcome::SkippedDiverged);
        }

        run_git_in(
            self.git_runner(),
            wt_path,
            &["merge", "--ff-only", &base_ref],
        )
        .await?;
        Ok(TrackSyncOutcome::FastForwarded)
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
                self.git_runner(),
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
                self.git_runner(),
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
                let _ = run_git_in(self.git_runner(), bare_path, &["worktree", "prune"]).await;
            }
        } else {
            // Directory's already gone — just prune so git's
            // metadata catches up.
            let _ = run_git_in(self.git_runner(), bare_path, &["worktree", "prune"]).await;
        }
        Ok(())
    }

    /// The branch a valid managed worktree at `wt_path` currently sits on,
    /// or `None` when the path is not a healthy worktree of this bare
    /// clone or is on a detached HEAD. Used by the provisioner to reuse a
    /// workspace's *own* prior worktree branch instead of re-deriving a
    /// colliding one from a mutated title (issue #787) — so a second
    /// attempt on the same issue adopts the branch already on disk rather
    /// than hard-failing `BranchMismatch` against it.
    pub async fn existing_worktree_branch(
        &self,
        owner: &str,
        repo: &str,
        wt_path: &Path,
    ) -> Result<Option<String>, GitError> {
        if !wt_path.exists() {
            return Ok(None);
        }
        let bare_path = self.bare_clone_path(owner, repo);
        let lock = repo_lock(&bare_path);
        let _guard = lock.lock().await;
        if validate_worktree_dir(self.git_runner(), wt_path, &bare_path).await?
            != WorktreeDirState::Valid
        {
            return Ok(None);
        }
        current_worktree_branch(wt_path).await
    }

    /// Preserve a conflicting checkout by renaming it to a sibling
    /// `<path>.bak-<n>` and pruning the bare clone's now-dangling worktree
    /// registration so the branch it held is freed for re-provisioning
    /// (issue #787's in-modal "recreate" recovery). The rename keeps any
    /// uncommitted work recoverable rather than deleting it. Returns the
    /// backup path, or `None` when nothing was at `worktree_path` (a prune
    /// still runs so a stale registration can't block the fresh add).
    pub async fn preserve_worktree_aside(
        &self,
        bare_path: &Path,
        worktree_path: &Path,
    ) -> Result<Option<PathBuf>, GitError> {
        let lock = repo_lock(bare_path);
        let _guard = lock.lock().await;
        if !worktree_path.exists() {
            let _ = run_git_in(self.git_runner(), bare_path, &["worktree", "prune"]).await;
            return Ok(None);
        }
        let backup = backup_sibling_path(worktree_path);
        tokio::fs::rename(worktree_path, &backup).await?;
        // The worktree dir is gone from its registered location, so a
        // prune drops the registration and releases its branch; the fresh
        // `git worktree add` that follows then sees a clean slate.
        let _ = run_git_in(self.git_runner(), bare_path, &["worktree", "prune"]).await;
        Ok(Some(backup))
    }
}

/// First free `<path>.bak-<n>` sibling for a preserved worktree. Counts
/// up so repeated recreates on the same stuck workspace never clobber an
/// earlier backup.
fn backup_sibling_path(path: &Path) -> PathBuf {
    let mut n = 1u32;
    loop {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".bak-{n}"));
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
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
async fn bare_repo_health(git: &dyn GitRunner, bare: &Path) -> Result<bool, GitError> {
    // Quiet probes (no error-level logging): a failing probe is an
    // expected, recoverable state, not a git invocation bug. A probe
    // that can't SPAWN, however, is an environment error we surface.
    async fn probe(
        git: &dyn GitRunner,
        bare: &Path,
        args: &[&str],
    ) -> Result<Option<String>, GitError> {
        let out = git.run(Some(bare), args, &[]).await.map_err(|e| {
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
    match probe(git, bare, &["rev-parse", "--is-bare-repository"]).await? {
        Some(out) if out.trim() == "true" => {}
        _ => return Ok(false),
    }
    Ok(
        probe(git, bare, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .await?
            .is_some(),
    )
}

/// Read `remote.origin.url` straight from the config file of a (possibly
/// broken) bare clone. Uses `git config --file` so it works even when
/// the directory is too damaged for normal repo commands.
async fn configured_origin_url(git: &dyn GitRunner, bare: &Path) -> Option<String> {
    let config = bare.join("config");
    let out = git
        .run(
            None,
            &[
                "config",
                "--file",
                &config.to_string_lossy(),
                "--get",
                "remote.origin.url",
            ],
            &[],
        )
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
async fn resumable_partial_origin(git: &dyn GitRunner, partial: &Path) -> Option<String> {
    let is_bare = git
        .run(Some(partial), &["rev-parse", "--is-bare-repository"], &[])
        .await
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false);
    if !is_bare {
        return None;
    }
    configured_origin_url(git, partial).await
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
    git: &dyn GitRunner,
    partial: &Path,
    envs: &[(String, String)],
) -> Result<(), GitError> {
    // `ls-remote --symref origin HEAD` prints e.g.
    // "ref: refs/heads/main\tHEAD" when the server advertises it.
    let advertised = run_git_in_env(
        git,
        partial,
        &["ls-remote", "--symref", "origin", "HEAD"],
        envs,
    )
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
            if ref_exists_with(git, partial, guess).await {
                head = Some(guess.to_string());
                break;
            }
        }
    }
    if head.is_none() {
        head = run_git_in(
            git,
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
        run_git_in(git, partial, &["symbolic-ref", "HEAD", &head]).await?;
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
    git: &dyn GitRunner,
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
                    // A registered gitdir alone doesn't prove the
                    // checkout completed: `git worktree add` writes the
                    // metadata and the `.git` file BEFORE populating
                    // files, and the index only lands once the checkout
                    // finishes. A killed add (timeout, Esc-cancel)
                    // therefore leaves a half-populated tree this used
                    // to report `Valid` forever. Repair in place with
                    // `reset --hard` — it completes the checkout while
                    // preserving untracked files, unlike deleting the
                    // directory (the tree may hold user work from a
                    // fallback session spawned after the failed
                    // provision). Repair failure falls through to the
                    // content check (loud refusal / empty reprovision).
                    if tokio::fs::metadata(gitdir.join("index")).await.is_ok() {
                        return Ok(WorktreeDirState::Valid);
                    }
                    // `reset --hard` preserves untracked files but
                    // *discards* edits to tracked files. If a fallback
                    // session spawned after the failed provision (#446)
                    // wrote content into a tracked file, repairing here
                    // would silently destroy it (#512). Only repair when
                    // no tracked-file edits are at risk; otherwise fall
                    // through to the content check, which refuses loudly
                    // (real work is non-pristine) rather than clobber.
                    if worktree_has_tracked_edits(git, &gitdir, wt_path).await {
                        tracing::warn!(
                            path = %wt_path.display(),
                            gitdir = %gitdir.display(),
                            "worktree checkout incomplete (no index) but holds \
                             tracked-file edits — refusing reset --hard, deferring \
                             to the content check to avoid discarding user work"
                        );
                    } else {
                        tracing::warn!(
                            path = %wt_path.display(),
                            gitdir = %gitdir.display(),
                            "worktree checkout incomplete (no index — interrupted \
                             `git worktree add`?); repairing with reset --hard"
                        );
                        if run_git_in(git, wt_path, &["reset", "--hard"]).await.is_ok() {
                            return Ok(WorktreeDirState::Valid);
                        }
                    }
                } else {
                    tracing::warn!(
                        path = %wt_path.display(),
                        gitdir = %gitdir.display(),
                        "worktree .git points at a missing gitdir (bare clone deleted?) — not valid"
                    );
                }
            }
        }
    }
    // Not a worktree of our bare clone. Every dir reaching here sits
    // at a lazybox-managed worktree path, so it's debris lazybox
    // created — reclaimable — with one exception: a dir holding
    // uncommitted work a fallback session wrote after the failed
    // provision (issue #446), which must be preserved.
    //
    // Empty-ish leftovers (at most a stray `.git`) never held work;
    // clear them outright. A content-full leftover is the rapid-`w w`
    // orphan (issue #447): a `worktree add` that checked files out but
    // lost its registration — killed mid-checkout by the #422 timeout,
    // or a bare re-clone that wiped `worktrees/` metadata. It's
    // disposable only when its checkout is pristine (byte-identical to
    // a commit we already have); real edits make it non-pristine and
    // it is refused, never clobbered.
    let mut has_real_content = false;
    let mut entries = tokio::fs::read_dir(wt_path).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry.file_name() != ".git" {
            has_real_content = true;
            break;
        }
    }
    if has_real_content && !leftover_is_pristine_checkout(git, bare_path, wt_path).await {
        return Err(GitError::Command(format!(
            "{} exists but is not a worktree of {} and holds uncommitted work — \
             refusing to reuse or overwrite it; move the directory aside and retry",
            wt_path.display(),
            bare_path.display()
        )));
    }
    tracing::warn!(
        path = %wt_path.display(),
        "reclaiming invalid worktree directory (failed earlier provision?) before re-provisioning"
    );
    tokio::fs::remove_dir_all(wt_path).await?;
    // A killed `worktree add` registers `<bare>/worktrees/<name>`
    // before the checkout finishes; with the directory now gone that
    // entry is prunable, and clearing it keeps the re-provision's
    // `worktree add -B <branch>` from failing with "'<branch>' is
    // already used by worktree".
    let _ = run_git_in(git, bare_path, &["worktree", "prune"]).await;
    Ok(WorktreeDirState::Reprovision)
}

/// Whether the half-checked-out tree at `wt` (a `git worktree add`
/// killed before its index landed) holds tracked-file edits that a
/// repairing `reset --hard` would discard — a fallback session's work
/// written after the failed provision (#446/#512).
///
/// `reset --hard` already preserves *untracked* additions and legitimately
/// *restores* tracked files the interrupted checkout never wrote, so
/// neither counts as work at risk. Only a tracked file present in the
/// working tree with content differing from `HEAD` (a modification, not a
/// deletion) would be silently clobbered. Detection runs against a
/// throwaway index so it never mutates the worktree's own git state:
/// `read-tree HEAD` loads the committed tree, then `diff-files` reports
/// working-tree entries that differ from it — a pure deletion (`D`) is the
/// interrupted checkout's own unwritten file and is ignored; any other
/// status (a modification) means real work.
///
/// Any probe failure returns `true`: without a confident "no edits"
/// verdict the caller must not run the destructive repair.
async fn worktree_has_tracked_edits(git: &dyn GitRunner, gitdir: &Path, wt: &Path) -> bool {
    static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = gitdir.join(format!(
        "lazybox-edit-probe-{}-{seq}.index",
        std::process::id()
    ));
    let gitdir_arg = gitdir.to_string_lossy().into_owned();
    let wt_arg = wt.to_string_lossy().into_owned();
    let index_env = vec![(
        "GIT_INDEX_FILE".to_string(),
        index.to_string_lossy().into_owned(),
    )];

    let loaded = run_git_in_env(
        git,
        wt,
        &[
            "--git-dir",
            &gitdir_arg,
            "--work-tree",
            &wt_arg,
            "read-tree",
            "HEAD",
        ],
        &index_env,
    )
    .await
    .is_ok();
    if !loaded {
        let _ = tokio::fs::remove_file(&index).await;
        return true;
    }
    let diff = run_git_in_env(
        git,
        wt,
        &[
            "--git-dir",
            &gitdir_arg,
            "--work-tree",
            &wt_arg,
            "diff-files",
            "--name-status",
        ],
        &index_env,
    )
    .await;
    let _ = tokio::fs::remove_file(&index).await;
    match diff {
        // Each line is `<status>\t<path>`. A pure deletion (`D`) is the
        // interrupted checkout's own missing file; anything else means a
        // tracked file was edited in place.
        Ok(out) => out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .any(|l| !l.starts_with('D')),
        Err(_) => true,
    }
}

/// Whether the working-tree content at `wt` is a pristine checkout —
/// byte-identical to a commit already in `bare`, carrying no
/// uncommitted work. This is what distinguishes a failed-`worktree add`
/// leftover (disposable debris lazybox created) from a directory
/// holding real edits a fallback session made, which must be preserved
/// (issue #446).
///
/// Linkage-free and branch-agnostic: the worktree's own `.git` may be
/// dangling (its `worktrees/<name>` metadata pruned, or a bare re-clone
/// wiped it), so this never runs git *inside* `wt`. Instead it hashes
/// the directory through a throwaway index against `bare`'s object
/// store and checks the resulting tree against the tree of every ref.
/// `.gitignore` is honored, so build debris doesn't read as work. A
/// checkout whose tree matches no ref (real edits, or a start point
/// that has since advanced off every tip) reads as non-pristine — the
/// conservative direction, since the cost of a wrong "pristine" verdict
/// is deleting user work.
///
/// Any probe failure returns `false`: without a confident "pristine"
/// verdict the caller refuses rather than risk clobbering.
async fn leftover_is_pristine_checkout(git: &dyn GitRunner, bare: &Path, wt: &Path) -> bool {
    // A per-call throwaway index. The name is process-and-sequence
    // unique so two probes on the same bare can never share it — the
    // one shared-state hazard of this otherwise read-only inspection,
    // and the interleaving that could corrupt it is exactly the one
    // that would mislabel a dirty leftover as pristine and delete it.
    static PROBE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = PROBE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = bare.join(format!(
        "lazybox-recover-{}-{seq}.index",
        std::process::id()
    ));
    let index_env = vec![(
        "GIT_INDEX_FILE".to_string(),
        index.to_string_lossy().into_owned(),
    )];
    let bare_arg = bare.to_string_lossy().into_owned();
    let wt_arg = wt.to_string_lossy().into_owned();

    // Stage every working file (respecting `.gitignore`) into the
    // throwaway index, then snapshot it as a tree. `add` writes the
    // blobs into `bare`'s object store — for a pristine checkout they
    // already exist (no-op dedup); for a dirty one the extra loose
    // objects are unreachable and reaped by the next `git gc`.
    let staged = run_git_in_env(
        git,
        wt,
        &["--git-dir", &bare_arg, "--work-tree", &wt_arg, "add", "-A"],
        &index_env,
    )
    .await
    .is_ok();
    let tree = if staged {
        run_git_in_env(git, wt, &["--git-dir", &bare_arg, "write-tree"], &index_env).await
    } else {
        Err(GitError::Command(
            "staging the leftover checkout failed".into(),
        ))
    };
    let _ = tokio::fs::remove_file(&index).await;
    let Ok(tree) = tree else { return false };
    let tree = tree.trim();
    if tree.is_empty() {
        return false;
    }

    // Peel every branch tip to its tree in one `rev-parse`. A pristine
    // checkout's tree is among these; anything else carries real work.
    // Only `refs/heads` and `refs/remotes` — a worktree checkout always
    // derives from a branch, never a tag, so tags add no legitimate
    // match while a single non-peelable tag (e.g. git.git's blob-valued
    // `refs/tags/junio-gpg-pub`) would fail the whole batched
    // `rev-parse` and disable recovery for the entire repo.
    let Ok(specs) = run_git_in(
        git,
        bare,
        &[
            "for-each-ref",
            "--format=%(objectname)^{tree}",
            "refs/heads",
            "refs/remotes",
        ],
    )
    .await
    else {
        return false;
    };
    let specs: Vec<&str> = specs.lines().filter(|l| !l.is_empty()).collect();
    if specs.is_empty() {
        return false;
    }
    let mut args: Vec<&str> = vec!["rev-parse"];
    args.extend(specs);
    match run_git_in(git, bare, &args).await {
        Ok(out) => out.lines().any(|l| l.trim() == tree),
        Err(_) => false,
    }
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
    git: &dyn GitRunner,
    bare_path: &Path,
    owner: &str,
    repo: &str,
    branch: &str,
    envs: &[(String, String)],
) -> Result<(), GitError> {
    // The source ref must be fully qualified and pruning disabled.
    // Under a user's global `fetch.prune = true`, a re-fetch with a
    // short source name ("feat") doesn't reverse-map the existing
    // `refs/remotes/origin/feat` onto the command-line refspec, so git
    // PRUNES the very ref this fetch maintains — and exits 0. The next
    // start-point lookup then finds neither the remote-tracking nor a
    // local ref, provisioning fails, and the spawn falls back to an
    // empty non-git dir (issue #404). `refs/heads/` also pins the
    // source to a branch, so a tag sharing the branch's name can't win
    // the DWIM lookup.
    run_git_in_env(
        git,
        bare_path,
        &[
            "fetch",
            "--no-prune",
            "origin",
            &format!("+refs/heads/{branch}:refs/remotes/origin/{branch}"),
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

/// Fetch a PR's head commit via GitHub's `refs/pull/<N>/head` pseudo-ref
/// into a private tracking ref `refs/lazybox/pr/<N>`. The base repo
/// (`origin`) exposes this ref for *every* PR — fork PRs (whose head lives
/// on the contributor's fork) and PRs whose head branch was deleted
/// included — so it's the robust fallback when a PR's head branch isn't a
/// plain branch on `origin` (issue #550). `--no-prune` for the same reason
/// as [`fetch_origin_ref`]: a user's global `fetch.prune = true` must not
/// delete the tracking ref this fetch maintains. On failure, log a warning
/// and return the error — the caller falls back to a local ref or a clear
/// error.
async fn fetch_pull_head(
    git: &dyn GitRunner,
    bare_path: &Path,
    owner: &str,
    repo: &str,
    pr_number: u64,
    envs: &[(String, String)],
) -> Result<(), GitError> {
    run_git_in_env(
        git,
        bare_path,
        &[
            "fetch",
            "--no-prune",
            "origin",
            &format!("+refs/pull/{pr_number}/head:refs/lazybox/pr/{pr_number}"),
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
            pr_number,
            error = %e,
            "could not fetch pull-request head from origin"
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
    git: &dyn GitRunner,
    bare_path: &Path,
    branch: &str,
    err: &GitError,
    authed: bool,
    now: std::time::SystemTime,
) -> Option<CheckoutPhase> {
    let start = if ref_exists_with(git, bare_path, &format!("refs/remotes/origin/{branch}")).await {
        format!("refs/remotes/origin/{branch}")
    } else if ref_exists_with(git, bare_path, &format!("refs/heads/{branch}")).await {
        format!("refs/heads/{branch}")
    } else {
        return None;
    };
    // `%h <short sha>` + `%cr <committer date, relative>` → e.g.
    // "a1b2c3d, 3 days ago". One `git show` for both fields.
    let output = git
        .run(
            Some(bare_path),
            &["show", "-s", "--format=%h, %cr", &start],
            &[],
        )
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let desc = String::from_utf8_lossy(&output.stdout);
    let desc = desc.trim();
    if desc.is_empty() {
        return None;
    }
    let mut note = format!("could not refresh {branch} — branched from local ref ({desc}); ");
    note.push_str(&fetch_failure_reason(err, authed));
    Some(classify_stale(last_refresh_age(bare_path, now), note))
}

fn classify_stale(age: Option<std::time::Duration>, mut note: String) -> CheckoutPhase {
    match age {
        Some(age) if age >= PERSISTENT_STALE_AFTER => {
            note.push_str(&format!(
                "; origin has not refreshed in {}",
                format_age(age)
            ));
            CheckoutPhase::BaseRefStalePersistent(note)
        }
        _ => CheckoutPhase::BaseRefStale(note),
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
        GitError::BranchHeldLive { .. }
        | GitError::BranchHeldManaged { .. }
        | GitError::WorktreeBranchMismatch { .. } => err.to_string(),
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
fn last_refresh_age(bare: &Path, now: std::time::SystemTime) -> Option<std::time::Duration> {
    let mtime = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let t = mtime(bare.join(REFRESH_STAMP)).or_else(|| mtime(bare.join("HEAD")))?;
    now.duration_since(t).ok()
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
async fn ref_exists_with(git: &dyn GitRunner, bare_path: &Path, ref_name: &str) -> bool {
    git.run(
        Some(bare_path),
        &["show-ref", "--verify", "--quiet", ref_name],
        &[],
    )
    .await
    .map(|o| o.status.success())
    .unwrap_or(false)
}

#[cfg(test)]
async fn ref_exists(bare_path: &Path, ref_name: &str) -> bool {
    ref_exists_with(default_git_runner(), bare_path, ref_name).await
}

/// Whether every commit reachable from `maybe_ancestor` is also reachable
/// from `descendant` — i.e. resetting `maybe_ancestor` onto `descendant`
/// would lose nothing. `git merge-base --is-ancestor` exits 0 for yes, 1
/// for no; any other failure (bad ref, probe error) is treated as "not an
/// ancestor" so callers stay on the conservative side (keep the local ref)
/// rather than discarding commits on an inconclusive answer.
async fn is_ancestor(
    git: &dyn GitRunner,
    bare_path: &Path,
    maybe_ancestor: &str,
    descendant: &str,
) -> bool {
    git.run(
        Some(bare_path),
        &["merge-base", "--is-ancestor", maybe_ancestor, descendant],
        &[],
    )
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
/// - `GIT_HTTP_LOW_SPEED_LIMIT/TIME` — abort an HTTP transfer that
///   moves fewer than 1 KB/s for 30 straight seconds. A clone whose
///   pack transfer stalls mid-flight otherwise sits connected-but-idle
///   until the wall-clock cap, which reads as "forever" from the
///   provisioning checklist (issue #403). Only affects the HTTP(S)
///   transport — the path network ops take when a gh token resolves
///   (issue #394). Skipped when the operator set either variable
///   themselves (env beats git config, so ours would silently override
///   their tuning — including a deliberate opt-out).
/// - `GIT_SSH_COMMAND` with keepalives — over SSH there is no
///   low-speed knob, and the diagnosed #403 hang was an ssh sitting on
///   a dead-but-connected socket at 0% CPU. `ServerAliveInterval=15` /
///   `CountMax=4` fails an unresponsive connection within ~60s (a
///   server that still answers keepalives but never sends pack data is
///   only caught by the wall-clock cap). Skipped when the user brings
///   their own SSH transport — `GIT_SSH`/`GIT_SSH_COMMAND` in the env,
///   or `core.sshCommand` in their git config (env would silently
///   override the config, breaking e.g. per-host key setups).
///
/// Removes any inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`
/// / `GIT_COMMON_DIR`. Those override `current_dir(cwd)` silently — if
/// lazybox is ever launched from inside another git worktree (or
/// `cargo test` from one), the subprocess would operate on the
/// inherited repo instead of the bare clone we're targeting.
///
fn apply_git_env(cmd: &mut Command) -> &mut Command {
    if std::env::var_os("GIT_HTTP_LOW_SPEED_LIMIT").is_none()
        && std::env::var_os("GIT_HTTP_LOW_SPEED_TIME").is_none()
    {
        cmd.env("GIT_HTTP_LOW_SPEED_LIMIT", "1024")
            .env("GIT_HTTP_LOW_SPEED_TIME", "30");
    }
    if !user_has_own_ssh_transport() {
        cmd.env(
            "GIT_SSH_COMMAND",
            "ssh -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -o ConnectTimeout=30",
        );
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_FLUSH", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
}

/// Whether the user routes git-over-SSH through their own transport —
/// `GIT_SSH`/`GIT_SSH_COMMAND` in the environment, or `core.sshCommand`
/// in their git config. When they do, lazybox must not inject its
/// keepalive `GIT_SSH_COMMAND`: the env variable outranks config, so it
/// would silently replace per-host key setups. The config half is one
/// `git config` probe, cached for the process lifetime (global/system
/// config only — lazybox-created bare clones never set it locally).
fn user_has_own_ssh_transport() -> bool {
    static CONFIGURED: OnceLock<bool> = OnceLock::new();
    if std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some() {
        return true;
    }
    *CONFIGURED.get_or_init(|| {
        std::process::Command::new("git")
            .args(["config", "--get", "core.sshCommand"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    })
}

/// A transfer-progress stderr line, as opposed to a ref listing,
/// remote banner, or fatal error. Gates both the `on_progress` sink in
/// [`run_git_transfer`] and the noise filter on its error tail.
fn is_transfer_progress(line: &str) -> bool {
    line.contains('%') || line.ends_with("done.")
}

/// Run `git worktree add -B <branch> <wt_path> <start_point>`,
/// resolving a "branch already checked out in another worktree"
/// collision instead of failing hard (issue #439).
///
/// A branch can be checked out in only one worktree at a time. Claude
/// Code creates its own sub-agent worktrees under
/// `<bare>.git/.claude/worktrees/agent-*`; because lazybox's bare clone
/// is the git dir, those nested worktrees register against the same
/// bare clone and can already hold the branch lazybox now wants.
/// `git worktree add -B` then refuses with
/// `fatal: '<branch>' is already used by worktree at '<path>'`.
///
/// Resolution ladder:
/// 1. Any *other* failure propagates unchanged (reworded for the
///    blobless-clone promisor case).
/// 2. `git worktree prune` + retry — drops *stale* registrations whose
///    directory is gone (a leftover lazybox worktree, a reaped agent
///    run), the common recoverable case, without touching any live
///    checkout.
/// 3. If the holder is a Claude Code agent worktree — one living under
///    `<bare>/.claude/worktrees/`, never one of lazybox's own, which
///    live under `<base>/worktrees/` — attach here with `--force`.
///    git overrides the "already checked out" refusal for the *add*,
///    but separately refuses to force-*reset* a branch (`-B`) held by
///    another worktree — so the force path drops `-B` and checks the
///    branch out at its current tip. Both worktrees then share the
///    branch; the abandoned agent worktree's files are left in place.
/// 4. Otherwise the holder is another real checkout (e.g. a live
///    lazybox session on the same branch); surface a clear, actionable
///    error instead of silently stealing its branch.
async fn add_worktree_resilient(
    git: &dyn GitRunner,
    bare_path: &Path,
    wt_path: &Path,
    branch: &str,
    start_point: &str,
    auth: &[(String, String)],
) -> Result<(), GitError> {
    let wt = wt_path.to_string_lossy();
    let wt: &str = &wt;
    let plain: [&str; 6] = ["worktree", "add", "-B", branch, wt, start_point];
    let forced: [&str; 5] = ["worktree", "add", "--force", wt, branch];

    let err = match git.run_transfer(bare_path, &plain, auth, None).await {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    let Some(holder) = branch_already_checked_out_at(&err) else {
        // Same-path collision: the target path itself is registered
        // (its directory was `rm -rf`'d by hand) — a stale entry prune
        // clears, after which the plain add succeeds. Distinct from
        // the branch-holder ladder below because git names the path,
        // not a holding worktree.
        if worktree_missing_but_registered(&err) {
            let _ = run_git_in(git, bare_path, &["worktree", "prune"]).await;
            return git
                .run_transfer(bare_path, &plain, auth, None)
                .await
                .map_err(explain_promisor_failure);
        }
        return Err(explain_promisor_failure(err));
    };

    let _ = run_git_in(git, bare_path, &["worktree", "prune"]).await;
    if git
        .run_transfer(bare_path, &plain, auth, None)
        .await
        .is_ok()
    {
        return Ok(());
    }

    // Only force-share the branch when the holder is a genuine Claude
    // Code agent worktree — those live under `<bare>/.claude/worktrees/`
    // (see the doc comment). A bare-path `starts_with` was too loose:
    // any holder that merely canonicalized *somewhere* under the bare
    // directory would get its branch silently co-opted (#512). Classify
    // against the actual agent-worktree root instead; anything outside
    // it falls through to the loud refusal below.
    let agent_root = canonical_or_self(&bare_path.join(".claude").join("worktrees"));
    if canonical_or_self(&holder).starts_with(&agent_root) {
        return git
            .run_transfer(bare_path, &forced, auth, None)
            .await
            .map_err(explain_promisor_failure);
    }

    Err(GitError::BranchHeldLive {
        branch: branch.to_string(),
        holder,
    })
}

/// Materialize a *detached* worktree at `wt_path` checked out on
/// `start_point`'s commit. Unlike [`add_worktree_resilient`] there is no
/// branch to contend for, so the branch-collision ladder is irrelevant —
/// a detached HEAD never conflicts with another worktree holding the
/// branch name (#721). Retains the same-path `prune`-and-retry rung for a
/// stale registration whose directory was `rm -rf`'d by hand, and the
/// same blobless-clone error rewording.
async fn add_worktree_detached(
    git: &dyn GitRunner,
    bare_path: &Path,
    wt_path: &Path,
    start_point: &str,
    auth: &[(String, String)],
) -> Result<(), GitError> {
    let wt = wt_path.to_string_lossy();
    let wt: &str = &wt;
    let add: [&str; 5] = ["worktree", "add", "--detach", wt, start_point];
    match git.run_transfer(bare_path, &add, auth, None).await {
        Ok(()) => Ok(()),
        Err(e) if worktree_missing_but_registered(&e) => {
            let _ = run_git_in(git, bare_path, &["worktree", "prune"]).await;
            git.run_transfer(bare_path, &add, auth, None)
                .await
                .map_err(explain_promisor_failure)
        }
        Err(e) => Err(explain_promisor_failure(e)),
    }
}

/// Parse the holding worktree's path out of a failed `worktree add`
/// where the branch is checked out elsewhere, returning `None` for any
/// other failure (the caller then treats the error as fatal). git
/// phrases this collision three ways across versions and add flags, all
/// carrying the path in trailing single quotes: `'<b>' is already used
/// by worktree at '<path>'` (modern `-B`) and `cannot force update the
/// branch '<b>' used by worktree at '<path>'` (the branch-reset guard)
/// both match `used by worktree at`; older gits say `'<b>' is already
/// checked out at '<path>'`.
fn branch_already_checked_out_at(err: &GitError) -> Option<PathBuf> {
    let GitError::Command(msg) = err else {
        return None;
    };
    let after_marker = ["used by worktree at", "is already checked out at"]
        .into_iter()
        .find_map(|m| msg.find(m).map(|i| i + m.len()))?;
    let rest = &msg[after_marker..];
    let start = rest.find('\'')? + 1;
    let end = rest[start..].find('\'')? + start;
    Some(PathBuf::from(rest[start..end].trim()))
}

/// Companion matcher to [`branch_already_checked_out_at`] for the
/// *same-path* re-add case: after a manual `rm -rf` of a worktree its
/// registration in `<bare>/worktrees/` survives, and `git worktree add`
/// at that same path fails with `'<path>' is a missing but already
/// registered worktree; use 'add -f' to override, or 'prune' or
/// 'remove' to clear`. That message names the path, not the branch, so
/// the branch-collision matcher never fires and the prune-and-retry
/// ladder used to be skipped — leaving the session permanently
/// unprovisionable. A stale registration whose directory is gone is
/// exactly what `git worktree prune` clears, so the caller retries
/// after pruning.
fn worktree_missing_but_registered(err: &GitError) -> bool {
    let GitError::Command(msg) = err else {
        return false;
    };
    msg.contains("missing but already registered worktree")
}

/// Whether the worktree at `wt_path` is in a state a background sync
/// must not advance: detached HEAD, or a bisect / rebase / merge /
/// cherry-pick in progress. Returns the human-readable reason, `None`
/// when the tree is on a plain branch with no operation underway.
///
/// Checks the way git itself does: `symbolic-ref -q HEAD` fails on a
/// detached HEAD, and each in-progress operation leaves a marker in
/// the worktree's private git dir (`BISECT_LOG`, `rebase-merge/`,
/// `rebase-apply/`, `MERGE_HEAD`, `CHERRY_PICK_HEAD`). Errors only
/// when the git dir itself can't be resolved (not a worktree at all).
async fn worktree_unsafe_to_advance(
    git: &dyn GitRunner,
    wt_path: &Path,
) -> Result<Option<&'static str>, GitError> {
    // Resolve the worktree's own git dir first (where per-worktree
    // operation state lives). Failure here means "not a usable
    // worktree" — a real error the caller should surface, not an
    // unsafe-state skip.
    let gitdir = run_git_in(git, wt_path, &["rev-parse", "--absolute-git-dir"]).await?;
    let gitdir = PathBuf::from(gitdir.trim());

    if run_git_in(git, wt_path, &["symbolic-ref", "-q", "HEAD"])
        .await
        .is_err()
    {
        return Ok(Some("detached HEAD"));
    }
    for (marker, reason) in [
        ("BISECT_LOG", "bisect in progress"),
        ("rebase-merge", "rebase in progress"),
        ("rebase-apply", "rebase/am in progress"),
        ("MERGE_HEAD", "merge in progress"),
        ("CHERRY_PICK_HEAD", "cherry-pick in progress"),
    ] {
        if tokio::fs::metadata(gitdir.join(marker)).await.is_ok() {
            return Ok(Some(reason));
        }
    }
    Ok(None)
}

/// Cheap health probe for an existing directory at a lazybox worktree
/// path, usable without knowing the owning bare clone. `true` when the
/// directory plausibly is a completed checkout:
/// - `.git` is a directory (a standalone-init worktree or a full
///   clone), or
/// - `.git` is a worktree pointer file whose `gitdir:` target exists
///   and has an `index` (i.e. the `git worktree add` finished).
///
/// `false` for everything else — an empty dir left by an old failed
/// provision, a `.git` dangling after the bare clone was deleted, a
/// half-checked-out tree. Callers on the spawn fast path use this to
/// decide whether to short-circuit or route through the full
/// provision/repair machinery ([`WorktreeManager::checkout_at`]'s
/// validation), which owns the actual reclaim/repair decisions.
pub async fn worktree_dir_ready(wt_path: &Path) -> bool {
    let dot_git = wt_path.join(".git");
    let Ok(meta) = tokio::fs::metadata(&dot_git).await else {
        return false;
    };
    if meta.is_dir() {
        return true;
    }
    let Ok(contents) = tokio::fs::read_to_string(&dot_git).await else {
        return false;
    };
    let Some(gitdir) = contents
        .lines()
        .find_map(|l| l.strip_prefix("gitdir:"))
        .map(|p| PathBuf::from(p.trim()))
    else {
        return false;
    };
    // `git worktree add` writes absolute gitdir paths, but tolerate a
    // relative one (hand-crafted / repaired pointers) by resolving it
    // against the worktree itself, the way git does.
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        wt_path.join(gitdir)
    };
    tokio::fs::metadata(gitdir.join("index")).await.is_ok()
}

/// Whether `wt_path` is a completed checkout ready to reuse for
/// `expected_branch`.
///
/// This is the branch-aware spawn fast path: structural readiness alone
/// is insufficient because a valid worktree can have been switched to a
/// *different named branch* since its session was persisted — that must
/// re-provision. A *detached* HEAD is not a mismatch, though: it is a
/// deliberate state (a branch held by another live worktree provisions
/// detached, #721; an agent may detach to inspect a commit), reused in
/// place rather than re-provisioned. This mirrors `ensure_worktree_branch`,
/// the reuse guard on the provision path, and sync, which likewise never
/// fights a detached HEAD (`worktree_unsafe_to_advance`). Without the tolerance the readiness
/// probe reports every detached worktree as stale, and each spawn re-runs
/// the full provision (mounts, setup scripts, progress modal) for a tree
/// that only ever gets reused.
pub async fn worktree_dir_ready_on_branch(wt_path: &Path, expected_branch: &str) -> bool {
    worktree_dir_ready(wt_path).await
        && current_worktree_branch(wt_path)
            .await
            .is_ok_and(|branch| match branch.as_deref() {
                None => true,
                Some(actual) => actual == expected_branch,
            })
}

/// Guard the idempotent re-entry: a worktree already at the session path
/// must be on the expected branch before we reuse it, else a completed
/// checkout on the *wrong named branch* would be silently repurposed
/// (`BranchMismatch`). A *detached* HEAD is deliberately tolerated — it
/// is a state lazybox itself creates when the branch is exclusively held
/// by another live worktree (#721), and one an agent/user reaches by
/// inspecting a commit. Erroring the re-entry (or forcing HEAD back onto
/// the branch) would strand the session or yank work; sync already
/// refuses to advance a detached HEAD (`worktree_unsafe_to_advance`), so
/// reusing it in place is consistent.
async fn ensure_worktree_branch(wt_path: &Path, expected: &str) -> Result<(), GitError> {
    match current_worktree_branch(wt_path).await? {
        None => Ok(()),
        Some(actual) if actual == expected => Ok(()),
        Some(actual) => Err(GitError::WorktreeBranchMismatch {
            path: wt_path.to_path_buf(),
            expected: expected.to_string(),
            actual,
        }),
    }
}

async fn current_worktree_branch(wt_path: &Path) -> Result<Option<String>, GitError> {
    let output = apply_git_env(Command::new("git").current_dir(wt_path).args([
        "symbolic-ref",
        "--quiet",
        "--short",
        "HEAD",
    ]))
    .output()
    .await?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((!branch.is_empty()).then_some(branch));
    }
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    Err(GitError::Command(
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// A blobless clone materializes file contents through origin at
/// checkout time, so `worktree add` can fail on a *network* problem
/// even though every ref it needs is local. Reword git's opaque
/// promisor error (`could not fetch <oid> from promisor remote`) into
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
/// cap — transfers are slow by nature but must stay finite — and
/// [`exec_git_bounded`]'s process-group sweep, so a timed-out or
/// cancelled transfer takes its ssh / remote helpers down with it.
/// Callers hold the repo lock throughout, so the cap is also how long
/// a hung transfer can stall every other operation on the same repo —
/// adaptive/timeout tuning is issue #403's territory.
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
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    apply_git_env(cmd.args(args))
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // Same process-group discipline as `exec_git_bounded`: transfers
    // are the invocations most likely to be sitting on a live ssh /
    // remote helper when they time out or their future is dropped
    // (Esc-cancel), and `kill_on_drop` alone orphans those helpers.
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child = cmd.spawn()?;
    let mut group = KillGroupOnDrop(child.id().map(|id| id as i32));
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
            // Disarm only on a clean exit — a non-zero or timed-out git
            // may have left transport helpers behind, and sweeping the
            // dead leader's group costs one syscall that can only reach
            // processes this spawn created.
            group.0 = None;
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

const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const GIT_NO_CWD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

async fn run_git(
    git: &dyn GitRunner,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<String, GitError> {
    run_git_with(git, None, args, envs).await
}

async fn run_git_with(
    git: &dyn GitRunner,
    cwd: Option<&Path>,
    args: &[&str],
    envs: &[(String, String)],
) -> Result<String, GitError> {
    let output = git.run(cwd, args, envs).await?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into())
    } else {
        Err(GitError::Command(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

/// Kills the child's whole process group with SIGKILL on drop unless
/// disarmed. `kill_on_drop` alone only signals the direct `git` child:
/// git's transport helpers (`ssh … git-upload-pack`, remote helpers)
/// survive it orphaned, exactly the leftover process issue #403
/// diagnosed after a stalled clone. The child is spawned as its own
/// process group leader so one `killpg` reaps the entire tree — on a
/// timeout, on an Esc-cancel dropping the provisioning future, or on
/// task abort.
struct KillGroupOnDrop(Option<i32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0 {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
    }
}

async fn exec_git_bounded(
    cwd: Option<&Path>,
    args: &[&str],
    envs: &[(String, String)],
    timeout: std::time::Duration,
) -> Result<Output, GitError> {
    let label = match cwd {
        Some(cwd) => format!("git (in {}) {}", cwd.display(), args.join(" ")),
        None => format!("git {}", args.join(" ")),
    };
    let started = std::time::Instant::now();
    tracing::info!("{label}");
    let mut cmd = Command::new("git");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    apply_git_env(cmd.args(args))
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let fut = async {
        let child = cmd.spawn()?;
        let mut group = KillGroupOnDrop(child.id().map(|id| id as i32));
        let output = child.wait_with_output().await;
        // Disarm only on a clean exit: a git that died non-zero (or an
        // errored wait) may have left transport helpers behind, and
        // sweeping the dead leader's group costs one syscall that can
        // only reach processes this spawn created.
        if matches!(&output, Ok(o) if o.status.success()) {
            group.0 = None;
        }
        output
    };
    let output = match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res?,
        Err(_) => {
            let elapsed = started.elapsed();
            tracing::error!("{label} TIMED OUT after {elapsed:?}");
            return Err(GitError::Command(format!(
                "`git {}` exceeded {}s wall-clock",
                args.join(" "),
                timeout.as_secs()
            )));
        }
    };
    let elapsed = started.elapsed();
    if output.status.success() {
        tracing::info!("{label} ok ({elapsed:?})");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::error!("{label} failed ({elapsed:?}): {}", stderr.trim());
    }
    Ok(output)
}

async fn exec_git_bounded_with_stdout_limit(
    cwd: Option<&Path>,
    args: &[&str],
    envs: &[(String, String)],
    timeout: std::time::Duration,
    stdout_limit: usize,
) -> Result<(Output, bool), GitError> {
    let label = match cwd {
        Some(cwd) => format!("git (in {}) {}", cwd.display(), args.join(" ")),
        None => format!("git {}", args.join(" ")),
    };
    let started = std::time::Instant::now();
    tracing::info!("{label}");
    let mut cmd = Command::new("git");
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    apply_git_env(cmd.args(args))
        .envs(envs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let fut = async {
        let mut child = cmd.spawn()?;
        let mut group = KillGroupOnDrop(child.id().map(|id| id as i32));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("git stdout was not piped"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("git stderr was not piped"))?;
        let read_stderr = async {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await?;
            Ok::<_, std::io::Error>(bytes)
        };
        let (status, stdout, stderr) = tokio::join!(
            child.wait(),
            read_stdout_limited(stdout, stdout_limit),
            read_stderr
        );
        let (stdout, truncated) = stdout?;
        let output = Output {
            status: status?,
            stdout,
            stderr: stderr?,
        };
        if output.status.success() {
            group.0 = None;
        }
        Ok::<_, GitError>((output, truncated))
    };
    let (output, truncated) = match tokio::time::timeout(timeout, fut).await {
        Ok(result) => result?,
        Err(_) => {
            let elapsed = started.elapsed();
            tracing::error!("{label} TIMED OUT after {elapsed:?}");
            return Err(GitError::Command(format!(
                "`git {}` exceeded {}s wall-clock",
                args.join(" "),
                timeout.as_secs()
            )));
        }
    };
    let elapsed = started.elapsed();
    if output.status.success() || truncated {
        tracing::info!("{label} ok ({elapsed:?})");
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::error!("{label} failed ({elapsed:?}): {}", stderr.trim());
    }
    Ok((output, truncated))
}

async fn read_stdout_limited(
    mut stdout: tokio::process::ChildStdout,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut chunk = [0u8; 8192];
    loop {
        let read = stdout.read(&mut chunk).await?;
        if read == 0 {
            return Ok((bytes, false));
        }
        let remaining = limit.saturating_sub(bytes.len());
        if read > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            return Ok((bytes, true));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() == limit {
            let mut probe = [0u8; 1];
            let truncated = stdout.read(&mut probe).await? != 0;
            return Ok((bytes, truncated));
        }
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

async fn run_git_in(git: &dyn GitRunner, cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    run_git_in_env(git, cwd, args, &[]).await
}

/// `git rev-list --count <range>` in `cwd`, as a `u64`. Any failure
/// (unresolvable range, unborn HEAD, not a repo) counts as `0` — the
/// callers all treat "no commits in the range" and "couldn't tell" the
/// same conservative way.
async fn count_commits(git: &dyn GitRunner, cwd: &Path, range: &str) -> u64 {
    run_git_in(git, cwd, &["rev-list", "--count", range])
        .await
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

async fn run_git_in_env(
    git: &dyn GitRunner,
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
    run_git_with(git, Some(cwd), args, envs).await
}

#[cfg(test)]
mod decision_tests {
    use super::*;

    #[test]
    fn start_point_precedence_is_exhaustive() {
        struct Case {
            has_origin: bool,
            pr: Option<u64>,
            pr_fetched: bool,
            local_exists: bool,
            local_is_ancestor: bool,
            expected: StartPoint,
        }

        for case in [
            Case {
                has_origin: true,
                pr: Some(42),
                pr_fetched: true,
                local_exists: true,
                local_is_ancestor: true,
                expected: StartPoint::Origin,
            },
            Case {
                has_origin: false,
                pr: Some(42),
                pr_fetched: true,
                local_exists: false,
                local_is_ancestor: false,
                expected: StartPoint::PullRequest(42),
            },
            Case {
                has_origin: false,
                pr: Some(42),
                pr_fetched: true,
                local_exists: true,
                local_is_ancestor: true,
                expected: StartPoint::PullRequest(42),
            },
            Case {
                has_origin: false,
                pr: Some(42),
                pr_fetched: true,
                local_exists: true,
                local_is_ancestor: false,
                expected: StartPoint::Local,
            },
            Case {
                has_origin: false,
                pr: Some(42),
                pr_fetched: false,
                local_exists: true,
                local_is_ancestor: true,
                expected: StartPoint::Local,
            },
            Case {
                has_origin: false,
                pr: None,
                pr_fetched: false,
                local_exists: true,
                local_is_ancestor: false,
                expected: StartPoint::Local,
            },
            Case {
                has_origin: false,
                pr: Some(42),
                pr_fetched: false,
                local_exists: false,
                local_is_ancestor: false,
                expected: StartPoint::Missing,
            },
            Case {
                has_origin: false,
                pr: None,
                pr_fetched: false,
                local_exists: false,
                local_is_ancestor: false,
                expected: StartPoint::Missing,
            },
        ] {
            assert_eq!(
                resolve_start_point(
                    case.has_origin,
                    case.pr,
                    case.pr_fetched,
                    case.local_exists,
                    case.local_is_ancestor,
                ),
                case.expected,
            );
        }
    }

    #[test]
    fn stale_classification_changes_at_twenty_four_hours() {
        let recent = PERSISTENT_STALE_AFTER - std::time::Duration::from_secs(1);
        assert!(matches!(
            classify_stale(Some(recent), "note".into()),
            CheckoutPhase::BaseRefStale(note) if note == "note"
        ));
        assert!(matches!(
            classify_stale(None, "note".into()),
            CheckoutPhase::BaseRefStale(note) if note == "note"
        ));
        assert!(matches!(
            classify_stale(Some(PERSISTENT_STALE_AFTER), "note".into()),
            CheckoutPhase::BaseRefStalePersistent(note)
                if note == "note; origin has not refreshed in 24 hours"
        ));
    }

    #[test]
    fn refresh_age_uses_the_supplied_time() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let head = tmp.path().join("HEAD");
        std::fs::write(&head, "ref: refs/heads/main\n").expect("write HEAD");
        let modified = std::fs::metadata(&head)
            .and_then(|metadata| metadata.modified())
            .expect("HEAD mtime");
        let age = std::time::Duration::from_secs(37 * 60);

        assert_eq!(last_refresh_age(tmp.path(), modified + age), Some(age));
    }
}

#[cfg(all(test, unix))]
mod runner_seam_tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[derive(Debug)]
    struct Call {
        cwd: Option<PathBuf>,
        args: Vec<String>,
        env: Vec<(String, String)>,
    }

    struct ScriptedGit {
        worktree: PathBuf,
        calls: Mutex<Vec<Call>>,
    }

    impl ScriptedGit {
        fn output(success: bool, stdout: &[u8]) -> Output {
            Output {
                status: std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 }),
                stdout: stdout.to_vec(),
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
            let worktree = self.worktree.to_string_lossy();
            let response = match args {
                ["rev-parse", "--is-bare-repository"] => Ok(Self::output(true, b"true\n")),
                ["rev-parse", "--verify", "--quiet", "HEAD"]
                | [
                    "fetch",
                    "--no-prune",
                    "origin",
                    "+refs/heads/feature:refs/remotes/origin/feature",
                ]
                | [
                    "show-ref",
                    "--verify",
                    "--quiet",
                    "refs/remotes/origin/feature",
                ]
                | ["config", "branch.feature.remote", "origin"]
                | ["config", "branch.feature.merge", "refs/heads/feature"] => {
                    Ok(Self::output(true, &[]))
                }
                ["show-ref", "--verify", "--quiet", "refs/heads/feature"] => {
                    Ok(Self::output(false, &[]))
                }
                [
                    "worktree",
                    "add",
                    "-B",
                    "feature",
                    target,
                    "refs/remotes/origin/feature",
                ] if *target == worktree => Ok(Self::output(true, &[])),
                _ => Err(GitError::Command(format!(
                    "unexpected scripted git command: {}",
                    args.join(" ")
                ))),
            };
            Box::pin(async move { response })
        }
    }

    #[tokio::test]
    async fn checkout_provisioning_uses_the_injected_runner_end_to_end() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path().join("repos/acme/widget.git");
        let worktree = tmp.path().join("worktrees/session");
        std::fs::create_dir_all(&bare).expect("bare dir");
        let git = Arc::new(ScriptedGit {
            worktree: worktree.clone(),
            calls: Mutex::new(Vec::new()),
        });
        let manager = WorktreeManager::new(tmp.path())
            .with_git_runner(Arc::clone(&git) as Arc<dyn GitRunner>);

        let checkout = manager
            .checkout_at(&worktree, "acme", "widget", "feature", None)
            .await
            .expect("scripted checkout");

        assert_eq!(checkout.path, worktree);
        assert_eq!(checkout.branch, "feature");
        let calls = git.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            calls
                .iter()
                .all(|call| call.cwd.as_deref() == Some(bare.as_path()))
        );
        assert!(calls.iter().all(|call| call.env.is_empty()));
        assert_eq!(
            calls
                .iter()
                .map(|call| call.args.join(" "))
                .collect::<Vec<_>>(),
            [
                "rev-parse --is-bare-repository",
                "rev-parse --verify --quiet HEAD",
                "fetch --no-prune origin +refs/heads/feature:refs/remotes/origin/feature",
                "show-ref --verify --quiet refs/heads/feature",
                "show-ref --verify --quiet refs/remotes/origin/feature",
                &format!(
                    "worktree add -B feature {} refs/remotes/origin/feature",
                    checkout.path.display()
                ),
                "config branch.feature.remote origin",
                "config branch.feature.merge refs/heads/feature",
            ]
        );
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
            fetch_origin_ref(default_git_runner(), &bare, "acme", "widgets", "main", &[])
                .await
                .is_err(),
            "sanity: the dead SSH-style URL must not fetch on its own"
        );

        let hub_base = format!("{}/", tmp.path().join("hub").display());
        let envs =
            git_config_env(&[(&format!("url.{hub_base}.insteadOf"), "git@invalid.example:")]);
        fetch_origin_ref(
            default_git_runner(),
            &bare,
            "acme",
            "widgets",
            "main",
            &envs,
        )
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
#[cfg(unix)]
mod stalled_clone_tests {
    use super::*;

    /// A clone whose transport connects but never transfers must fail
    /// within the wall-clock deadline AND take its transport helper
    /// down with it — the diagnosed hang left an `ssh …
    /// git-upload-pack` orphan running forever (issue #403). The fake
    /// remote is an `ext::` helper that records its pid and sleeps;
    /// after the timeout the whole process group must be dead.
    #[tokio::test]
    async fn stalled_clone_times_out_and_kills_the_transport_helper() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pidfile = tmp.path().join("helper.pid");
        let helper = tmp.path().join("stall-remote.sh");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 600\n",
                pidfile.display()
            ),
        )
        .expect("write helper");
        chmod_executable(&helper).expect("chmod helper");

        let url = format!("ext::{}", helper.display());
        let dest = tmp.path().join("dest.git");
        let envs = git_config_env(&[("protocol.ext.allow", "always")]);
        let started = std::time::Instant::now();
        // 4s: long enough for git to have spawned the helper even on a
        // loaded CI machine (so the kill assertion below has a live
        // victim), short enough to keep the test snappy.
        let err = exec_git_bounded(
            None,
            &["clone", "--bare", &url, &dest.to_string_lossy()],
            &envs,
            std::time::Duration::from_secs(4),
        )
        .await
        .expect_err("a stalled clone must fail, not hang");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "timed out far past the deadline: {:?}",
            started.elapsed()
        );
        assert!(
            err.to_string().contains("exceeded"),
            "error must name the wall-clock cap: {err}"
        );

        // The helper starts within milliseconds of the clone, but give
        // the write a moment on loaded CI machines.
        let pid_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid: i32 = loop {
            match std::fs::read_to_string(&pidfile) {
                Ok(s) => break s.trim().parse().expect("pidfile holds a pid"),
                Err(_) if std::time::Instant::now() < pid_deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    let listing: Vec<_> = std::fs::read_dir(tmp.path())
                        .map(|it| it.filter_map(|e| e.ok().map(|e| e.file_name())).collect())
                        .unwrap_or_default();
                    panic!("helper never recorded its pid: {e}; tmp holds {listing:?}");
                }
            }
        };
        // SIGKILL delivery is immediate but reaping isn't; poll briefly.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "transport helper (pid {pid}) survived the timeout — orphaned child"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// The on-disk leftovers of a stalled attempt — a half-written
    /// `.partial` staging dir plus an unusable directory at the
    /// canonical path — must not poison the next attempt: a retry
    /// clears both and re-clones from the recorded origin.
    #[tokio::test]
    async fn retry_after_stalled_clone_clears_partial_and_reclones() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git = |cwd: &std::path::Path, args: &[&str]| {
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
        git(&src, &["init", "-q"]);
        git(&src, &["config", "user.email", "t@example.com"]);
        git(&src, &["config", "user.name", "t"]);
        git(&src, &["commit", "--allow-empty", "-q", "-m", "init"]);

        let mgr = WorktreeManager::new(tmp.path().join("base"));
        let bare = mgr.bare_clone_path("acme", "widgets");
        std::fs::create_dir_all(&bare).expect("mkdir bare");
        // The shape a killed clone leaves behind: a config recording the
        // origin (so the retry re-clones from the same remote) inside a
        // directory git can't use, plus a stale half-written staging dir.
        std::fs::write(
            bare.join("config"),
            format!("[remote \"origin\"]\n\turl = {}\n", src.display()),
        )
        .expect("write config");
        let partial = partial_clone_path(&bare);
        std::fs::create_dir_all(&partial).expect("mkdir partial");
        std::fs::write(partial.join("junk"), "half-written pack").expect("write junk");

        let recloned = mgr
            .ensure_bare_clone("acme", "widgets")
            .await
            .expect("retry re-clones from the recorded origin");
        assert_eq!(recloned, bare);
        assert!(
            bare_repo_health(default_git_runner(), &bare)
                .await
                .expect("probe runs"),
            "retry must leave a healthy bare clone"
        );
        assert!(!partial.exists(), "stale partial must be cleared");
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
        assert!(bare_repo_health(default_git_runner(), &bare).await.unwrap());
    }

    /// B10 (issue #557): when `origin/HEAD` can't be resolved (offline,
    /// or a bare clone that never had it set), `default_branch` still
    /// resolves a repo whose default is the conventional-but-not-
    /// main/master `develop` via the last-resort probe. Origin is
    /// removed so the `set-head --auto` / `symbolic-ref` tiers both fail
    /// and only the probe can answer.
    #[tokio::test]
    async fn default_branch_probes_develop_when_head_unresolved() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir src");
        git(&src, &["init", "-q"]);
        std::fs::write(src.join("f.txt"), "x\n").expect("write");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "init"]);
        // Rename the sole branch to `develop` — whatever git's default
        // init name is, this leaves exactly one head named `develop`.
        git(&src, &["branch", "-m", "develop"]);

        let mgr = WorktreeManager::new(tmp.path().join("base"));
        let bare = mgr.bare_clone_path("acme", "widget");
        std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("mkdir repos");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().expect("utf8"),
                bare.to_str().expect("utf8"),
            ],
        );
        // Drop origin so `origin/HEAD` can never be (re)established; the
        // `refs/heads/develop` probe is the only path left.
        git(&bare, &["remote", "remove", "origin"]);

        let got = mgr
            .default_branch("acme", "widget")
            .await
            .expect("develop must resolve via the fallback probe");
        assert_eq!(got, "develop");
    }

    /// The same path with no resolvable branch at all reports the
    /// offline/auth-flavored guidance the failure modal keys off (B10).
    #[tokio::test]
    async fn default_branch_names_the_likely_cause_when_unresolvable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir src");
        git(&src, &["init", "-q"]);
        std::fs::write(src.join("f.txt"), "x\n").expect("write");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "init"]);
        // An unconventional sole branch: the repo is healthy (so no
        // reclone is attempted) but matches none of the probe guesses.
        git(&src, &["branch", "-m", "wip-unconventional"]);

        let mgr = WorktreeManager::new(tmp.path().join("base"));
        let bare = mgr.bare_clone_path("acme", "widget");
        std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("mkdir repos");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().expect("utf8"),
                bare.to_str().expect("utf8"),
            ],
        );
        git(&bare, &["remote", "remove", "origin"]);

        let err = mgr
            .default_branch("acme", "widget")
            .await
            .expect_err("no branch can resolve");
        let msg = err.to_string();
        assert!(msg.contains("could not resolve default branch"), "{msg}");
        assert!(
            msg.contains("unreachable"),
            "guidance names the cause: {msg}"
        );
    }

    /// git ran and CONFIRMED the directory is not a usable repo
    /// (interrupted-clone shape) → `Ok(false)` — the only verdict that
    /// authorizes delete + reclone.
    #[tokio::test]
    async fn non_repo_directory_probes_ok_false() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("not-a-repo.git");
        std::fs::create_dir(&dir).expect("mkdir");
        assert!(!bare_repo_health(default_git_runner(), &dir).await.unwrap());
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
            bare_repo_health(default_git_runner(), &file).await.is_err(),
            "a probe that couldn't run must propagate an error, not condemn the repo"
        );
    }

    /// The bare-path-free fast probe (`worktree_dir_ready`, used by the
    /// spawn fast path): true only for a completed checkout — a real
    /// worktree with its index landed, or a plain `.git`-dir repo — and
    /// false for the debris shapes that used to pass the old bare
    /// `exists()` check (empty dir, dangling `.git` pointer).
    #[tokio::test]
    async fn worktree_dir_ready_accepts_only_completed_checkouts() {
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
        assert!(worktree_dir_ready(&wt).await, "a real worktree is ready");
        assert!(
            worktree_dir_ready_on_branch(&wt, "wt-branch").await,
            "the branch-aware probe accepts the checked-out branch"
        );
        assert!(
            !worktree_dir_ready_on_branch(&wt, "main").await,
            "structural readiness must not hide a branch mismatch"
        );

        // A detached HEAD is a deliberate reuse-in-place state (#721), not
        // a stale worktree — the probe accepts it so the spawn fast path
        // doesn't re-provision (and re-run setup) a tree it only reuses.
        git(&wt, &["checkout", "-q", "--detach"]);
        assert!(
            worktree_dir_ready_on_branch(&wt, "wt-branch").await,
            "a detached HEAD is reused in place, never reported stale"
        );

        // A standalone repo (`.git` is a directory) is ready too.
        let standalone = tmp.path().join("standalone");
        std::fs::create_dir(&standalone).expect("mkdir");
        git(&standalone, &["init", "-q"]);
        assert!(worktree_dir_ready(&standalone).await);

        // The empty dir an old failed-provision fallback fabricated.
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).expect("mkdir");
        assert!(!worktree_dir_ready(&empty).await);

        // Missing path entirely.
        assert!(!worktree_dir_ready(&tmp.path().join("nope")).await);

        // `.git` pointer whose gitdir target is gone (bare clone
        // deleted from under the worktree).
        let dangling = tmp.path().join("dangling");
        std::fs::create_dir(&dangling).expect("mkdir");
        std::fs::write(
            dangling.join(".git"),
            format!("gitdir: {}\n", tmp.path().join("gone").display()),
        )
        .expect("write pointer");
        assert!(!worktree_dir_ready(&dangling).await);

        // Registered but incomplete: metadata + `.git` file exist, the
        // index never landed (killed `worktree add`).
        let half = tmp.path().join("half");
        git(
            &bare,
            &[
                "worktree",
                "add",
                half.to_str().expect("utf8 path"),
                "-B",
                "half-branch",
                "HEAD",
            ],
        );
        let gitdir_file = std::fs::read_to_string(half.join(".git")).expect("read .git");
        let gitdir = gitdir_file
            .strip_prefix("gitdir:")
            .expect("gitdir pointer")
            .trim();
        std::fs::remove_file(Path::new(gitdir).join("index")).expect("drop index");
        assert!(
            !worktree_dir_ready(&half).await,
            "an interrupted checkout is not ready — the repair path must run"
        );
    }

    /// A `git worktree add` killed mid-checkout (timeout, Esc-cancel)
    /// leaves registered metadata and a `.git` file but no index — a
    /// shape that used to validate `Valid` forever, landing every
    /// later session in a half-populated tree. Validation now repairs
    /// it in place (`reset --hard`): tracked files come back, and
    /// untracked user work — possibly created by a fallback session
    /// spawned after the failed provision — survives.
    #[tokio::test]
    async fn half_checked_out_worktree_is_repaired_in_place() {
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
        // Simulate the kill: the index never landed and a tracked file
        // is missing; an untracked file stands in for user work.
        let gitdir = bare.join("worktrees").join("wt");
        std::fs::remove_file(gitdir.join("index")).expect("drop index");
        std::fs::remove_file(wt.join("f.txt")).expect("drop tracked file");
        std::fs::write(wt.join("user-notes.md"), "keep me").expect("write untracked");

        assert_eq!(
            validate_worktree_dir(default_git_runner(), &wt, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Valid,
            "repair completes the checkout and reuses the worktree"
        );
        assert!(wt.join("f.txt").exists(), "repair restores tracked files");
        assert!(gitdir.join("index").exists(), "repair rebuilds the index");
        assert_eq!(
            std::fs::read_to_string(wt.join("user-notes.md")).expect("still readable"),
            "keep me",
            "repair must not touch untracked user work"
        );
    }

    /// The destructive-repair guard (#512): a half-checked-out worktree
    /// (no index) whose tracked file a fallback session *edited in place*
    /// must NOT be repaired with `reset --hard` — that would discard the
    /// edit. Validation refuses loudly (the edited tree is non-pristine)
    /// and leaves the work untouched, exactly like a dangling-gitdir
    /// dirty leftover.
    #[tokio::test]
    async fn half_checked_out_worktree_with_tracked_edits_is_preserved() {
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
        // Simulate the killed add whose index never landed, then a
        // fallback session editing a TRACKED file (not merely adding an
        // untracked one) — the content `reset --hard` would silently
        // revert to HEAD.
        let gitdir = bare.join("worktrees").join("wt");
        std::fs::remove_file(gitdir.join("index")).expect("drop index");
        std::fs::write(wt.join("f.txt"), "important fallback-session edits\n")
            .expect("edit tracked file");

        let verdict = validate_worktree_dir(default_git_runner(), &wt, &bare).await;
        assert!(
            verdict.is_err(),
            "a tracked-file edit must refuse (got {verdict:?}), never reset --hard it away"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("f.txt")).expect("still readable"),
            "important fallback-session edits\n",
            "refusal must not touch the edited tracked file"
        );
        assert!(
            !gitdir.join("index").exists(),
            "no repair ran, so no index was rebuilt"
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
        assert_eq!(
            resumable_partial_origin(default_git_runner(), &junk).await,
            None
        );

        let no_origin = tmp.path().join("no-origin.partial");
        git(tmp.path(), &["init", "-q", "--bare", "no-origin.partial"]);
        assert_eq!(
            resumable_partial_origin(default_git_runner(), &no_origin).await,
            None
        );

        let staged = tmp.path().join("staged.partial");
        git(tmp.path(), &["init", "-q", "--bare", "staged.partial"]);
        git(
            &staged,
            &["remote", "add", "origin", "git@github.com:acme/widgets.git"],
        );
        assert_eq!(
            resumable_partial_origin(default_git_runner(), &staged).await,
            Some("git@github.com:acme/widgets.git".to_string())
        );
    }

    /// A real worktree of the bare clone validates `Valid`; after the
    /// bare clone's `worktrees/` metadata vanishes (bare deleted /
    /// re-cloned), the same directory must STOP reporting `Valid` —
    /// its `.git` gitdir pointer is dangling and every git command in
    /// it would fail. The checkout is pristine (a failed-provision
    /// leftover), so validation reclaims it for re-provision (#447)
    /// instead of wedging on "move the directory aside and retry".
    #[tokio::test]
    async fn dangling_gitdir_pristine_leftover_is_reclaimed() {
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
            validate_worktree_dir(default_git_runner(), &wt, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Valid,
            "sanity: a live worktree of this bare clone is Valid"
        );

        std::fs::remove_dir_all(bare.join("worktrees")).expect("nuke worktree metadata");

        assert_eq!(
            validate_worktree_dir(default_git_runner(), &wt, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Reprovision,
            "a pristine dangling leftover is reclaimed, never wedged"
        );
        assert!(
            !wt.exists(),
            "reclaim removes the leftover so the caller can re-provision"
        );
    }

    /// The safety half of #447: a dangling-gitdir leftover carrying
    /// genuine uncommitted work (a fallback session's edits, #446) is
    /// NOT pristine — validation must refuse and leave the files
    /// untouched, never mistaking real work for disposable debris.
    #[tokio::test]
    async fn dangling_gitdir_dirty_leftover_is_preserved() {
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
        // A fallback session's uncommitted work: an edit to a tracked
        // file plus a brand-new untracked file.
        std::fs::write(wt.join("f.txt"), "edited by the user\n").expect("edit tracked file");
        std::fs::write(wt.join("notes.md"), "keep me").expect("write untracked");

        std::fs::remove_dir_all(bare.join("worktrees")).expect("nuke worktree metadata");

        let verdict = validate_worktree_dir(default_git_runner(), &wt, &bare).await;
        assert!(
            verdict.is_err(),
            "a dirty dangling leftover must refuse (got {verdict:?}), never delete work"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("notes.md")).expect("still readable"),
            "keep me",
            "refusal must not touch the user's uncommitted work"
        );
    }

    /// `.gitignore`d build debris is not "real work": a pristine
    /// checkout that only grew ignored files (a `target/` a session
    /// built) is still reclaimable, so ignored artifacts never wedge a
    /// re-provision.
    #[tokio::test]
    async fn dangling_gitdir_ignored_debris_is_reclaimed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir src");
        git(&src, &["init", "-q"]);
        std::fs::write(src.join("f.txt"), "content\n").expect("write f.txt");
        std::fs::write(src.join(".gitignore"), "target/\n").expect("write gitignore");
        git(&src, &["add", "."]);
        git(&src, &["commit", "-q", "-m", "init"]);
        git(tmp.path(), &["clone", "-q", "--bare", "src", "bare.git"]);
        let bare = tmp.path().join("bare.git");

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
        std::fs::create_dir(wt.join("target")).expect("mkdir target");
        std::fs::write(wt.join("target").join("out.o"), "built").expect("write artifact");

        std::fs::remove_dir_all(bare.join("worktrees")).expect("nuke worktree metadata");

        assert_eq!(
            validate_worktree_dir(default_git_runner(), &wt, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Reprovision,
            "only-ignored-debris leftover is pristine and reclaimable"
        );
    }

    /// A ref that doesn't peel to a tree — a blob-valued tag, as
    /// git.git's own `refs/tags/junio-gpg-pub` — must not disable
    /// recovery. The pristine probe peels only `refs/heads` /
    /// `refs/remotes`, so such a ref is never in the batch that would
    /// otherwise fail wholesale and refuse every pristine leftover.
    #[tokio::test]
    async fn blob_valued_tag_does_not_break_recovery() {
        let (tmp, bare) = local_bare_clone();
        // A tag pointing straight at a blob (no commit to peel through).
        let blob = {
            let out = std::process::Command::new("git")
                .current_dir(&bare)
                .args(["hash-object", "-w", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn hash-object");
            use std::io::Write;
            out.stdin.as_ref().unwrap().write_all(b"gpg key").unwrap();
            let out = out.wait_with_output().expect("hash-object");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&bare, &["update-ref", "refs/tags/blob-tag", &blob]);

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
        std::fs::remove_dir_all(bare.join("worktrees")).expect("nuke worktree metadata");

        assert_eq!(
            validate_worktree_dir(default_git_runner(), &wt, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Reprovision,
            "a non-peelable tag must not wedge recovery of a pristine leftover"
        );
    }
}

#[cfg(test)]
mod resilient_add_tests {
    use super::*;

    fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-c")
            .arg("user.email=test@example.com")
            .arg("-c")
            .arg("user.name=test")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .current_dir(cwd)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git must be runnable in tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `<tmp>/src` repo with one commit, cloned to `<tmp>/bare.git`.
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

    /// The parser pulls the holding worktree's path out of both the
    /// modern and the older git refusal wording, and yields `None` for
    /// unrelated failures.
    #[test]
    fn parses_the_holder_path() {
        let modern = GitError::Command(
            "Preparing worktree\nfatal: 'feat' is already used by worktree at \
             '/repo.git/.claude/worktrees/agent-abc'\n"
                .into(),
        );
        assert_eq!(
            branch_already_checked_out_at(&modern),
            Some(PathBuf::from("/repo.git/.claude/worktrees/agent-abc"))
        );
        let older = GitError::Command(
            "fatal: 'feat' is already checked out at '/some/other/tree'\n".into(),
        );
        assert_eq!(
            branch_already_checked_out_at(&older),
            Some(PathBuf::from("/some/other/tree"))
        );
        // The branch-reset guard's distinct wording — note the branch
        // name is itself quoted before the path, so the parser must
        // anchor on the marker, not the first quote in the message.
        let reset_guard = GitError::Command(
            "Preparing worktree (resetting branch 'feat'; was at 6790fc7)\n\
             fatal: cannot force update the branch 'feat' used by worktree at \
             '/repo.git/.claude/worktrees/agent-abc'\n"
                .into(),
        );
        assert_eq!(
            branch_already_checked_out_at(&reset_guard),
            Some(PathBuf::from("/repo.git/.claude/worktrees/agent-abc"))
        );
        let unrelated = GitError::Command("fatal: invalid reference: refs/heads/feat\n".into());
        assert_eq!(branch_already_checked_out_at(&unrelated), None);
    }

    /// The headline #439 case: a nested Claude Code agent worktree
    /// (living *inside* the bare clone) already holds the branch.
    /// Provisioning must resolve it with `--force` — landing lazybox's
    /// worktree on the branch — rather than failing hard, and it must
    /// leave the agent worktree's files untouched.
    #[tokio::test]
    async fn nested_agent_worktree_holding_the_branch_is_forced_through() {
        let (tmp, bare) = local_bare_clone();
        let nested = bare.join(".claude").join("worktrees").join("agent-abc");
        git(
            &bare,
            &[
                "worktree",
                "add",
                nested.to_str().unwrap(),
                "-B",
                "feat",
                "HEAD",
            ],
        );
        std::fs::write(nested.join("agent-work.txt"), "in progress").expect("write agent file");

        let target = tmp.path().join("target");
        add_worktree_resilient(default_git_runner(), &bare, &target, "feat", "HEAD", &[])
            .await
            .expect("nested agent collision must resolve, not fail");

        assert!(target.join(".git").exists(), "target is a real worktree");
        assert_eq!(
            validate_worktree_dir(default_git_runner(), &target, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Valid,
            "the forced worktree checked out on the branch"
        );
        assert_eq!(
            std::fs::read_to_string(nested.join("agent-work.txt")).expect("still readable"),
            "in progress",
            "the agent worktree's files are left in place"
        );
    }

    /// A *stale* registration — a worktree whose directory is gone
    /// (leftover lazybox worktree, reaped agent run) — is cleared by
    /// the `prune` + retry step alone, without needing `--force`.
    #[tokio::test]
    async fn stale_registration_is_pruned_then_retried() {
        let (tmp, bare) = local_bare_clone();
        let ghost = tmp.path().join("ghost");
        git(
            &bare,
            &[
                "worktree",
                "add",
                ghost.to_str().unwrap(),
                "-B",
                "feat",
                "HEAD",
            ],
        );
        // Drop the directory but leave git's registration behind.
        std::fs::remove_dir_all(&ghost).expect("remove ghost dir");
        assert!(
            branch_already_checked_out_at(
                &run_git_transfer(
                    &bare,
                    &[
                        "worktree",
                        "add",
                        "-B",
                        "feat",
                        tmp.path().join("probe").to_str().unwrap(),
                        "HEAD",
                    ],
                    &[],
                    None,
                )
                .await
                .expect_err("sanity: the stale registration still blocks a plain add")
            )
            .is_some(),
            "sanity: git reports the branch as still checked out"
        );

        let target = tmp.path().join("target");
        add_worktree_resilient(default_git_runner(), &bare, &target, "feat", "HEAD", &[])
            .await
            .expect("prune must clear the stale registration and let the retry win");
        assert_eq!(
            validate_worktree_dir(default_git_runner(), &target, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Valid
        );
    }

    /// A *live* worktree that is NOT nested inside the bare clone (e.g.
    /// another lazybox session on the same branch) is not force-stolen:
    /// the caller gets a clear, actionable error and the existing
    /// worktree is left intact.
    #[tokio::test]
    async fn live_external_holder_degrades_with_a_clear_error() {
        let (tmp, bare) = local_bare_clone();
        let external = tmp.path().join("external");
        git(
            &bare,
            &[
                "worktree",
                "add",
                external.to_str().unwrap(),
                "-B",
                "feat",
                "HEAD",
            ],
        );

        let target = tmp.path().join("target");
        let err = add_worktree_resilient(default_git_runner(), &bare, &target, "feat", "HEAD", &[])
            .await
            .expect_err("a live external holder must not be silently stolen from");
        assert!(
            matches!(
                &err,
                GitError::BranchHeldLive { branch, holder }
                    if branch == "feat"
                        && canonical_or_self(holder) == canonical_or_self(&external)
            ),
            "the caller needs the holder path to decide whether it is reclaimable: {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("already checked out at"), "{msg}");
        assert!(msg.contains("external"), "error names the holder: {msg}");
        assert!(
            external.join("f.txt").exists(),
            "the existing worktree is untouched"
        );
        assert!(
            !target.exists(),
            "no half-provisioned target is left behind"
        );
    }

    /// A live holder that sits *inside the bare directory* but NOT under
    /// the `.claude/worktrees/` agent root (#512) must not be force-shared
    /// by the old bare-path `starts_with` heuristic: only genuine Claude
    /// Code agent worktrees qualify. A checkout elsewhere in the bare
    /// tree gets the same clear refusal as an external holder, and its
    /// files are left intact.
    #[tokio::test]
    async fn nested_non_agent_holder_is_not_force_shared() {
        let (tmp, bare) = local_bare_clone();
        // Inside the bare directory, but not the agent-worktree root.
        let rogue = bare.join("rogue-checkout");
        git(
            &bare,
            &[
                "worktree",
                "add",
                rogue.to_str().unwrap(),
                "-B",
                "feat",
                "HEAD",
            ],
        );
        std::fs::write(rogue.join("f.txt"), "someone else's work\n").expect("edit rogue file");

        let target = tmp.path().join("target");
        let err = add_worktree_resilient(default_git_runner(), &bare, &target, "feat", "HEAD", &[])
            .await
            .expect_err("a non-agent holder inside the bare dir must not be force-stolen");
        let msg = err.to_string();
        assert!(msg.contains("already checked out at"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(rogue.join("f.txt")).expect("still readable"),
            "someone else's work\n",
            "the non-agent holder's work is left untouched"
        );
        assert!(
            !target.exists(),
            "no half-provisioned target is left behind"
        );
    }

    /// The matcher recognizes git's same-path refusal (a registered
    /// worktree whose directory was `rm -rf`'d by hand) and stays quiet
    /// on everything else.
    #[test]
    fn recognizes_missing_but_registered_worktree() {
        let missing = GitError::Command(
            "fatal: '/tmp/wt' is a missing but already registered worktree;\n\
             use 'add -f' to override, or 'prune' or 'remove' to clear\n"
                .into(),
        );
        assert!(worktree_missing_but_registered(&missing));
        let unrelated = GitError::Command("fatal: invalid reference: refs/heads/feat\n".into());
        assert!(!worktree_missing_but_registered(&unrelated));
        assert!(!worktree_missing_but_registered(&GitError::Io(
            std::io::Error::other("io")
        )));
    }

    /// Manual `rm -rf` of a worktree, then a re-provision at the SAME
    /// path. When the branch being added matches the stale
    /// registration's, git's branch-collision wording fires and the
    /// existing prune rung recovers. But when the derived branch has
    /// CHANGED in between (issue title edit, issue→PR attach) git
    /// instead refuses with "'<path>' is a missing but already
    /// registered worktree" — a path-shaped message the branch matcher
    /// never caught, so the prune-and-retry ladder used to be skipped
    /// entirely and the session stayed unprovisionable. Both shapes
    /// must recover.
    #[tokio::test]
    async fn rm_rfed_worktree_readded_at_same_path_recovers() {
        let (tmp, bare) = local_bare_clone();
        let target = tmp.path().join("target");
        add_worktree_resilient(default_git_runner(), &bare, &target, "feat", "HEAD", &[])
            .await
            .expect("initial provision");
        // Simulate the user's manual `rm -rf`: the directory is gone,
        // the registration in `<bare>/worktrees/` survives.
        std::fs::remove_dir_all(&target).expect("rm -rf the worktree");

        // Re-add on a DIFFERENT branch → the "missing but already
        // registered worktree" refusal, the shape the new matcher owns.
        add_worktree_resilient(
            default_git_runner(),
            &bare,
            &target,
            "feat-renamed",
            "HEAD",
            &[],
        )
        .await
        .expect("re-add at the same path must prune the stale registration and retry");
        assert_eq!(
            validate_worktree_dir(default_git_runner(), &target, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Valid,
            "the recovered directory is a real worktree again"
        );

        // And the same-branch shape (branch matcher + prune rung)
        // keeps working.
        std::fs::remove_dir_all(&target).expect("rm -rf again");
        add_worktree_resilient(
            default_git_runner(),
            &bare,
            &target,
            "feat-renamed",
            "HEAD",
            &[],
        )
        .await
        .expect("same-branch re-add also recovers");
        assert_eq!(
            validate_worktree_dir(default_git_runner(), &target, &bare)
                .await
                .unwrap(),
            WorktreeDirState::Valid,
        );
    }
}

#[cfg(test)]
mod track_main_tests {
    use super::*;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-c")
            .arg("user.email=test@example.com")
            .arg("-c")
            .arg("user.name=test")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("init.defaultBranch=main")
            .current_dir(cwd)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .expect("git must be runnable in tests");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn head(cwd: &Path) -> String {
        git(cwd, &["rev-parse", "HEAD"])
    }

    /// A `src` repo on `main` (one commit), bare-cloned into the
    /// manager's canonical path with `src` as its `origin`, and a
    /// lazybox worktree cut on branch `scratch` off `main`. Returns
    /// (tmp guard, manager, src path, worktree path).
    async fn tracked_worktree() -> (tempfile::TempDir, WorktreeManager, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).expect("mkdir src");
        git(&src, &["init", "-q"]);
        git(&src, &["branch", "-M", "main"]);
        std::fs::write(src.join("f.txt"), "c1\n").expect("write");
        git(&src, &["add", "f.txt"]);
        git(&src, &["commit", "-q", "-m", "c1"]);

        let mgr = WorktreeManager::new(tmp.path().join("base"));
        let bare = mgr.bare_clone_path("acme", "widgets");
        std::fs::create_dir_all(bare.parent().expect("bare parent")).expect("mkdir repos");
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().expect("utf8"),
                bare.to_str().expect("utf8"),
            ],
        );

        let wt = tmp.path().join("wt");
        mgr.checkout_new_branch_at(&wt, "acme", "widgets", "scratch", "main")
            .await
            .expect("provision scratch worktree");
        (tmp, mgr, src, wt)
    }

    /// Advance `src`'s `main` by one commit — the upstream moving ahead
    /// of the scratch worktree.
    fn advance_main(src: &Path, body: &str) {
        std::fs::write(src.join("f.txt"), body).expect("write");
        git(src, &["add", "f.txt"]);
        git(src, &["commit", "-q", "-m", "advance"]);
    }

    /// Clean worktree behind main → fast-forwarded onto `origin/main`.
    #[tokio::test]
    async fn clean_behind_worktree_fast_forwards() {
        let (_tmp, mgr, src, wt) = tracked_worktree().await;
        advance_main(&src, "c2\n");

        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(outcome, TrackSyncOutcome::FastForwarded);
        // The worktree branch now points at the advanced main tip.
        let origin_main = git(&wt, &["rev-parse", "refs/remotes/origin/main"]);
        assert_eq!(head(&wt), origin_main, "worktree HEAD advanced to main");
        // And the working file reflects the fast-forwarded content.
        assert_eq!(
            std::fs::read_to_string(wt.join("f.txt")).expect("read"),
            "c2\n"
        );
    }

    /// Already up to date → no-op, no error.
    #[tokio::test]
    async fn up_to_date_worktree_is_noop() {
        let (_tmp, mgr, _src, wt) = tracked_worktree().await;
        let before = head(&wt);
        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(outcome, TrackSyncOutcome::UpToDate);
        assert_eq!(head(&wt), before, "HEAD unchanged when already synced");
    }

    /// Behind main but the tree has uncommitted changes → skipped, never
    /// touched.
    #[tokio::test]
    async fn dirty_worktree_is_skipped() {
        let (_tmp, mgr, src, wt) = tracked_worktree().await;
        advance_main(&src, "c2\n");
        let before = head(&wt);
        // Uncommitted edit in the worktree.
        std::fs::write(wt.join("f.txt"), "work in progress\n").expect("write");

        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(outcome, TrackSyncOutcome::SkippedDirty);
        assert!(outcome.is_behind());
        assert_eq!(head(&wt), before, "HEAD untouched on a dirty tree");
        assert_eq!(
            std::fs::read_to_string(wt.join("f.txt")).expect("read"),
            "work in progress\n",
            "the uncommitted edit is preserved"
        );
    }

    /// Behind main AND carrying a local commit (diverged) → skipped;
    /// a fast-forward is impossible and the local commit must survive.
    #[tokio::test]
    async fn diverged_worktree_is_skipped() {
        let (_tmp, mgr, src, wt) = tracked_worktree().await;
        // Local commit on scratch...
        std::fs::write(wt.join("local.txt"), "mine\n").expect("write");
        git(&wt, &["add", "local.txt"]);
        git(&wt, &["commit", "-q", "-m", "local work"]);
        let before = head(&wt);
        // ...while main also advances upstream.
        advance_main(&src, "c2\n");

        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(outcome, TrackSyncOutcome::SkippedDiverged);
        assert!(outcome.is_behind());
        assert_eq!(head(&wt), before, "diverged HEAD is left intact");
        assert!(
            wt.join("local.txt").exists(),
            "the local commit's file survives"
        );
    }

    /// A detached HEAD (the user or an agent shell inspecting a commit,
    /// a bisect step) must never be advanced: `merge --ff-only` happily
    /// moves a detached HEAD without advancing the branch, yanking the
    /// checkout out from under whoever detached it.
    #[tokio::test]
    async fn detached_head_worktree_is_never_advanced() {
        let (_tmp, mgr, src, wt) = tracked_worktree().await;
        let before = head(&wt);
        git(&wt, &["checkout", "-q", "--detach"]);
        advance_main(&src, "c2\n");

        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(outcome, TrackSyncOutcome::SkippedUnsafeState);
        assert!(
            outcome.is_behind(),
            "a refused sync must not render as up to date"
        );
        assert_eq!(head(&wt), before, "detached HEAD is left exactly in place");
        assert_eq!(
            std::fs::read_to_string(wt.join("f.txt")).expect("read"),
            "c1\n",
            "the working tree is untouched"
        );
    }

    /// A branch already checked out in another live worktree can still be
    /// provisioned as a *detached* checkout of its head (#721): rather
    /// than dead-ending the PR's workspace, the new tree lands on the same
    /// commit without contending for the branch name, so the holder keeps
    /// sole ownership of the branch and its files untouched. A later
    /// idempotent provision then reuses the detached tree in place instead
    /// of erroring on the "wrong branch".
    #[tokio::test]
    async fn held_branch_provisions_as_detached_head() {
        let (tmp, mgr, _src, holder) = tracked_worktree().await;
        // `holder` is a live worktree holding branch `scratch`.
        let holder_tip = head(&holder);
        std::fs::write(holder.join("WIP.txt"), "holder work\n").expect("write WIP");

        // A second workspace wants the same branch. A plain `checkout_at`
        // would collide (the branch is held live); the detached provision
        // sidesteps it.
        let detached_wt = tmp.path().join("detached");
        let wt = mgr
            .checkout_pr_head_detached(&detached_wt, "acme", "widgets", "scratch", None)
            .await
            .expect("detached provision must succeed despite the live holder");
        assert_eq!(wt.path, detached_wt);
        assert_eq!(wt.branch, "scratch", "the logical branch is still recorded");
        assert_eq!(
            head(&detached_wt),
            holder_tip,
            "the detached tree lands on the branch head"
        );
        let symref = std::process::Command::new("git")
            .current_dir(&detached_wt)
            .args(["symbolic-ref", "-q", "HEAD"])
            .output()
            .expect("git runs");
        assert!(
            !symref.status.success(),
            "the new tree is on a detached HEAD, not the branch"
        );

        // The holder still owns `scratch`, its work untouched.
        assert_eq!(
            git(&holder, &["symbolic-ref", "--short", "HEAD"]),
            "scratch"
        );
        assert_eq!(
            std::fs::read_to_string(holder.join("WIP.txt")).expect("read"),
            "holder work\n"
        );

        // Idempotent re-entry: a plain `checkout_at` reuses the detached
        // tree in place rather than erroring on a branch mismatch.
        let again = mgr
            .checkout_at(&detached_wt, "acme", "widgets", "scratch", None)
            .await
            .expect("re-entry reuses the detached worktree in place");
        assert_eq!(again.path, detached_wt);
        assert_eq!(
            head(&detached_wt),
            holder_tip,
            "re-entry left HEAD in place"
        );
    }

    /// A bisect in progress (HEAD may still be on the branch before the
    /// first good/bad answer) must also refuse — the marker file, not
    /// just detachment, gates the sync.
    #[tokio::test]
    async fn bisecting_worktree_is_never_advanced() {
        let (_tmp, mgr, src, wt) = tracked_worktree().await;
        let before = head(&wt);
        git(&wt, &["bisect", "start"]);
        advance_main(&src, "c2\n");

        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(outcome, TrackSyncOutcome::SkippedUnsafeState);
        assert_eq!(head(&wt), before, "mid-bisect HEAD is left in place");

        // Once the bisect ends, the same worktree syncs normally again.
        git(&wt, &["bisect", "reset"]);
        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(outcome, TrackSyncOutcome::FastForwarded);
    }

    /// Untracked debris (build output, scratch notes) is exactly what a
    /// scratch worktree accumulates — it must NOT count as dirty and
    /// block the fast-forward, or the feature never syncs in practice.
    #[tokio::test]
    async fn untracked_file_does_not_block_fast_forward() {
        let (_tmp, mgr, src, wt) = tracked_worktree().await;
        advance_main(&src, "c2\n");
        // A stray untracked file the FF doesn't need to touch.
        std::fs::write(wt.join("build.log"), "noise\n").expect("write");

        let outcome = mgr
            .fast_forward_to_base(&wt, "acme", "widgets", "main")
            .await
            .expect("sync runs");
        assert_eq!(
            outcome,
            TrackSyncOutcome::FastForwarded,
            "untracked files must not be treated as dirty"
        );
        let origin_main = git(&wt, &["rev-parse", "refs/remotes/origin/main"]);
        assert_eq!(
            head(&wt),
            origin_main,
            "worktree advanced despite untracked file"
        );
        assert!(
            wt.join("build.log").exists(),
            "the untracked file is preserved through the fast-forward"
        );
    }

    /// Issue #787: `existing_worktree_branch` reads a managed worktree's
    /// own branch, so the provisioner can reuse it instead of deriving a
    /// colliding one. A missing path (or one that isn't a worktree of the
    /// bare clone) reports `None`.
    #[tokio::test]
    async fn existing_worktree_branch_reads_the_checked_out_branch() {
        let (tmp, mgr, _src, wt) = tracked_worktree().await;
        assert_eq!(
            mgr.existing_worktree_branch("acme", "widgets", &wt)
                .await
                .expect("read branch")
                .as_deref(),
            Some("scratch"),
        );
        let missing = tmp.path().join("nope");
        assert_eq!(
            mgr.existing_worktree_branch("acme", "widgets", &missing)
                .await
                .expect("missing path is not an error"),
            None,
        );

        // A directory that exists and is a git repo but is NOT a worktree
        // of this bare clone must report `None` — the provisioner then
        // derives a fresh branch rather than adopting a foreign checkout's.
        let foreign = tmp.path().join("foreign");
        std::fs::create_dir(&foreign).expect("mkdir foreign");
        git(&foreign, &["init", "-q"]);
        assert_eq!(
            mgr.existing_worktree_branch("acme", "widgets", &foreign)
                .await
                .expect("foreign checkout is not an error"),
            None,
        );
    }

    /// Issue #787: preserving a conflicting worktree aside keeps its
    /// (uncommitted) files under a `.bak-<n>` sibling and frees the branch
    /// it held, so a fresh provision on that same branch succeeds where it
    /// would previously have hit `BranchHeldLive` / `BranchMismatch`.
    #[tokio::test]
    async fn preserve_worktree_aside_keeps_files_and_frees_the_branch() {
        let (_tmp, mgr, _src, wt) = tracked_worktree().await;
        let bare = mgr.bare_path("acme", "widgets");
        std::fs::write(wt.join("wip.txt"), "unsaved\n").expect("write wip");

        let backup = mgr
            .preserve_worktree_aside(&bare, &wt)
            .await
            .expect("preserve runs")
            .expect("a backup was created");
        assert!(!wt.exists(), "the conflicting worktree moved aside");
        assert_eq!(
            std::fs::read_to_string(backup.join("wip.txt")).expect("read backup"),
            "unsaved\n",
            "uncommitted work is preserved, not deleted",
        );

        // The branch `scratch` is no longer checked out anywhere, so a
        // fresh worktree add on it succeeds.
        mgr.checkout_new_branch_at(&wt, "acme", "widgets", "scratch", "main")
            .await
            .expect("re-provision on the freed branch");
        assert_eq!(
            mgr.existing_worktree_branch("acme", "widgets", &wt)
                .await
                .expect("read branch")
                .as_deref(),
            Some("scratch"),
        );

        // A second preserve counts up rather than clobbering the first.
        let backup2 = mgr
            .preserve_worktree_aside(&bare, &wt)
            .await
            .expect("preserve runs")
            .expect("second backup");
        assert_ne!(backup, backup2, "backups never collide");
    }
}
