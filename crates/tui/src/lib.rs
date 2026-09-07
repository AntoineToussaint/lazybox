//! lazybox-tui — the client TUI: realm-based component tree, key
//! routing, event dispatch, rendering.
//!
//! Built on `tuirealm` 4.1 (which sits on `ratatui`); modal /
//! component / orchestrator types live under `crate::realm`. Lazybox's
//! domain components (Sidebar, RightPane, TerminalStack, Mailbox,
//! activity-feed renderers, status pills) live under
//! `crate::components`.

// Cosmetic clippy 1.95 suppressions — same shape as lazybox-server.
// Doc-list-indentation, manual-strip, collapsible-match,
// nonminimal-bool, ptr-arg, manual-strip suggestions are pedantic
// style; the let-else→? rewrite makes affected sites less
// readable. Each can be re-enabled in a focused cleanup pass.
#![allow(
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::empty_line_after_doc_comments,
    clippy::manual_strip,
    clippy::collapsible_match,
    clippy::collapsible_if,
    clippy::nonminimal_bool,
    clippy::unnecessary_get_then_check,
    clippy::redundant_closure,
    clippy::needless_late_init
)]

pub mod build_guard;
pub mod components;
pub mod lazybox_theme;
pub mod notify_coalesce;
pub mod pane;
pub mod perf;
pub mod realm;
pub mod sandbox_flow;
pub mod setup;
pub mod setup_flow;
pub mod theme;

// ── re-exported from lazybox-tui-core ─────────────────────────────
// These modules used to live here; they were extracted into
// `lazybox-tui-core` so edits to (say) `intent.rs` don't trigger a
// lazybox-tui rebuild. Re-exported at the same paths so existing
// `lazybox_tui::intent::Foo` / `crate::intent::Foo` keeps resolving.
pub use lazybox_tui_core::{
    agent_attention, confirm_latch, editors, intent, notify, platform, prompts, util,
};

pub use pane::{Binding, PaneId, PaneOutcome};
pub use theme::Theme;

/// Point `LAZYBOX_HOME` at a throwaway dir for the lifetime of a test
/// binary, so any config persist lands there instead of the developer's
/// real `~/.lazybox/config.yaml`.
///
/// Persisting a `ui.*` edit resolves its destination through
/// `LAZYBOX_HOME` in `lazybox-config`, a different crate — so `cfg(test)`
/// there is never active for a `lazybox-tui` test run and cannot redirect
/// the write. A test that toggles a pin / collapse / focus therefore
/// reaches `Config::default_path()` = the real config and rewrites it
/// (#1539). Each test binary installs this from a before-main `#[ctor]`,
/// the one hook that beats the test harness to every test. It is
/// `#[doc(hidden)]`/`pub` only so the crate's integration binaries — which
/// don't share the lib's `#[cfg(test)]` code — can call the same body;
/// production never calls it.
#[doc(hidden)]
pub fn __sandbox_config_home_for_tests() {
    let dir = std::env::temp_dir().join(format!(
        "lazybox-tui-config-sandbox-{}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&dir);
    // SAFETY: runs from a `#[ctor]` before the test harness spawns any
    // thread, so no concurrent env access races this write.
    unsafe { std::env::set_var("LAZYBOX_HOME", &dir) };
    lazybox_config::Config::invalidate_cache();
}

#[cfg(test)]
mod config_sandbox {
    #[ctor::ctor]
    unsafe fn redirect_config_home() {
        super::__sandbox_config_home_for_tests();
    }

    /// The real config path, computed straight from `$HOME` so it is
    /// independent of the `LAZYBOX_HOME` redirect above.
    fn real_config_path() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"))
            .join(".lazybox")
            .join("config.yaml")
    }

    fn fingerprint(path: &std::path::Path) -> Option<(u64, std::time::SystemTime)> {
        let m = std::fs::metadata(path).ok()?;
        Some((m.len(), m.modified().ok()?))
    }

    /// Guard against a regression of #1539: no config persist run by this
    /// binary may touch the developer's real `~/.lazybox/config.yaml`.
    #[test]
    fn config_persistence_stays_in_the_sandbox() {
        let real = real_config_path();
        let sandbox_path = lazybox_config::Config::default_path();
        assert_ne!(
            sandbox_path, real,
            "LAZYBOX_HOME redirect is not active — unit tests would rewrite the real config"
        );

        let before = fingerprint(&real);
        // A representative keystroke-persist (the shape every sidebar
        // toggle uses). Under the redirect it must land in the sandbox.
        lazybox_config::Config::save_with_async(|_| {});
        assert!(
            lazybox_config::Config::flush_pending_saves(std::time::Duration::from_secs(5)),
            "pending config saves must flush within the bound"
        );

        assert!(
            sandbox_path.exists(),
            "persist did not write the sandbox path — redirect may be a no-op"
        );
        assert_eq!(
            fingerprint(&real),
            before,
            "a config persist modified the real ~/.lazybox/config.yaml"
        );
    }
}
