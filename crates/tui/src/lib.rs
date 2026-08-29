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
