//! `lb` — short alias for the `lazybox` binary.
//!
//! Same entrypoint as `lazybox`: this re-uses `main.rs` verbatim via a
//! `#[path]` include so the two binaries can never drift. `main.rs`
//! refers to the library through the `lazybox_tui::` path (no `crate::`),
//! so it compiles unchanged inside this separate bin target.
#[path = "../main.rs"]
mod lazybox_main;

fn main() -> anyhow::Result<()> {
    lazybox_main::main()
}
