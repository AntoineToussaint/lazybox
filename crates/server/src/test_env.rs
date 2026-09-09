//! Serialization for tests that touch the process-global environment.
//!
//! `HOME` and `LAZYBOX_HOME` are process-wide, and every
//! [`lazybox_core::paths`] helper re-reads them on each call. Under cargo's
//! default parallel execution a test that redirects either one — and, for
//! `LAZYBOX_HOME`, deletes the directory it pointed at — moves a path out from
//! under a sibling test in the same binary, mid-write. Every test in this
//! binary that redirects either variable, or that writes under a path derived
//! from one, holds this lock for its whole body; the guard below is the
//! `LAZYBOX_HOME` case, and [`lock`] serves the rest.
//!
//! Mirrors `lazybox-tui-boot`'s `test_env`; a shared helper would put an
//! env-mutating type on a production API surface, and `cfg(test)` never
//! crosses crates anyway.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

const ENV: &str = "LAZYBOX_HOME";

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Take the environment lock for the caller's whole body. Poisoning is
/// ignored — the test that panicked has already failed, and the guards restore
/// what they set while unwinding.
pub fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the environment lock with `LAZYBOX_HOME` pointed at a throwaway
/// directory, so every path derived from it is fixed for the guard's lifetime
/// (and never the developer's real profile). `HOME` is left alone — holding
/// the lock is what keeps a sibling from moving it.
pub struct PinnedHome {
    _lock: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    prev: Option<OsString>,
}

impl PinnedHome {
    pub fn enter() -> Self {
        let _lock = lock();
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
        // SAFETY: as in `enter` — `Drop::drop` runs before the fields, so the
        // restore happens while the lock is still held.
        unsafe {
            match &self.prev {
                Some(prev) => std::env::set_var(ENV, prev),
                None => std::env::remove_var(ENV),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// A redirect that skips the lock reintroduces exactly the race this
    /// module exists to close, and it does so silently — the victim is
    /// whichever sibling test happened to read the variable in that window, so
    /// the failure surfaces somewhere else entirely. Scan the crate rather
    /// than trusting each new test to remember.
    #[test]
    fn every_env_redirect_in_this_crate_takes_the_lock() {
        // `test_env.rs` is the lock. `lib.rs` redirects from a `#[ctor]` that
        // runs before `main` while the process is still single-threaded, so
        // there is no concurrent reader to race — the one case where skipping
        // the lock is provably safe.
        const EXEMPT: [&str; 2] = ["test_env.rs", "lib.rs"];
        let redirects = [
            "set_var(\"HOME\"",
            "remove_var(\"HOME\"",
            "set_var(\"LAZYBOX_HOME\"",
            "remove_var(\"LAZYBOX_HOME\"",
        ];

        let mut unguarded = Vec::new();
        for file in rust_sources(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")) {
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            if EXEMPT.contains(&name.as_ref()) {
                continue;
            }
            let body = std::fs::read_to_string(&file).expect("read source");
            if redirects.iter().any(|needle| body.contains(needle)) && !body.contains("test_env::")
            {
                unguarded.push(file);
            }
        }

        assert!(
            unguarded.is_empty(),
            "these files redirect HOME/LAZYBOX_HOME without taking \
             `crate::test_env::lock()`: {unguarded:?}"
        );
    }

    fn rust_sources(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(rust_sources(&path));
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
        found
    }
}
