//! lazybox-tui-core — pure-logic modules used by the TUI client.
//!
//! Ratatui-free by design. The TUI's render-heavy modules
//! (Sidebar, RightPane, TerminalStack, model) live in `lazybox-tui`;
//! everything that doesn't need a render context — latches,
//! intent state machines, agent-attention tracking, editor
//! discovery, platform shims, setup helpers, test-mode harness —
//! lives here so edits to those modules don't trigger a full
//! lazybox-tui rebuild.

pub mod action;
pub mod agent_attention;

/// Agent metadata the render layer needs — the built-in registry (display
/// names + badge glyphs) and the prompt-trim helper — re-exported so the
/// UI library reaches them through `lazybox-tui-core` instead of depending
/// on `lazybox-agents` directly. That keeps the UI lib's dependency set to
/// `{ipc, tui-core, tui-term, config, core}` (#548): agent internals live
/// one crate away, behind this gateway.
pub mod agents {
    pub use lazybox_agents::{Registry, claude_ambient_model, registry, trim_leading_blank_lines};
}
pub mod choice;
pub mod confirm_latch;
pub mod dispatch;
pub mod editors;
pub mod epic_graph;
pub mod help;
pub mod inbox;
pub mod intent;
pub mod markers;
pub mod notify;
pub mod platform;
pub mod pr_chat;
pub mod prompts;
pub mod remote;
pub mod snippets;
pub mod theme;
pub mod tips;
pub mod usage;
pub mod util;

// Test-only config sandbox for this unit-test binary (#1539, #1751). The
// same body lives in the crate's `tests/common/mod.rs` and in every other
// test binary that can reach `lazybox-config`; `crates/core/tests/
// test_isolation.rs` requires one per binary, and a shared helper would
// put an env-mutating function on a production API surface. Keep the
// copies in step; the git half is carried only by the crates that spawn
// git.
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
            "lazybox-tui-core-config-sandbox-{}-{}",
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
