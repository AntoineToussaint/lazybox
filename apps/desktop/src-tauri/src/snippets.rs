//! The desktop's state-of-record for the snippet picker (#734).
//!
//! The catalog is loaded once from `lazybox_config` (built-in → global →
//! launch-directory precedence); the "Recent" MRU is owned by the daemon
//! (#548) and reduced from the gateway control stream — seeded from every
//! `Snapshot` and advanced on every `SnippetDelivered`, so the desktop's
//! Recent group stays in sync with the in-process TUI. On each keystroke
//! the frontend asks for a recomputed view, produced entirely by the
//! shared `lazybox_tui_core::snippets` logic (grouping / filter / recent
//! float / auto-submit) — the TypeScript picker reimplements none of it.

use lazybox_tui_core::snippets::{PickerRow, SnippetPickerView, compute_picker_view};

/// Newest-first cap on the retained MRU, matching the daemon's
/// `RECENT_SNIPPETS_MAX` so a locally-advanced Recent group never shows
/// more entries than the daemon persists.
const RECENT_SNIPPETS_MAX: usize = 5;

/// Catalog rows (key-sorted) plus the daemon-provided MRU key list.
pub struct SnippetModel {
    catalog: Vec<PickerRow>,
    recent: Vec<String>,
}

impl SnippetModel {
    pub fn new(catalog: Vec<PickerRow>) -> Self {
        Self {
            catalog,
            recent: Vec::new(),
        }
    }

    /// Replace the MRU from a daemon `Snapshot` — the authoritative,
    /// persisted list. Replayed on every reconnect.
    pub fn seed_recent(&mut self, recent: Vec<String>) {
        self.recent = recent;
    }

    /// Advance the MRU locally when any client delivers a snippet: move
    /// the key to the front, de-duplicate, and cap. Mirrors the daemon's
    /// `record_recent_snippet` so the two stay aligned between snapshots.
    pub fn record_recent(&mut self, key: String) {
        self.recent.retain(|k| k != &key);
        self.recent.insert(0, key);
        self.recent.truncate(RECENT_SNIPPETS_MAX);
    }

    /// The grouped, filtered picker view the frontend renders.
    pub fn view(&self, filter: &str) -> SnippetPickerView {
        compute_picker_view(&self.catalog, filter, &self.recent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_config::{Snippet, SnippetOrigin};
    use lazybox_tui_core::snippets::RECENT_CAT;

    fn row(key: &str, category: &str) -> PickerRow {
        PickerRow::new(
            key,
            &Snippet {
                description: format!("{key} desc"),
                category: category.into(),
                body: format!("{key} body"),
                skill: None,
                provider: None,
                origin: SnippetOrigin::Global,
            },
        )
    }

    fn model() -> SnippetModel {
        // Key-sorted, as `Snippets::all()` (a BTreeMap walk) delivers.
        SnippetModel::new(vec![
            row("deploy", "Chores"),
            row("pr", "Git & PR"),
            row("rev", "Review"),
        ])
    }

    #[test]
    fn empty_filter_groups_the_catalog_by_category() {
        let view = model().view("");
        assert_eq!(view.total, 3);
        let cats: Vec<_> = view.groups.iter().map(|g| g.label.clone()).collect();
        assert_eq!(cats, vec!["Review", "Git & PR", "Chores"]);
        assert!(view.auto_submit.is_none());
    }

    #[test]
    fn filter_narrows_and_reports_auto_submit() {
        let view = model().view("rev");
        assert_eq!(view.auto_submit.as_deref(), Some("rev"));
        assert_eq!(view.visible_count, 1);
    }

    #[test]
    fn seeded_recent_floats_to_a_recent_group() {
        let mut m = model();
        m.seed_recent(vec!["pr".into()]);
        let view = m.view("");
        assert_eq!(view.groups[0].category, RECENT_CAT);
        assert_eq!(view.groups[0].rows[0].key, "pr");
    }

    #[test]
    fn delivered_snippet_advances_the_mru_deduped_and_capped() {
        let mut m = model();
        for key in ["deploy", "pr", "rev", "pr"] {
            m.record_recent(key.to_string());
        }
        // "pr" re-used → front; no duplicate.
        let view = m.view("");
        let recent = &view.groups[0];
        assert_eq!(recent.category, RECENT_CAT);
        let keys: Vec<_> = recent.rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(keys, vec!["pr", "rev", "deploy"]);
    }

    #[test]
    fn stale_recent_key_is_dropped_from_the_group() {
        let mut m = model();
        m.seed_recent(vec!["ghost".into(), "rev".into()]);
        let view = m.view("");
        assert_eq!(view.groups[0].category, RECENT_CAT);
        let keys: Vec<_> = view.groups[0].rows.iter().map(|r| r.key.clone()).collect();
        assert_eq!(
            keys,
            vec!["rev"],
            "a key absent from the catalog is ignored"
        );
    }
}
