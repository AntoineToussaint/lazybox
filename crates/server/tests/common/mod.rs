//! Shared scaffolding for `lazybox-server`'s integration binaries.
//!
//! `mod common;` in a `tests/*.rs` links in a before-main `#[ctor]` that
//! points `LAZYBOX_HOME` at a throwaway dir, so nothing the binary runs
//! can read or rewrite the developer's real `~/.lazybox/config.yaml`
//! (#1539, #1751), and points every git it spawns at a sandbox global
//! config (#1751), plus a guard test that fails *this* binary if the
//! redirect ever stops working. `crates/core/tests/test_isolation.rs`
//! requires it in every integration binary of a crate that depends on
//! `lazybox-config` or spawns git.
//!
//! A copy rather than a shared helper: integration binaries link the
//! non-`cfg(test)` library as an external crate, so sharing the body would
//! put an env-mutating function on the production API surface. Keep the
//! copies across the workspace in step; the git half is carried only by
//! the crates that spawn git.
//!
//! One sandbox dir is shared by every test thread in the binary, so a test
//! that needs a home of its own still pins one per test under a lock; the
//! ctor is the floor beneath it, never the real profile.

/// Unique per process run (pid + start nanos) so a recycled pid can never
/// make a later run read a stale sandbox.
fn install() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "lazybox-server-config-sandbox-{}-{}",
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
    let _ = GIT_SANDBOX.set(std::env::var_os("GIT_CONFIG_GLOBAL").map(std::path::PathBuf::from));
}

/// `GIT_CONFIG_GLOBAL` as read back inside the ctor: `None` if the write
/// never landed, `Some(path)` otherwise.
static GIT_SANDBOX: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

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
