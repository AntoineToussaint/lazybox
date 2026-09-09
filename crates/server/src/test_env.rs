//! Serialization for tests that touch the process-global environment.
//!
//! `LAZYBOX_HOME` is process-wide, and every [`lazybox_core::paths`] helper
//! re-reads it on each call. Under cargo's default parallel execution a test
//! that redirects it — and then deletes the directory it pointed at — moves the
//! runtime dir out from under a sibling test in the same binary, mid-write.
//! Every test in this binary that redirects the variable, or that writes under
//! a path derived from it, holds this lock for its whole body.
//!
//! Mirrors `lazybox-tui-boot`'s `test_env`; a shared helper would put an
//! env-mutating type on a production API surface, and `cfg(test)` never
//! crosses crates anyway.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

const ENV: &str = "LAZYBOX_HOME";

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Holds the environment lock with `LAZYBOX_HOME` pointed at a throwaway
/// directory, so every path derived from it is fixed for the guard's lifetime
/// (and never the developer's real profile).
pub struct PinnedHome {
    _lock: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    prev: Option<OsString>,
}

impl PinnedHome {
    /// Poisoning is ignored — the test that panicked has already failed, and
    /// this guard restores what it set while unwinding.
    pub fn enter() -> Self {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os(ENV);
        // SAFETY: the lock we hold excludes every other environment reader and
        // writer in this test binary until this guard drops.
        unsafe { std::env::set_var(ENV, dir.path()) };
        Self {
            _lock,
            _dir: dir,
            prev,
        }
    }
}

impl Drop for PinnedHome {
    fn drop(&mut self) {
        // SAFETY: as in `enter` — still holding the lock.
        unsafe {
            match &self.prev {
                Some(prev) => std::env::set_var(ENV, prev),
                None => std::env::remove_var(ENV),
            }
        }
    }
}
