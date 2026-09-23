//! `lb` — small executable alias for the sibling `lazybox` binary.
//!
//! Keeping this as an exec shim avoids shipping a second copy of the full TUI
//! while preserving the convenient command name in Cargo and release archives.

use std::path::{Path, PathBuf};
use std::process::Command;

fn lazybox_sibling(current_exe: &Path, args: &[std::ffi::OsString]) -> PathBuf {
    // An optional side-by-side mobile build keeps the normal release intact.
    // Source/release bundles without that alternate use the same main binary.
    let mobile = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "-m" || arg == "--mobile");
    let alternate = current_exe.with_file_name("lazybox-mobile");
    if mobile && alternate.is_file() {
        alternate
    } else {
        current_exe.with_file_name("lazybox")
    }
}

#[cfg(unix)]
fn main() {
    use std::os::unix::process::CommandExt;

    let current = std::env::current_exe().unwrap_or_else(|error| {
        eprintln!("lb: could not locate this executable: {error}");
        std::process::exit(126);
    });
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let lazybox = lazybox_sibling(&current, &args);
    let error = Command::new(&lazybox).args(&args).exec();
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
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let lazybox = lazybox_sibling(&current, &args);
    match Command::new(&lazybox).args(&args).status() {
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
    fn mobile_flag_selects_optional_side_by_side_build() {
        let dir = tempfile::tempdir().unwrap();
        let lb = dir.path().join("lb");
        let default = dir.path().join("lazybox");
        let alternate = dir.path().join("lazybox-mobile");
        let args = |items: &[&str]| {
            items
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(lazybox_sibling(&lb, &args(&["-m"])), default);
        std::fs::write(&alternate, "fixture").unwrap();
        for flag in ["-m", "--mobile"] {
            assert_eq!(lazybox_sibling(&lb, &args(&[flag, "--connect"])), alternate);
        }
        assert_eq!(lazybox_sibling(&lb, &args(&["--", "-m"])), default);
        assert_eq!(lazybox_sibling(&lb, &args(&["--version"])), default);
    }

    #[test]
    fn resolves_alias_next_to_main_binary() {
        assert_eq!(
            lazybox_sibling(Path::new("/tmp/release/lb"), &[]),
            Path::new("/tmp/release/lazybox")
        );
    }
}

// Test-only sandbox for this unit-test binary (#1539, #1751). The same
// body lives in the crate's `tests/common/mod.rs` and in every other test
// binary that can reach `lazybox-config` or spawn git; `crates/core/tests/
// test_isolation.rs` requires one per binary, and a shared helper would
// put an env-mutating function on a production API surface. Keep the
// copies in step; the git half is carried only by the crates that spawn
// git.
#[cfg(test)]
mod config_sandbox {
    /// Unique per process run (pid + start nanos) so a recycled pid can never
    /// make a later run read a stale sandbox.
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
        // SAFETY: a `#[ctor]` runs before `main`, while the process is still
        // single-threaded and no other initializer in this binary spawns a
        // thread — so nothing can race this env write.
        unsafe { std::env::set_var("LAZYBOX_HOME", &dir) };
        lazybox_config::Config::invalidate_cache();
        isolate_git(&dir);
    }

    /// Point every git this binary spawns — fixture commands and the code
    /// under test alike — at a sandbox global config, with no system config.
    /// Two things the developer's real config brings that a test must not:
    /// a signing setup that hangs on a locked agent, and git's own background
    /// work. `fetch` and `commit` (since 2.29) and, on current git, `clone`
    /// fork a *detached* `git maintenance run --auto` that outlives the
    /// command and keeps repacking objects and holding `maintenance.lock` in
    /// the fixture repo — a local `clone --bare` then fails mid-copy on a box
    /// loaded enough for the two to overlap (#1751). Env is inherited, so the
    /// sandbox reaches the git that production code runs under the test too.
    ///
    /// The value the variable held right after the write is recorded in
    /// `GIT_SANDBOX`, so the guard test can prove the redirect landed at
    /// process start without reading the live environment — a sibling test
    /// may legitimately have swapped it under its own lock by then.
    fn isolate_git(sandbox: &std::path::Path) {
        let gitconfig = sandbox.join("gitconfig");
        let _ = std::fs::write(
            &gitconfig,
            "[commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n\
             [maintenance]\n\tauto = false\n[gc]\n\tauto = 0\n",
        );
        // SAFETY: called from the before-main `#[ctor]` below, while the
        // process is still single-threaded.
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &gitconfig);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        }
        let _ =
            GIT_SANDBOX.set(std::env::var_os("GIT_CONFIG_GLOBAL").map(std::path::PathBuf::from));
    }

    /// `GIT_CONFIG_GLOBAL` as read back inside the ctor: `None` if the write
    /// never landed, `Some(path)` otherwise.
    static GIT_SANDBOX: std::sync::OnceLock<Option<std::path::PathBuf>> =
        std::sync::OnceLock::new();

    #[ctor::ctor]
    unsafe fn redirect_config_home() {
        install();
    }

    /// The redirect must actually be in force in this binary. Hermetic — the
    /// real file is never read, and the check holds under a sibling test's own
    /// pinned home too, since that is not the real profile either.
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

    /// Every git this binary spawns started out reading the sandbox config,
    /// not the developer's. Asserted from what the ctor recorded, never from
    /// the live environment: a sibling test that pins its own gitconfig under
    /// its own lock would otherwise decide this test's outcome by schedule.
    /// Auto-maintenance is the setting a fixture cannot afford to inherit, so
    /// it is the one checked in the file.
    #[test]
    fn git_in_this_binary_reads_the_sandbox_config() {
        let gitconfig = GIT_SANDBOX
            .get()
            .expect("the ctor ran before this test")
            .as_ref()
            .expect("GIT_CONFIG_GLOBAL was unset right after the ctor wrote it");
        assert!(
            gitconfig.to_string_lossy().contains("-config-sandbox-"),
            "GIT_CONFIG_GLOBAL was {} at process start, not the sandbox gitconfig",
            gitconfig.display()
        );
        let body = std::fs::read_to_string(gitconfig)
            .unwrap_or_else(|err| panic!("read {}: {err}", gitconfig.display()));
        assert!(
            body.contains("[maintenance]\n\tauto = false"),
            "the sandbox gitconfig does not switch auto-maintenance off:\n{body}"
        );
    }
}
