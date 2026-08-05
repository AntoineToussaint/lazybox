//! Client-free snippet-picker logic: the row view-model, category
//! grouping, the case-insensitive substring filter, the "Recent" MRU
//! float, and the unique-prefix auto-submit fast path (`]]srev`).
//!
//! Lifted out of the ratatui `SnippetPicker` (#734) so both the TUI and
//! the desktop select snippets through the exact same algorithm — the
//! two clients cannot drift on which row a filter resolves to, when the
//! fast path fires, or how the Recent group floats. The TUI keeps its
//! cursor / scroll / theming (`category_color` stays `Theme`-coupled);
//! this module owns everything that does not need a render context.
//!
//! The serializable [`PickerRow`] / [`SnippetPickerView`] pair is what
//! the desktop `src-tauri` emits to its TypeScript picker: it loads the
//! catalog, feeds the daemon-provided MRU, and calls
//! [`compute_picker_view`] on every keystroke to get grouped rows plus
//! the auto-submit target — no picker logic reimplemented in TS.

use lazybox_config::Snippet;

/// A single picker row — the bits the picker needs to render the list,
/// the preview pane, and the filter. Carries the full `body` so a
/// preview pane can show exactly what will be sent.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct PickerRow {
    pub key: String,
    pub description: String,
    pub category: String,
    pub body: String,
    /// Provenance label — `"built-in"` / `"global"` / `"repo"` / `""`.
    /// A plain string (not the `SnippetOrigin` enum) so the row stays
    /// trivially serializable for the desktop; the picker only ever
    /// displays it.
    pub origin: String,
}

impl PickerRow {
    pub fn new(key: &str, snippet: &Snippet) -> Self {
        Self {
            key: key.to_string(),
            description: snippet.description.clone(),
            category: snippet.category.clone(),
            // The preview must show exactly what gets sent, so a
            // skill-dispatching snippet (#798) previews its resolved skill
            // invocation, not the raw authored body.
            body: snippet.dispatch_body(),
            origin: snippet.origin.label().to_string(),
        }
    }
}

/// Header label for the most-recently-used group shown at the top of the
/// picker on an empty filter. A leading space keeps it from colliding
/// with a real user category of the same name in [`category_rank`]
/// (categories never start with whitespace).
pub const RECENT_CAT: &str = " Recent";

/// Category display order: the opinionated built-in groups first (in the
/// order the issue lists them), then any custom user category
/// alphabetically, then the empty "Other" bucket last.
pub fn category_rank(cat: &str) -> u8 {
    match cat {
        "Review" => 0,
        "Git & PR" => 1,
        "GitHub" => 2,
        "Linear" => 3,
        "Testing" => 4,
        "Debugging" => 5,
        "Refactor" => 6,
        "Performance" => 7,
        "Security" => 8,
        "Docs" => 9,
        "Chores" => 10,
        "" => 254, // "Other" bucket, always last
        _ => 200,  // custom categories, before Other, sorted by name
    }
}

/// Label shown for a group header — empty category renders as "Other",
/// and the whitespace-sentinel recent group as "Recent".
pub fn category_label(cat: &str) -> &str {
    match cat {
        "" => "Other",
        RECENT_CAT => "Recent",
        other => other,
    }
}

/// Case-insensitive ASCII prefix check that doesn't allocate.
pub fn starts_with_icase(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    h.len() >= n.len() && h[..n.len()].eq_ignore_ascii_case(n)
}

/// Case-insensitive ASCII substring check that doesn't allocate — the
/// filter re-runs on every keystroke, so we scan bytes rather than
/// lowercasing a fresh `String` per row.
pub fn contains_icase(haystack: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// Resolve a caller's MRU key list (most-recent first) to row indices:
/// drop keys absent from `rows`, keep each key's first (most-recent)
/// position. Used to seed the "Recent" group.
pub fn resolve_recent(rows: &[PickerRow], recent: &[String]) -> Vec<usize> {
    let mut seen = std::collections::HashSet::new();
    recent
        .iter()
        .filter(|k| seen.insert((*k).clone()))
        .filter_map(|k| rows.iter().position(|r| &r.key == k))
        .collect()
}

/// The grouped visible-index list for one filter, plus how many leading
/// entries belong to the floated "Recent" group.
#[derive(Debug, Clone, PartialEq)]
pub struct VisibleList {
    /// Indices into `rows`, in display order (grouped by category rank,
    /// key-sorted within a group), with any Recent rows prepended.
    pub indices: Vec<usize>,
    /// Number of leading `indices` that belong to the "Recent" group —
    /// non-zero only on an empty filter with recents.
    pub recent_count: usize,
}

/// Recompute the visible index list for `filter`, grouped by category in
/// display order. A row matches when the filter is a case-insensitive
/// substring of its key, description, or category — richer than a
/// key-only prefix so snippets are discoverable by what they *do*. On an
/// empty filter the `recent_rows` float into a "Recent" group at the top
/// (they also remain in their real category below — a shortcut, not a
/// move); any filter text steps the group aside. The exact-key
/// auto-submit fast path is decided separately (see [`auto_submit_index`])
/// and is unaffected by description hits.
pub fn compute_visible(rows: &[PickerRow], filter: &str, recent_rows: &[usize]) -> VisibleList {
    let q = filter.trim();
    let mut idxs: Vec<usize> = if q.is_empty() {
        (0..rows.len()).collect()
    } else {
        rows.iter()
            .enumerate()
            .filter_map(|(i, r)| {
                (contains_icase(&r.key, q)
                    || contains_icase(&r.description, q)
                    || contains_icase(&r.category, q))
                .then_some(i)
            })
            .collect()
    };
    // Group by category (headers), keeping rows key-sorted within a group
    // — `rows` already arrives key-sorted, so a stable sort by category
    // rank preserves that.
    idxs.sort_by(|&a, &b| {
        let (ca, cb) = (&rows[a].category, &rows[b].category);
        category_rank(ca)
            .cmp(&category_rank(cb))
            .then_with(|| ca.cmp(cb))
    });
    if q.is_empty() && !recent_rows.is_empty() {
        VisibleList {
            recent_count: recent_rows.len(),
            indices: recent_rows.iter().copied().chain(idxs).collect(),
        }
    } else {
        VisibleList {
            indices: idxs,
            recent_count: 0,
        }
    }
}

/// `Some(idx)` when the picker should auto-submit: the typed filter
/// exactly equals a snippet key AND that key is the *only* snippet whose
/// key starts with the filter. Decided over key-prefix matches alone, so
/// the broader description-inclusive display filter can't suppress the
/// `]rev` fast path (nor make an ambiguous prefix auto-fire).
///
/// Case-tie break: matching is ASCII case-insensitive, so a library
/// defining both `rev` and `Rev` would otherwise make the fast path
/// permanently ambiguous. When several case-insensitive prefix matches
/// exist but exactly one is a case-EXACT full-key match of the typed
/// text, that one wins.
pub fn auto_submit_index(rows: &[PickerRow], filter: &str) -> Option<usize> {
    let q = filter.trim();
    if q.is_empty() {
        return None;
    }
    let prefix_matches: Vec<(usize, &PickerRow)> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| starts_with_icase(&r.key, q))
        .collect();
    match prefix_matches.as_slice() {
        [] => None,
        [(idx, row)] => row.key.eq_ignore_ascii_case(q).then_some(*idx),
        many => {
            let mut exact = many.iter().filter(|(_, r)| r.key == q);
            let &(idx, _) = exact.next()?;
            // Keys are unique (BTreeMap-backed), so a second exact match
            // can't happen — the guard is belt-and-braces.
            exact.next().is_none().then_some(idx)
        }
    }
}

/// A category-grouped run of rows the desktop renderer draws under one
/// header. The floated "Recent" group carries [`RECENT_CAT`] as its
/// `category` and `"Recent"` as its `label`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct SnippetGroup {
    /// Raw category key (empty string for "Other", [`RECENT_CAT`] for the
    /// Recent group). Grouping is by this value.
    pub category: String,
    /// Display label — `"Other"` / `"Recent"` / the category itself.
    pub label: String,
    pub rows: Vec<PickerRow>,
}

/// The serializable snippet picker the desktop emits to its renderer:
/// category-grouped rows in display order, the unique-prefix auto-submit
/// target, and the counts the header shows. Computed by
/// [`compute_picker_view`] from the same primitives the TUI picker uses.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct SnippetPickerView {
    pub filter: String,
    /// Groups in display order (Recent first when present).
    pub groups: Vec<SnippetGroup>,
    /// The snippet key the picker should send without waiting for Enter,
    /// when the filter uniquely identifies one key (`]]srev`).
    pub auto_submit: Option<String>,
    /// Rows visible under the current filter (Recent duplicates counted),
    /// for the `visible/total` header.
    pub visible_count: u32,
    /// Total snippets in the catalog.
    pub total: u32,
}

/// Run the shared grouping/filter/recent/auto-submit pass and package it
/// as the [`SnippetPickerView`] the desktop renders. `rows` is the full
/// catalog (key-sorted); `recent` is the daemon-provided MRU key list,
/// most-recent first.
pub fn compute_picker_view(
    rows: &[PickerRow],
    filter: &str,
    recent: &[String],
) -> SnippetPickerView {
    let recent_rows = resolve_recent(rows, recent);
    let visible = compute_visible(rows, filter, &recent_rows);

    let mut groups: Vec<SnippetGroup> = Vec::new();
    let mut i = 0;
    if visible.recent_count > 0 {
        groups.push(SnippetGroup {
            category: RECENT_CAT.to_string(),
            label: category_label(RECENT_CAT).to_string(),
            rows: visible.indices[..visible.recent_count]
                .iter()
                .map(|&idx| rows[idx].clone())
                .collect(),
        });
        i = visible.recent_count;
    }
    while i < visible.indices.len() {
        let cat = &rows[visible.indices[i]].category;
        let mut j = i;
        while j < visible.indices.len() && &rows[visible.indices[j]].category == cat {
            j += 1;
        }
        groups.push(SnippetGroup {
            category: cat.clone(),
            label: category_label(cat).to_string(),
            rows: visible.indices[i..j]
                .iter()
                .map(|&idx| rows[idx].clone())
                .collect(),
        });
        i = j;
    }

    SnippetPickerView {
        filter: filter.to_string(),
        groups,
        auto_submit: auto_submit_index(rows, filter).map(|idx| rows[idx].key.clone()),
        visible_count: visible.indices.len() as u32,
        total: rows.len() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_config::{Snippet, SnippetOrigin, Snippets};

    fn snip(category: &str, description: &str, body: &str, origin: SnippetOrigin) -> Snippet {
        Snippet {
            description: description.into(),
            category: category.into(),
            body: body.into(),
            skill: None,
            provider: None,
            origin,
        }
    }

    /// Keys already alphabetical, matching the production caller which
    /// hands rows derived from `Snippets::all()` (a BTreeMap walk).
    fn make_rows() -> Vec<PickerRow> {
        vec![
            PickerRow::new(
                "deploy",
                &snip(
                    "Chores",
                    "Pre-deploy check",
                    "deploy check",
                    SnippetOrigin::Repo,
                ),
            ),
            PickerRow::new(
                "pr",
                &snip(
                    "Git & PR",
                    "Open PR",
                    "open pr please",
                    SnippetOrigin::Global,
                ),
            ),
            PickerRow::new(
                "rev",
                &snip(
                    "Review",
                    "Review diff",
                    "review please",
                    SnippetOrigin::Global,
                ),
            ),
        ]
    }

    fn keys(rows: &[PickerRow], visible: &VisibleList) -> Vec<String> {
        visible
            .indices
            .iter()
            .map(|&i| rows[i].key.clone())
            .collect()
    }

    #[test]
    fn empty_filter_groups_by_category_rank() {
        let rows = make_rows();
        let visible = compute_visible(&rows, "", &[]);
        // Review, Git & PR, Chores → rev, pr, deploy.
        assert_eq!(keys(&rows, &visible), vec!["rev", "pr", "deploy"]);
        assert_eq!(visible.recent_count, 0);
    }

    #[test]
    fn filter_matches_key_description_and_category() {
        let rows = make_rows();
        // "diff" appears only in rev's description ("Review diff").
        let visible = compute_visible(&rows, "diff", &[]);
        assert_eq!(keys(&rows, &visible), vec!["rev"]);
        // Category text matches too.
        let visible = compute_visible(&rows, "chores", &[]);
        assert_eq!(keys(&rows, &visible), vec!["deploy"]);
    }

    #[test]
    fn typing_full_key_auto_submits() {
        let rows = make_rows();
        assert_eq!(auto_submit_index(&rows, "r"), None);
        assert_eq!(auto_submit_index(&rows, "re"), None);
        assert_eq!(
            auto_submit_index(&rows, "rev").map(|i| rows[i].key.clone()),
            Some("rev".into())
        );
    }

    #[test]
    fn description_match_does_not_block_exact_key_auto_submit() {
        let rows = vec![
            PickerRow::new(
                "audit",
                &snip(
                    "Review",
                    "Review the branch",
                    "audit body",
                    SnippetOrigin::Global,
                ),
            ),
            PickerRow::new(
                "rev",
                &snip(
                    "Review",
                    "Review diff",
                    "review please",
                    SnippetOrigin::Global,
                ),
            ),
        ];
        // Both rows visible (audit's description matches "rev"), but only
        // `rev`'s key prefixes "rev", so it auto-submits.
        assert_eq!(compute_visible(&rows, "rev", &[]).indices.len(), 2);
        assert_eq!(
            auto_submit_index(&rows, "rev").map(|i| rows[i].key.clone()),
            Some("rev".into())
        );
    }

    #[test]
    fn case_tied_keys_auto_submit_on_the_exact_match() {
        let rows = vec![
            PickerRow::new("Rev", &snip("Review", "Big", "big", SnippetOrigin::Global)),
            PickerRow::new(
                "rev",
                &snip("Review", "Small", "small", SnippetOrigin::Global),
            ),
        ];
        assert_eq!(
            auto_submit_index(&rows, "rev").map(|i| rows[i].key.clone()),
            Some("rev".into())
        );
        assert_eq!(
            auto_submit_index(&rows, "Rev").map(|i| rows[i].key.clone()),
            Some("Rev".into())
        );
        // A genuinely ambiguous prefix never fires.
        assert_eq!(auto_submit_index(&rows, "re"), None);
    }

    #[test]
    fn builtin_rev_auto_submits() {
        let rows: Vec<PickerRow> = Snippets::builtin()
            .all()
            .map(|(k, v)| PickerRow::new(k, v))
            .collect();
        assert_eq!(
            auto_submit_index(&rows, "rev").map(|i| rows[i].key.clone()),
            Some("rev".into())
        );
    }

    #[test]
    fn recent_group_floats_to_top_on_empty_filter_only() {
        let rows = make_rows();
        let recent = resolve_recent(&rows, &["pr".to_string()]);
        let visible = compute_visible(&rows, "", &recent);
        assert_eq!(visible.recent_count, 1);
        assert_eq!(rows[visible.indices[0]].key, "pr");
        // `pr` appears twice: once in Recent, once in its category.
        assert_eq!(
            keys(&rows, &visible).iter().filter(|k| *k == "pr").count(),
            2
        );
        // A filter steps the group aside.
        let visible = compute_visible(&rows, "pr", &recent);
        assert_eq!(visible.recent_count, 0);
        assert_eq!(
            keys(&rows, &visible).iter().filter(|k| *k == "pr").count(),
            1
        );
    }

    #[test]
    fn resolve_recent_drops_stale_keys_and_keeps_mru_order() {
        let rows = make_rows();
        let recent = resolve_recent(&rows, &["ghost".into(), "rev".into(), "pr".into()]);
        assert_eq!(recent.len(), 2);
        assert_eq!(rows[recent[0]].key, "rev");
        assert_eq!(rows[recent[1]].key, "pr");
    }

    #[test]
    fn picker_view_emits_recent_and_category_groups() {
        let rows = make_rows();
        let view = compute_picker_view(&rows, "", &["pr".to_string()]);
        assert_eq!(view.total, 3);
        assert_eq!(
            view.visible_count, 4,
            "pr floats into Recent as a fourth entry"
        );
        assert_eq!(view.groups[0].category, RECENT_CAT);
        assert_eq!(view.groups[0].label, "Recent");
        assert_eq!(view.groups[0].rows[0].key, "pr");
        // Real category groups follow, in rank order.
        let cats: Vec<_> = view.groups[1..].iter().map(|g| g.label.clone()).collect();
        assert_eq!(cats, vec!["Review", "Git & PR", "Chores"]);
        assert_eq!(view.auto_submit, None);
    }

    #[test]
    fn picker_view_carries_auto_submit_target() {
        let rows = make_rows();
        let view = compute_picker_view(&rows, "rev", &[]);
        assert_eq!(view.auto_submit.as_deref(), Some("rev"));
        assert_eq!(view.visible_count, 1);
        assert!(view.groups.iter().all(|g| g.category != RECENT_CAT));
    }

    #[test]
    fn picker_row_carries_origin_label() {
        let row = PickerRow::new(
            "pr",
            &snip("Git & PR", "Open PR", "body", SnippetOrigin::Global),
        );
        assert_eq!(row.origin, "global");
    }
}
