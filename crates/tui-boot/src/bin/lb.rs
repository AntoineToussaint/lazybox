//! `lb` — small executable alias for the sibling `lazybox` binary.
//!
//! Keeping this as an exec shim avoids shipping a second copy of the full TUI
//! while preserving the convenient command name in Cargo and release archives.

use std::path::{Path, PathBuf};
use std::process::Command;

fn lazybox_sibling(current_exe: &Path) -> PathBuf {
    current_exe.with_file_name("lazybox")
}

#[cfg(unix)]
fn main() {
    use std::os::unix::process::CommandExt;

    let current = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("lb: could not locate this executable: {error}");
        std::process::exit(126);
    });
    let lazybox = lazybox_sibling(&current);
    let error = Command::new(&lazybox)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("lb: could not execute {}: {error}", lazybox.display());
    std::process::exit(if error.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    });
}

#[cfg(not(unix))]
fn main() {
    let current = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("lb: could not locate this executable: {error}");
        std::process::exit(126);
    });
    let lazybox = lazybox_sibling(&current);
    match Command::new(&lazybox)
        .args(std::env::args_os().skip(1))
        .status()
    {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => {
            eprintln!("lb: could not run {}: {error}", lazybox.display());
            std::process::exit(126);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_alias_next_to_main_binary() {
        assert_eq!(
            lazybox_sibling(Path::new("/tmp/release/lb")),
            Path::new("/tmp/release/lazybox")
        );
    }
}

// Test-only config sandbox for this unit-test binary (#1539, #1751). The
// same body lives in the crate's `tests/common/mod.rs` and in every other
// test binary that can reach `lazybox-config`; `crates/core/tests/
// test_isolation.rs` requires one per binary, and a shared helper would
// put an env-mutating function on a production API surface. Keep the
// copies identical.
#[cfg(test)]
mod config_sandbox {
    /// Point `LAZYBOX_HOME` at a throwaway dir so every `Config::load()` in
    /// this test binary resolves to defaults instead of the developer's
    /// real config. Installed from a before-main `#[ctor]`, the one hook
    /// that beats the test harness to every test. Unique per process run
    /// (pid + start nanos) so a recycled pid can never make a later run
    /// read a stale sandbox.
    fn install() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "lazybox-lb-bin-config-sandbox-{}-{}",
            std::process::id(),
            nanos
        ));
        let _ = std::fs::create_dir_all(&dir);
        // SAFETY: a `#[ctor]` runs before `main`, while the process is
        // still single-threaded and no other initializer in this binary
        // spawns a thread — so nothing can race this env write.
        unsafe { std::env::set_var("LAZYBOX_HOME", &dir) };
        lazybox_config::Config::invalidate_cache();
    }

    #[ctor::ctor]
    unsafe fn redirect_config_home() {
        install();
    }

    /// The redirect must actually be in force in this binary. Hermetic —
    /// the real file is never read, and the check holds under a sibling
    /// test's own pinned home too, since that is not the real profile
    /// either.
    #[test]
    fn config_path_resolves_to_a_sandbox_not_the_real_home() {
        let real = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"))
            .join(".lazybox")
            .join("config.yaml");
        assert_ne!(
            lazybox_config::Config::default_path(),
            real,
            "LAZYBOX_HOME redirect is not active — this binary can reach the real config"
        );
    }
}
