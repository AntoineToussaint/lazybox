//! Thin re-export shim. The sidebar's visible-row builder — grouping,
//! sort, filter, scoped search, and the per-repo summaries — moved to
//! the client-free `lazybox_tui_core::inbox` module (#731) so the
//! desktop client builds the same tree from the same code. Call sites
//! keep their `crate::components::visible_rows::*` import paths.

pub use lazybox_tui_core::inbox::{
    ComputeInputs, ComputeOutcome, compute_visible, group_label, project_label, search_matches,
    search_scope_covers,
};
