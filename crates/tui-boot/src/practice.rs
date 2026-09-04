//! `lazybox practice` — a sandboxed, reactive practice simulator (#1459).
//!
//! Practice mode is a living lazybox a new user can learn in: a full inbox
//! where agents work, CI flips and an agent asks a question — and where every
//! key can be pressed with no consequence. No GitHub, no credentials, no
//! network, no real worktrees, and — the part that must not be got wrong —
//! **nothing written to the real `~/.lazybox`**.
//!
//! This module owns that last guarantee. The simulated world itself (the
//! seeded inbox, the mock-backed daemon, the reactor) is the same machinery
//! `--demo` uses, in [`crate::scenario`]; practice adds the isolation and the
//! permanent chrome.
//!
//! ## The isolation lever
//!
//! Every durable path lazybox resolves — `config.yaml`, `state.db`, worktrees,
//! scrollback — hangs off `lazybox_core::paths::home()`, which reads
//! `LAZYBOX_HOME` (see [`crate`] docs). [`PracticeSandbox`] points that env var
//! at a throwaway temp directory for the life of the process, so the client's
//! own config writes (starring, pinning, theme, `recent_snippets`, …) — which
//! never go through the store — land in the sandbox, not the user's real
//! profile. On drop the temp dir is deleted and the previous value restored,
//! so after any practice session the real `~/.lazybox` is byte-identical.

use std::ffi::OsString;

use tempfile::TempDir;

/// Redirects every `LAZYBOX_HOME`-derived path at a throwaway directory for
/// the life of a practice session, then restores the prior environment and
/// deletes the directory on drop.
///
/// Must be entered *before* the practice daemon or client loads or saves any
/// config: the config cache is keyed by resolved path, so a snapshot read
/// before the swap belongs to the real profile and is a cache miss afterwards
/// — but a *write* issued before the swap would hit the real file. In the
/// `lazybox practice` entry point nothing touches config before this runs.
pub struct PracticeSandbox {
    /// Deleted on drop.
    _dir: TempDir,
    /// `LAZYBOX_HOME` as it was before we entered, restored on drop.
    previous: Option<OsString>,
}

impl PracticeSandbox {
    /// Create the sandbox directory and repoint `LAZYBOX_HOME` at it.
    pub fn enter() -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("lazybox-practice-")
            .tempdir()
            .map_err(|e| anyhow::anyhow!("create practice sandbox: {e}"))?;
        let previous = std::env::var_os(ENV);
        // SAFETY: process-wide env mutation. This runs once at the top of the
        // `practice` entry point, on the main thread before the daemon or any
        // config-touching thread starts, so no other thread observes the swap
        // mid-flight — the same contract the config test-suite relies on.
        unsafe {
            std::env::set_var(ENV, dir.path());
        }
        Ok(Self {
            _dir: dir,
            previous,
        })
    }

    /// The sandbox home the session is confined to.
    #[cfg(test)]
    pub fn home(&self) -> &std::path::Path {
        self._dir.path()
    }
}

impl Drop for PracticeSandbox {
    fn drop(&mut self) {
        // Quiesce the background config-persist worker BEFORE touching the env
        // var. That worker resolves `LAZYBOX_HOME` (`Config::default_path()` →
        // getenv) on its own thread each time it runs a queued save; restoring
        // the var here is a setenv, and a concurrent getenv/setenv is undefined
        // behaviour (why `set_var` is unsafe). A practice star enqueues such a
        // save, so a star-then-quit could race the restore. Draining the queue
        // first parks the worker in `recv()` — not in getenv — so the restore
        // is race-free. Bounded so shutdown stays prompt.
        let _ = lazybox_config::Config::flush_pending_saves(std::time::Duration::from_secs(2));
        // SAFETY: see `enter`, plus the flush above removes the only concurrent
        // env reader. Restore exactly what was there before — an unset var must
        // go back to unset, not to an empty string (which `home()` treats as
        // "no override", but which would still leak as a set-but-empty var to
        // child processes).
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var(ENV, value),
                None => std::env::remove_var(ENV),
            }
        }
        // `_dir` drops here → the temp directory is removed.
    }
}

const ENV: &str = "LAZYBOX_HOME";

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the shared `LAZYBOX_HOME` env var so these tests don't race each
    /// other or the config-crate suites.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn sandbox_redirects_paths_then_restores() {
        let _guard = env_lock();
        // SAFETY: single-threaded test, guarded by env_lock.
        unsafe { std::env::set_var(ENV, "/tmp/lazybox-practice-real-home") };

        let sandbox = PracticeSandbox::enter().expect("enter sandbox");
        let home = sandbox.home().to_path_buf();
        assert!(home.exists(), "sandbox dir exists while entered");
        // Every derived path now resolves under the sandbox, not the real home.
        let config = lazybox_core::paths::config_yaml();
        assert!(
            config.starts_with(&home),
            "config.yaml resolves inside the sandbox: {config:?}"
        );
        assert!(
            lazybox_core::paths::state_db().starts_with(&home),
            "state.db resolves inside the sandbox"
        );

        drop(sandbox);
        // The prior value is restored and the directory is gone.
        assert_eq!(
            std::env::var_os(ENV).as_deref(),
            Some(std::ffi::OsStr::new("/tmp/lazybox-practice-real-home")),
            "prior LAZYBOX_HOME restored on drop"
        );
        assert!(!home.exists(), "sandbox dir deleted on drop");

        // SAFETY: single-threaded test cleanup.
        unsafe { std::env::remove_var(ENV) };
    }

    #[test]
    fn sandbox_restores_an_unset_var_to_unset() {
        let _guard = env_lock();
        // SAFETY: single-threaded test, guarded by env_lock.
        unsafe { std::env::remove_var(ENV) };

        let sandbox = PracticeSandbox::enter().expect("enter sandbox");
        assert!(std::env::var_os(ENV).is_some(), "set while entered");
        drop(sandbox);
        assert!(
            std::env::var_os(ENV).is_none(),
            "an unset LAZYBOX_HOME goes back to unset, not empty"
        );
    }
}
