//! Shared scaffolding for `lazybox-relay`'s integration binaries.
//!
//! `mod common;` in a `tests/*.rs` links in a before-main `#[ctor]` that
//! points `LAZYBOX_HOME` at a throwaway dir, so nothing the binary runs
//! can read or rewrite the developer's real `~/.lazybox/config.yaml`
//! (#1539, #1751), plus a guard test that fails *this* binary if the
//! redirect ever stops working. `crates/core/tests/test_isolation.rs`
//! requires it in every integration binary of a crate that depends on
//! `lazybox-config`.
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
        "lazybox-relay-config-sandbox-{}-{}",
        std::process::id(),
        nanos
    ));
    let _ = std::fs::create_dir_all(&dir);
    // SAFETY: a `#[ctor]` runs before `main`, while the process is still
    // single-threaded and no other initializer in this binary spawns a
    // thread — so nothing can race this env write.
    unsafe { std::env::set_var("LAZYBOX_HOME", &dir) };
    lazybox_config::Config::invalidate_cache();
}

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
