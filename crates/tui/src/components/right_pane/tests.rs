#[cfg(test)]
mod should_arm_mark_timer_tests {
    use super::super::should_arm_mark_timer;
    use chrono::Utc;
    use pilot_core::{Workspace, WorkspaceKey};

    fn empty_ws() -> Workspace {
        Workspace::empty(WorkspaceKey::new("k"), "main", Utc::now())
    }

    fn ws_with_activity(unread: usize, read: usize) -> Workspace {
        let mut w = empty_ws();
        // Activity rows are indexed newest-first; `seen_count`
        // counts trailing reads. Build `unread` new + `read` old.
        for i in 0..(unread + read) {
            w.activity.push(pilot_core::Activity {
                author: format!("u{i}"),
                body: "x".into(),
                created_at: Utc::now(),
                kind: pilot_core::ActivityKind::Comment,
                node_id: None,
                path: None,
                line: None,
                diff_hunk: None,
                thread_id: None,
            });
        }
        w.seen_count = read;
        w
    }

    #[test]
    fn focus_no_longer_gates_arming() {
        // Pre-fix the predicate required `focused=true`. Result:
        // the auto-mark-read timer would never fire if the user
        // kept the sidebar pane focused while reading the activity
        // shown in the right pane. Now: as long as the cursor sits
        // on an unread row, the timer arms regardless of focus.
        let w = ws_with_activity(3, 0);
        assert!(should_arm_mark_timer(false, Some(&w), 0));
        assert!(should_arm_mark_timer(true, Some(&w), 0));
    }

    #[test]
    fn focused_no_workspace_does_not_arm() {
        assert!(!should_arm_mark_timer(true, None, 0));
    }

    #[test]
    fn focused_unread_cursor_arms() {
        let w = ws_with_activity(3, 0); // 3 unread at indices 0..3
        assert!(should_arm_mark_timer(true, Some(&w), 0));
        assert!(should_arm_mark_timer(true, Some(&w), 2));
    }

    #[test]
    fn focused_read_cursor_does_not_arm() {
        // 1 unread (idx 0) + 2 read (idx 1, 2).
        let w = ws_with_activity(1, 2);
        assert!(should_arm_mark_timer(true, Some(&w), 0), "unread row arms");
        assert!(
            !should_arm_mark_timer(true, Some(&w), 1),
            "already-read row must not arm",
        );
    }

    #[test]
    fn focused_empty_activity_does_not_arm() {
        let w = empty_ws();
        assert!(!should_arm_mark_timer(true, Some(&w), 0));
    }

    #[test]
    fn focused_out_of_bounds_cursor_does_not_arm() {
        // Defensive: a stale cursor past the activity len shouldn't
        // crash or spuriously arm.
        let w = ws_with_activity(2, 0);
        assert!(!should_arm_mark_timer(true, Some(&w), 100));
    }
}

#[cfg(test)]
mod teaser_noise_tests {
    use super::super::markdown::strip_inline_markdown_noise;

    #[test]
    fn strips_sub_tags() {
        assert_eq!(
            strip_inline_markdown_noise("hello <sub>world</sub>!"),
            "hello world!"
        );
    }

    #[test]
    fn collapses_image_to_alt() {
        // GitHub PR descriptions love shields.io badges. We don't
        // render images; keep the alt label so the teaser is at
        // least informative.
        assert_eq!(
            strip_inline_markdown_noise("![P1 Badge](https://img.shields.io/badge/P1-orange)"),
            "[P1 Badge]"
        );
    }

    #[test]
    fn collapses_link_to_text() {
        assert_eq!(
            strip_inline_markdown_noise("see [the docs](https://example.com)"),
            "see the docs"
        );
    }

    #[test]
    fn handles_multibyte_chars_without_panicking() {
        // Regression: pressing Down on a PR with `✓ APPROVED` in
        // its activity crashed pilot with "byte index 1 is not a
        // char boundary; it is inside '✓'". The old loop advanced
        // by 1 byte at a time then `&s[i..]`-sliced, landing inside
        // a multi-byte char.
        let input = "✓ APPROVED · 🚀 ship it";
        let out = strip_inline_markdown_noise(input);
        assert_eq!(out, input, "no markdown noise → pass-through unchanged");
    }

    #[test]
    fn handles_the_real_world_pr_badge_soup() {
        let input = "<sub><sub>![P1 Badge](https://img.shields.io/badge/P1-orange)</sub></sub>";
        let out = strip_inline_markdown_noise(input);
        assert_eq!(out, "[P1 Badge]");
    }
}

#[cfg(test)]
mod card_state_tests {
    use super::super::CardState;

    fn base() -> CardState {
        CardState {
            is_cursor: false,
            is_unread: false,
            is_expanded: false,
            is_selected: false,
            focused: false,
        }
    }

    #[test]
    fn dim_byline_only_when_read_and_not_focused_cursor() {
        // Read + not focused → dim (the byline retreats so unread
        // pops).
        assert!(base().dim_byline());
        // Unread → never dim regardless of cursor / focus.
        assert!(
            !CardState {
                is_unread: true,
                ..base()
            }
            .dim_byline()
        );
        // Focused cursor → never dim, even on a read row.
        assert!(
            !CardState {
                is_cursor: true,
                focused: true,
                ..base()
            }
            .dim_byline()
        );
        // Cursor without focus doesn't count — the user can't see
        // it, so the row should still dim.
        assert!(
            CardState {
                is_cursor: true,
                focused: false,
                ..base()
            }
            .dim_byline()
        );
    }
}

#[cfg(test)]
mod click_dispatch_tests {
    use super::super::{PaneId, RightPane};

    /// Smoke test: with no rendered hits cached, a click is a no-op.
    /// This is the safety net for "user clicks before first render"
    /// or "click while workspace is None."
    #[test]
    fn click_with_no_hits_is_noop() {
        let mut pane = RightPane::new(PaneId::new(0));
        assert!(!pane.handle_mouse_click(0, 0));
    }

    #[test]
    fn body_header_row_click_cycles_view() {
        use super::super::TaskBodyView;
        let mut pane = RightPane::new(PaneId::new(0));
        pane.click_hits.body_header_row = Some(5);
        // Fresh pane is Collapsed.
        assert_eq!(pane.task_body_view, TaskBodyView::Collapsed);
        // Click: Collapsed → Preview.
        assert!(pane.handle_mouse_click(0, 5));
        assert_eq!(pane.task_body_view, TaskBodyView::Preview);
        // Click: Preview → Full.
        assert!(pane.handle_mouse_click(0, 5));
        assert_eq!(pane.task_body_view, TaskBodyView::Full);
        // Click: Full → Collapsed (wraps).
        assert!(pane.handle_mouse_click(0, 5));
        assert_eq!(pane.task_body_view, TaskBodyView::Collapsed);
    }

    #[test]
    fn activity_header_row_click_toggles_section() {
        let mut pane = RightPane::new(PaneId::new(0));
        pane.click_hits.activity_header_row = Some(10);
        let before = pane.activity_collapsed;
        assert!(pane.handle_mouse_click(0, 10));
        assert_ne!(pane.activity_collapsed, before);
        // Marks the user override so the auto-collapse-on-empty
        // rule doesn't fight the user back the other way.
        assert!(pane.activity_collapse_user_set);
    }

    #[test]
    fn card_click_moves_cursor_and_selects_only_this_row() {
        // Click is now radio-select (matches the mental model: "click
        // moves the highlight"). Previously it toggled the row in/out
        // of a multi-select set, which made clicking a *different*
        // row deselect the first without making the second feel
        // selected. Multi-select stays available via `v` (keyboard).
        let mut pane = RightPane::new(PaneId::new(0));
        pane.click_hits.activity_cards.push((3, 12..=14));
        pane.click_hits.activity_cards.push((5, 16..=18));
        // First click on row 3 — selection is {3}.
        assert!(pane.handle_mouse_click(0, 13));
        assert_eq!(pane.feed.cursor, 3);
        assert!(pane.feed.is_selected(3));
        assert!(!pane.feed.is_expanded(3));
        // Click row 5 — selection becomes {5} only, NOT {3, 5}.
        assert!(pane.handle_mouse_click(0, 17));
        assert_eq!(pane.feed.cursor, 5);
        assert!(pane.feed.is_selected(5));
        assert!(
            !pane.feed.is_selected(3),
            "clicking row 5 should replace the selection, not add to it"
        );
    }

    #[test]
    fn card_double_click_toggles_expand() {
        let mut pane = RightPane::new(PaneId::new(0));
        pane.click_hits.activity_cards.push((3, 12..=14));
        assert!(pane.handle_mouse_double_click(0, 13));
        assert_eq!(pane.feed.cursor, 3);
        assert!(pane.feed.is_expanded(3));
        // Double-click again collapses.
        assert!(pane.handle_mouse_double_click(0, 13));
        assert!(!pane.feed.is_expanded(3));
    }
}

/// Regression tests for the `z` undo path. The auto-mark feature
/// stores a fingerprint of the activity being marked, not just the
/// raw index, so a poll that introduces a new top-of-feed comment
/// (shifting every older row down by one) doesn't make `z` un-read
/// the wrong row.
#[cfg(test)]
mod undo_auto_mark_tests {
    use super::super::{ActivityFingerprint, AutoMarkRecord, PaneId, RightPane};
    use chrono::{TimeZone, Utc};
    use pilot_core::{Activity, ActivityKind, Workspace, WorkspaceKey};

    fn activity_with(node_id: &str, body: &str) -> Activity {
        Activity {
            author: "alice".into(),
            body: body.into(),
            // Fixed timestamp keeps the fingerprint stable across
            // workspace mutations within the same test.
            created_at: Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap(),
            kind: ActivityKind::Comment,
            node_id: Some(node_id.into()),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        }
    }

    fn ws_with(activities: Vec<Activity>) -> Workspace {
        let mut w = Workspace::empty(WorkspaceKey::new("k"), "main", Utc::now());
        w.activity = activities;
        w
    }

    /// Fingerprint matches the original row even after a new top-
    /// of-feed item shifts it down by one.
    #[test]
    fn resolve_finds_row_after_shift() {
        let activities = vec![
            activity_with("n-A", "alpha"),
            activity_with("n-B", "beta"),
            activity_with("n-C", "gamma"),
        ];
        let record = AutoMarkRecord {
            last_index: 1,
            fingerprint: ActivityFingerprint::NodeId("n-B".into()),
        };
        // Pre-shift the resolver returns the cached index.
        assert_eq!(record.resolve(&activities), Some(1));

        // Insert a fresh item at the top — every old row shifts +1.
        let mut shifted = activities.clone();
        shifted.insert(0, activity_with("n-NEW", "fresh"));
        assert_eq!(record.resolve(&shifted), Some(2));
    }

    /// When the activity is gone (deleted upstream, never re-merged)
    /// the resolver returns None — undo refuses rather than guessing.
    #[test]
    fn resolve_returns_none_when_activity_missing() {
        let activities = vec![activity_with("n-A", "alpha")];
        let record = AutoMarkRecord {
            last_index: 1,
            fingerprint: ActivityFingerprint::NodeId("n-GONE".into()),
        };
        assert_eq!(record.resolve(&activities), None);
    }

    /// Activities without `node_id` (status changes / CI events) use
    /// the (author, created_at, body_prefix) fingerprint instead.
    #[test]
    fn content_fingerprint_survives_when_node_id_missing() {
        let mut act = activity_with("", "ci passed");
        act.node_id = None;
        let fp = ActivityFingerprint::of(&act);
        assert!(matches!(fp, ActivityFingerprint::Content { .. }));
        let activities = vec![act];
        let record = AutoMarkRecord {
            last_index: 0,
            fingerprint: fp,
        };
        assert_eq!(record.resolve(&activities), Some(0));
    }

    /// Pre-fix bug: marking row 1, polling adds a new row at 0 (so
    /// the marked row is now at 2), pressing `z` unmarks row 1 (the
    /// wrong activity). Post-fix: `z` resolves the fingerprint to
    /// the new index 2 and emits the correct UnmarkActivityRead.
    #[test]
    fn z_undo_follows_the_row_across_a_shift() {
        let mut pane = RightPane::new(PaneId::new(0));
        let activities = vec![
            activity_with("n-A", "alpha"),
            activity_with("n-B", "beta"),
            activity_with("n-C", "gamma"),
        ];
        pane.set_workspace(Some(ws_with(activities.clone())));
        // Manually arm + fire on row index 1 (the "beta" row).
        pane.feed.cursor = 1;
        let fired = pane.fire_auto_mark();
        assert_eq!(fired.map(|(_k, i)| i), Some(1));

        // Poll injects a new row at the top, shifting everything down.
        let mut shifted = activities.clone();
        shifted.insert(0, activity_with("n-NEW", "fresh"));
        pane.set_workspace(Some(ws_with(shifted)));

        let undone = pane.undo_auto_mark();
        // The "beta" row is now at index 2 — undo MUST resolve to 2,
        // not the stale 1.
        assert_eq!(
            undone.map(|(_k, i)| i),
            Some(2),
            "z undo should follow the fingerprint to the new index, not unmark the wrong row"
        );
    }
}
