//! Lazybox components ported to tuirealm.
//!
//! Each module here corresponds to one pane or modal from the old
//! `crate::components::*` tree. The render bodies are largely copied
//! from the originals; the trait surface changes from
//! `tui_kit::Pane`/`Modal` to `tuirealm::Component` + `AppComponent`.

pub mod choice;
pub mod confirm;
pub mod error;
pub mod focus_header;
pub mod footer;
pub mod help;
pub mod help_ask;
pub mod input;
pub mod jump_picker;
pub mod loading;
pub mod polling;
pub mod right;
pub mod sidebar;
pub mod snippet_browser;
pub mod snippet_picker;
pub mod splash;
pub mod sync_status;
pub mod terminals;
pub mod textarea;
pub mod tour;
pub mod which_key;
pub mod worktree_progress;
