//! Serialization for tests that touch the process-global environment.
//!
//! `HOME` and `LAZYBOX_HOME` are process-wide, and every
//! [`lazybox_core::paths`] helper re-reads them on each call. Under cargo's
//! default parallel execution a test that redirects either one flips the
//! state root out from under a sibling test in the same binary — between the
//! call that derived a path and the assertion that checks where it landed.
//! Every test in this binary that writes those vars, or that compares a value
//! against a `paths::*` root, holds this lock for its whole body.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Take the environment lock for the caller's whole body. Poisoning is
/// ignored — the test that panicked has already failed, and the guards
/// restore what they set while unwinding.
pub fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the environment lock with `LAZYBOX_HOME` pointed at a throwaway
/// directory, so `paths::state_root()` is fixed for the guard's lifetime
/// (and never the developer's real profile).
pub struct PinnedStateRoot {
    _lock: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    prev: Option<OsString>,
}

impl PinnedStateRoot {
    pub fn enter() -> Self {
        let _lock = lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os(ENV);
        // SAFETY: the lock we hold excludes every other environment reader
        // and writer in this test binary until this guard drops.
        unsafe { std::env::set_var(ENV, dir.path()) };
        Self {
            _lock,
            _dir: dir,
            prev,
        }
    }
}

impl Drop for PinnedStateRoot {
    fn drop(&mut self) {
        // SAFETY: as in `enter` — still holding the lock.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(ENV, v),
                None => std::env::remove_var(ENV),
            }
        }
    }
}

const ENV: &str = "LAZYBOX_HOME";
