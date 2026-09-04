//! Lazybox components ported to tuirealm.
//!
//! Each module here corresponds to one pane or modal from the old
//! `crate::components::*` tree. The render bodies are largely copied
//! from the originals; the trait surface changes from
//! `tui_kit::Pane`/`Modal` to `tuirealm::Component` + `AppComponent`.

pub mod choice;
pub mod confirm;
pub mod diff_review;
pub mod editors_panel;
pub mod error;
pub mod error_inbox;
pub mod filterable;
pub mod focus_header;
pub mod footer;
pub mod help;
pub mod help_ask;
pub mod hopper;
pub mod input;
pub mod issue_browser;
pub mod jump_picker;
pub mod loading;
pub mod markdown_modal;
pub mod merge_history_modal;
pub mod messages;
pub mod polling;
pub mod pr_chat;
pub mod prompt_history_picker;
pub mod right;
pub mod scrollable;
pub mod settings;
pub mod sidebar;
pub mod snippet_browser;
pub mod snippet_picker;
pub mod splash;
pub mod stats;
pub mod sync_status;
pub mod terminals;
pub mod textarea;
pub mod which_key;
pub mod worktree_progress;
