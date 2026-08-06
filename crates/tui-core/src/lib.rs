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
    pub use lazybox_agents::{Registry, registry, trim_leading_blank_lines};
}
pub mod choice;
pub mod confirm_latch;
pub mod dispatch;
pub mod editors;
pub mod help;
pub mod inbox;
pub mod intent;
pub mod markers;
pub mod notify;
pub mod platform;
pub mod prompts;
pub mod snippets;
pub mod theme;
pub mod tips;
pub mod util;
