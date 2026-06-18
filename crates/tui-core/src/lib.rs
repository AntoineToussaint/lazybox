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
pub mod confirm_latch;
pub mod editors;
pub mod intent;
pub mod notify;
pub mod platform;
pub mod prompts;
pub mod tips;
pub mod util;
