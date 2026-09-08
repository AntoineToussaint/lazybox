//! Shared test scaffolding for `lazybox-tui`'s integration binaries.
//!
//! Add `mod common;` to any `tests/*.rs` that persists `ui.*` state —
//! directly or through a dispatched action. That one line installs a
//! before-main `#[ctor]` redirecting `LAZYBOX_HOME` to a throwaway dir, so
//! the persist can't rewrite the developer's real `~/.lazybox/config.yaml`
//! (#1539), plus a guard test that fails *this* binary if the redirect
//! ever stops working.
//!
//! Why a copy of `src/lib.rs`'s `config_sandbox` rather than a shared
//! helper: integration binaries link the non-`cfg(test)` library as an
//! external crate, so a shared helper would have to be `pub` — an
//! env-mutating function on the production API surface. Keeping the body
//! here confines that footgun to test code. Keep the two copies identical.
//!
//! Only for *write-only* persists. A test that reads the config back must
//! sandbox per-test under a lock (see `ConfigHome` in `tests/sidebar.rs`),
//! because every thread in the binary shares this one sandbox dir. This is
//! also why persisting binaries with read-back tests (sidebar) stay on
//! that stricter mechanism instead of adding `mod common;`: the guard's
//! `save_with_async` would write their home outside the serialization lock.

// A binary may use only the ctor and not reference the guard's helpers, or
// vice versa; don't warn on the unused half in any given binary.
#![allow(dead_code)]

/// Point `LAZYBOX_HOME` at a throwaway dir so any `ui.*` persist lands
/// there instead of the developer's real config. Unique per process run
/// (pid + start nanos) so a recycled pid can't make a later run read a
/// stale sandbox. Shared across the binary's test threads — write-only
/// safe; read-back tests must sandbox per-test (see the module docs).
fn install() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "lazybox-tui-config-sandbox-{}-{}",
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

/// Guard against a regression of #1539 in this binary: a config persist
/// must resolve to the sandbox, never the real config.
///
/// Hermetic by design — it never reads the real file. A dev box runs a
/// live daemon that rewrites `~/.lazybox/config.yaml` on its own, so a
/// before/after fingerprint of the real file would flake and wrongly blame
/// the code under test. Instead it asserts the resolved path is redirected
/// and that the write actually lands there.
#[test]
fn config_persistence_stays_in_the_sandbox() {
    let real = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"))
        .join(".lazybox")
        .join("config.yaml");
    let sandbox = lazybox_config::Config::default_path();
    assert_ne!(
        sandbox, real,
        "LAZYBOX_HOME redirect is not active — persists would rewrite the real config"
    );

    lazybox_config::Config::save_with_async(|_| {});
    // 30s, not a tight bound: this box runs many agents at once and can sit
    // at 100% CPU, so 5s flakes under load; a genuinely stuck worker still
    // fails.
    assert!(
        lazybox_config::Config::flush_pending_saves(std::time::Duration::from_secs(30)),
        "the async config-save worker did not flush within 30s"
    );
    assert!(
        sandbox.exists(),
        "persist did not write the sandbox path — the redirect may point at an unwritable dir"
    );
}
