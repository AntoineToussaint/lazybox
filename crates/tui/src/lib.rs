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

// Test-only config sandbox for the lib unit-test binary. The identical
// redirect for the crate's *integration* binaries lives in
// `tests/common/mod.rs`: it can't be shared from here because those
// binaries link the non-`cfg(test)` library as an external crate, so the
// only way to share the body would be a `pub` helper — which would put an
// env-mutating function on the production API surface (#1539). Keeping a
// `#[cfg(test)]` copy here confines that footgun to test code. The two
// copies are deliberately kept identical; mirror any edit.
#[cfg(test)]
mod config_sandbox {
    /// Point `LAZYBOX_HOME` at a throwaway dir so any `ui.*` persist lands
    /// there instead of the developer's real `~/.lazybox/config.yaml`.
    ///
    /// A persist resolves its destination through `LAZYBOX_HOME` in
    /// `lazybox-config`, a *different* crate — so `cfg(test)` is never
    /// active there for a `lazybox-tui` test run and cannot redirect the
    /// write. A test that toggles a pin / collapse / focus therefore
    /// reaches `Config::default_path()` = the real config and rewrites it
    /// (#1539). Installed from a before-main `#[ctor]`, the one hook that
    /// beats the test harness to every test.
    ///
    /// The dir is unique per process run (pid + start nanos) so a recycled
    /// pid can never make a later run read a stale sandbox. It is shared by
    /// every test thread in the binary, so it is safe only for *write-only*
    /// persists — a test that reads the config back must sandbox per-test
    /// under a lock instead (see `ConfigHome` in `tests/sidebar.rs`).
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

    /// Guard against a regression of #1539: a config persist run by this
    /// binary must resolve to the sandbox, never the real config.
    ///
    /// The proof is hermetic — it never reads the real file. A dev box
    /// runs a live daemon that rewrites `~/.lazybox/config.yaml` on its own
    /// every ~40s, so a before/after fingerprint of the real file would
    /// flake (and wrongly blame the code under test). Instead it asserts
    /// the resolved path is redirected and that the write actually lands
    /// there — which is what proves a persist can't reach the real config.
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

        // The shape every sidebar toggle uses: an async keystroke-persist.
        lazybox_config::Config::save_with_async(|_| {});
        // 30s, not a tight bound: this box runs many agents at once and can
        // sit at 100% CPU, so 5s flakes under load; a genuinely stuck
        // worker still fails.
        assert!(
            lazybox_config::Config::flush_pending_saves(std::time::Duration::from_secs(30)),
            "the async config-save worker did not flush within 30s"
        );
        assert!(
            sandbox.exists(),
            "persist did not write the sandbox path — the redirect may point at an unwritable dir"
        );
    }
}
