#[cfg(test)]
mod should_arm_mark_timer_tests {
    use super::super::should_arm_mark_timer;
    use chrono::Utc;
    use lazybox_core::{Workspace, WorkspaceKey};

    fn empty_ws() -> Workspace {
        Workspace::empty(WorkspaceKey::new("k"), "main", Utc::now())
    }

    fn ws_with_activity(unread: usize, read: usize) -> Workspace {
        let mut w = empty_ws();
        // Activity rows are indexed newest-first; `seen_count`
        // counts trailing reads. Build `unread` new + `read` old.
        for i in 0..(unread + read) {
            w.activity.push(lazybox_core::Activity {
                author: format!("u{i}"),
                body: "x".into(),
                created_at: Utc::now(),
                kind: lazybox_core::ActivityKind::Comment,
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
        // its activity crashed lazybox with "byte index 1 is not a
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
    fn body_header_row_click_toggles_view() {
        use super::super::TaskBodyView;
        let mut pane = RightPane::new(PaneId::new(0));
        pane.click_hits.body_header_row = Some(5);
        // Fresh pane is Collapsed.
        assert_eq!(pane.task_body_view, TaskBodyView::Collapsed);
        // Click: Collapsed → Preview.
        assert!(pane.handle_mouse_click(0, 5));
        assert_eq!(pane.task_body_view, TaskBodyView::Preview);
        // Click again (nothing overflowing): Preview → Collapsed.
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
    use lazybox_core::{Activity, ActivityKind, Workspace, WorkspaceKey};

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
        // Move to row index 1 (the "beta" row) and re-arm the way
        // every production cursor move does, then fire.
        pane.feed.cursor = 1;
        pane.rearm_mark_timer_for_new_row(true);
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

/// Encapsulation regression tests for issue #42 — scrolling must never
/// rebuild the activity virtual-line buffer (the expensive markdown +
/// header layout), or a large feed makes the UI unresponsive while
/// scrolling. The buffer is memoized on a key that deliberately omits
/// `comment_scroll`; these tests pin that contract from both ends — the
/// key itself and the observable rebuild count through `render`.
#[cfg(test)]
mod scroll_does_not_rebuild_tests {
    use super::super::{PaneId, RightPane};
    use chrono::Utc;
    use lazybox_core::{Activity, ActivityKind, Workspace, WorkspaceKey};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn ws_with_n_activities(n: usize) -> Workspace {
        let mut w = Workspace::empty(WorkspaceKey::new("k"), "main", Utc::now());
        for i in 0..n {
            w.activity.push(Activity {
                author: format!("user{i}"),
                body: format!("comment body number {i}\nwith a second line"),
                created_at: Utc::now(),
                kind: ActivityKind::Comment,
                node_id: Some(format!("n-{i}")),
                path: None,
                line: None,
                diff_hunk: None,
                thread_id: None,
            });
        }
        w
    }

    fn draw(pane: &mut RightPane, term: &mut Terminal<TestBackend>) {
        term.draw(|f| pane.render(Rect::new(0, 0, 80, 24), f, true))
            .unwrap();
    }

    #[test]
    fn scrolling_reuses_the_cached_buffer() {
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws_with_n_activities(60)));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();

        // First paint builds the buffer once.
        draw(&mut pane, &mut term);
        assert_eq!(pane.activity_rebuilds(), 1);

        // A no-op repaint must hit the cache.
        draw(&mut pane, &mut term);
        assert_eq!(pane.activity_rebuilds(), 1, "idle repaint must not rebuild");

        // Scroll, repeatedly, and repaint each time — this is the
        // gesture that used to lock up the UI. Not one rebuild.
        for _ in 0..20 {
            assert!(pane.scroll_activity(2));
            draw(&mut pane, &mut term);
        }
        assert_eq!(
            pane.activity_rebuilds(),
            1,
            "scrolling rebuilt the activity buffer — issue #42 regression"
        );
        // The scroll actually moved (otherwise the test proves nothing).
        assert!(pane.comment_scroll() > 0);
    }

    #[test]
    fn content_and_layout_changes_do_rebuild() {
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws_with_n_activities(60)));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();

        draw(&mut pane, &mut term);
        assert_eq!(pane.activity_rebuilds(), 1);

        // Expanding a card changes what's drawn — must rebuild.
        pane.feed.toggle_expand(0);
        draw(&mut pane, &mut term);
        assert_eq!(pane.activity_rebuilds(), 2, "expansion must rebuild");
    }

    /// Lowest-level pin: the cache key is invariant to anything scroll
    /// touches and sensitive to anything the buffer's content depends
    /// on. `comment_scroll` isn't even an input — that's the guarantee.
    #[test]
    fn buffer_key_excludes_scroll_includes_content() {
        let ws = ws_with_n_activities(10);
        let mut feed = crate::components::activity_feed::ActivityFeed::new();
        let logins = std::collections::HashMap::new();

        let base = RightPane::activity_buffer_key(0, &ws, &feed, &logins, "Dark", 60, true);
        // Same inputs → same key (deterministic).
        assert_eq!(
            base,
            RightPane::activity_buffer_key(0, &ws, &feed, &logins, "Dark", 60, true)
        );
        // Expansion is part of the buffer → key changes.
        feed.toggle_expand(0);
        assert_ne!(
            base,
            RightPane::activity_buffer_key(0, &ws, &feed, &logins, "Dark", 60, true),
            "expanded set must be part of the key"
        );
        // Width + theme are part of the buffer → key changes.
        feed.toggle_expand(0);
        assert_ne!(
            base,
            RightPane::activity_buffer_key(0, &ws, &feed, &logins, "Dark", 40, true)
        );
        assert_ne!(
            base,
            RightPane::activity_buffer_key(0, &ws, &feed, &logins, "Light", 60, true)
        );
        // The content revision is part of the key — bumping it (what a
        // mutated activity set does) invalidates the cache without the
        // key ever hashing body bytes.
        assert_ne!(
            base,
            RightPane::activity_buffer_key(1, &ws, &feed, &logins, "Dark", 60, true),
            "activity_rev must be part of the key"
        );
    }

    /// The revision counter is the content-change detector: re-setting
    /// a byte-identical workspace clone (what every pane sync does)
    /// must NOT bump it, while a genuine activity mutation must — and
    /// must therefore rebuild the memoized buffer through `render`.
    #[test]
    fn activity_rev_tracks_content_not_clones() {
        let mut pane = RightPane::new(PaneId::new(0));
        let ws = ws_with_n_activities(5);
        pane.set_workspace(Some(ws.clone()));
        let rev = pane.activity_rev();

        // Identical clone (routine pane sync) → no bump.
        pane.set_workspace(Some(ws.clone()));
        assert_eq!(
            pane.activity_rev(),
            rev,
            "an identical workspace clone must not invalidate the buffer"
        );

        // A new comment arrives via WorkspaceUpserted → bump + rebuild.
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut pane, &mut term);
        let rebuilds = pane.activity_rebuilds();
        let mut grown = ws.clone();
        grown.activity.push(lazybox_core::Activity {
            author: "late-commenter".into(),
            body: "a fresh comment".into(),
            created_at: Utc::now(),
            kind: ActivityKind::Comment,
            node_id: Some("n-new".into()),
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        pane.on_event(&lazybox_ipc::Event::WorkspaceUpserted(Box::new(grown)));
        assert_ne!(
            pane.activity_rev(),
            rev,
            "a mutated activity set must bump the revision"
        );
        draw(&mut pane, &mut term);
        assert_eq!(
            pane.activity_rebuilds(),
            rebuilds + 1,
            "the mutated set must rebuild the memoized buffer"
        );
    }
}

#[cfg(test)]
mod auto_mark_fingerprint_tests {
    //! The auto-mark timer fires on the row it was ARMED on, not on
    //! the raw cursor index — new activities insert at the TOP of
    //! the feed and shift every index, so a dwell started on row 0
    //! must not mark whatever fresh comment shifted into slot 0.
    use super::super::{PaneId, RightPane};
    use chrono::{TimeZone, Utc};
    use lazybox_core::{Activity, ActivityKind, Workspace, WorkspaceKey};
    use lazybox_ipc::Command;

    fn activity_with(node_id: &str, body: &str) -> Activity {
        Activity {
            author: "alice".into(),
            body: body.into(),
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

    #[test]
    fn fire_follows_the_armed_row_after_a_top_insert() {
        let mut pane = RightPane::new(PaneId::new(0));
        let activities = vec![activity_with("n-A", "alpha"), activity_with("n-B", "beta")];
        // set_workspace arms the timer on cursor 0 → fingerprint(A).
        pane.set_workspace(Some(ws_with(activities.clone())));
        assert!(pane.mark_timer.is_armed(), "unread cursor row arms");

        // Poll inserts a fresh comment at the top; index 0 now points
        // at a row the user never dwelt on.
        let mut shifted = activities;
        shifted.insert(0, activity_with("n-NEW", "fresh"));
        pane.set_workspace(Some(ws_with(shifted)));

        let fired = pane.fire_auto_mark();
        assert_eq!(
            fired.map(|(_k, i)| i),
            Some(1),
            "the armed row must be marked at its SHIFTED index",
        );
        let ws = pane.workspace.as_ref().expect("workspace");
        assert!(
            ws.is_activity_unread(0),
            "the fresh top-of-feed comment must stay unread",
        );
        assert!(!ws.is_activity_unread(1), "the armed row flipped to read");
    }

    #[test]
    fn fire_skips_when_the_armed_row_vanished() {
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws_with(vec![activity_with("n-A", "alpha")])));
        assert!(pane.mark_timer.is_armed());

        // The armed row is gone entirely (deleted upstream).
        pane.set_workspace(Some(ws_with(vec![activity_with("n-X", "other")])));

        assert!(pane.fire_auto_mark().is_none(), "no row matches — skip");
        assert!(
            !pane.mark_timer.is_armed(),
            "a vanished target disarms instead of re-firing every tick",
        );
        let ws = pane.workspace.as_ref().expect("workspace");
        assert!(ws.is_activity_unread(0), "the stranger row stays unread");
    }

    #[test]
    fn mark_cursor_row_read_applies_the_local_echo() {
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws_with(vec![
            activity_with("n-A", "alpha"),
            activity_with("n-B", "beta"),
        ])));

        let mut cmds: Vec<Command> = Vec::new();
        assert!(pane.mark_cursor_row_read(&mut cmds));
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Command::MarkActivityRead { index, .. } => assert_eq!(*index, 0),
            other => panic!("expected MarkActivityRead, got {other:?}"),
        }
        let ws = pane.workspace.as_ref().expect("workspace");
        assert!(
            !ws.is_activity_unread(0),
            "`m` must flip the row locally on this frame, not wait for the daemon echo",
        );
        assert!(ws.is_activity_unread(1), "only the cursor row flips");
    }
}

#[cfg(test)]
mod teaser_tests {
    //! Teaser extraction from activity / PR bodies. A body that opens
    //! with a thematic break (`---`) must not yield a blank teaser,
    //! and prose that happens to start with a dash keeps it — only
    //! real list markers (`- `, `* `, `+ `) are stripped.
    use super::super::markdown::teaser_text;

    #[test]
    fn leading_thematic_break_is_skipped() {
        assert_eq!(
            teaser_text("---\n\nRelease notes for v2", 80),
            "Release notes for v2",
        );
    }

    #[test]
    fn list_marker_with_space_is_stripped() {
        assert_eq!(teaser_text("- first item\n- second", 80), "first item");
    }

    #[test]
    fn leading_dash_prose_keeps_its_dash() {
        assert_eq!(
            teaser_text("-2 degrees and falling", 80),
            "-2 degrees and falling",
        );
    }

    #[test]
    fn heading_markers_still_strip() {
        assert_eq!(teaser_text("### Title\nbody text", 80), "Title");
    }
}

#[cfg(test)]
mod has_visible_content_tests {
    //! `has_visible_content` drives whether the orchestrator hides the
    //! Activity pane: empty workspaces collapse the pane and hand the
    //! space to the terminal.
    use super::super::{PaneId, RightPane};
    use chrono::Utc;
    use lazybox_core::{Task, TaskId, Workspace, WorkspaceKey};

    fn empty_ws() -> Workspace {
        Workspace::empty(WorkspaceKey::new("github:o/r#1"), "main", Utc::now())
    }

    fn pane_with(ws: Option<Workspace>) -> RightPane {
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(ws);
        pane
    }

    fn issue_task_with_body(body: Option<&str>) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: "github:o/r#1".into(),
            },
            title: "an issue".into(),
            body: body.map(Into::into),
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/issues/1".into(),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: None,
            closes_issues: vec![],
        }
    }

    #[test]
    fn no_workspace_has_no_content() {
        assert!(!pane_with(None).has_visible_content());
    }

    #[test]
    fn empty_workspace_has_no_content() {
        assert!(!pane_with(Some(empty_ws())).has_visible_content());
    }

    #[test]
    fn workspace_with_activity_has_content() {
        let mut ws = empty_ws();
        ws.activity.push(lazybox_core::Activity {
            author: "alice".into(),
            body: "hi".into(),
            created_at: Utc::now(),
            kind: lazybox_core::ActivityKind::Comment,
            node_id: None,
            path: None,
            line: None,
            diff_hunk: None,
            thread_id: None,
        });
        assert!(pane_with(Some(ws)).has_visible_content());
    }

    #[test]
    fn workspace_with_description_has_content() {
        let ws = Workspace::from_task(issue_task_with_body(Some("Real body")), Utc::now());
        assert!(pane_with(Some(ws)).has_visible_content());
    }

    #[test]
    fn blank_description_is_not_content() {
        let ws = Workspace::from_task(issue_task_with_body(Some("   \n  ")), Utc::now());
        assert!(!pane_with(Some(ws)).has_visible_content());
    }
}

#[cfg(test)]
mod summary_render_tests {
    //! The `Summary`-mode one-line render (#487): a slim count of new
    //! activity + failing CI + how recently the task moved. Snapshots
    //! the full / summary distinction at the row level with a pinned
    //! clock so the relative time stays deterministic.
    use super::super::{PaneId, RightPane};
    use chrono::{Duration, Utc};
    use lazybox_core::{Activity, ActivityKind, CiStatus, Task, TaskId, Workspace};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn task(ci: CiStatus, updated: chrono::DateTime<chrono::Utc>) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: "github:o/r#1".into(),
            },
            title: "a pr".into(),
            body: None,
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: updated,
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: None,
            closes_issues: vec![],
        }
    }

    fn ws_with(n_unread: usize, ci: CiStatus, now: chrono::DateTime<chrono::Utc>) -> Workspace {
        let mut w = Workspace::from_task(task(ci, now - Duration::minutes(5)), now);
        for i in 0..n_unread {
            w.activity.push(Activity {
                author: format!("user{i}"),
                body: "ping".into(),
                created_at: now,
                kind: ActivityKind::Comment,
                node_id: Some(format!("n-{i}")),
                path: None,
                line: None,
                diff_hunk: None,
                thread_id: None,
            });
        }
        w
    }

    fn summary_row(pane: &RightPane, now: chrono::DateTime<chrono::Utc>) -> String {
        let (w, h) = (80u16, 1u16);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| pane.render_summary(Rect::new(0, 0, w, h), f, now))
            .unwrap();
        let buf = term.backend().buffer();
        (0..w).map(|x| buf[(x, 0)].symbol()).collect()
    }

    #[test]
    fn summary_shows_new_count_and_failing_ci() {
        let now = Utc::now();
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws_with(3, CiStatus::Failure, now)));
        let row = summary_row(&pane, now);
        assert!(row.contains("3 new"), "new count missing: {row:?}");
        assert!(row.contains("CI failing"), "CI signal missing: {row:?}");
        assert!(
            row.contains("updated 5m ago"),
            "time trailer missing: {row:?}"
        );
        assert!(row.starts_with('▸'), "expand glyph missing: {row:?}");
    }

    #[test]
    fn summary_omits_ci_when_green() {
        let now = Utc::now();
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws_with(1, CiStatus::Success, now)));
        let row = summary_row(&pane, now);
        assert!(row.contains("1 new"));
        assert!(
            !row.contains("CI failing"),
            "green CI must not show: {row:?}"
        );
    }

    #[test]
    fn summary_reads_no_new_when_all_read() {
        let now = Utc::now();
        let mut ws = ws_with(2, CiStatus::None, now);
        ws.mark_read_all();
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws));
        let row = summary_row(&pane, now);
        assert!(
            row.contains("no new activity"),
            "all-read summary wrong: {row:?}"
        );
    }
}

#[cfg(test)]
mod mark_workspace_merged_tests {
    //! Issue #265: `Event::PrMerged` flips the right-pane header pill to
    //! MERGED immediately for the shown workspace — the twin of
    //! `Sidebar::mark_workspace_merged` — so the detail pane and the
    //! sidebar row agree without waiting for the confirming poll.
    use super::super::{PaneId, RightPane};
    use chrono::Utc;
    use lazybox_core::{Task, TaskId, TaskState, Workspace, WorkspaceKey};

    fn open_pr_task() -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: "o/r#1".into(),
            },
            title: "a pr".into(),
            body: None,
            state: TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::Success,
            review: lazybox_core::ReviewStatus::Approved,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/pull/1".into(),
            repo: Some("o/r".into()),
            branch: Some("feature".into()),
            base_branch: Some("main".into()),
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: Some("PR_node".into()),
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: None,
            closes_issues: vec![],
        }
    }

    fn state_of(pane: &RightPane) -> TaskState {
        pane.selected_workspace()
            .and_then(|w| w.pr.as_ref())
            .expect("pane shows a PR workspace")
            .state
    }

    #[test]
    fn flips_the_shown_workspace_to_merged() {
        let ws = Workspace::from_task(open_pr_task(), Utc::now());
        let key = ws.key.clone();
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws));
        assert_eq!(state_of(&pane), TaskState::Open);

        pane.mark_workspace_merged(&key);
        assert_eq!(
            state_of(&pane),
            TaskState::Merged,
            "shown PR flips to MERGED"
        );
    }

    #[test]
    fn ignores_a_merge_for_a_different_workspace() {
        let ws = Workspace::from_task(open_pr_task(), Utc::now());
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws));

        pane.mark_workspace_merged(&WorkspaceKey::new("github:other/repo#9"));
        assert_eq!(
            state_of(&pane),
            TaskState::Open,
            "a merge for another workspace must not touch this pane",
        );
    }
}

#[cfg(test)]
mod description_expand_tests {
    //! Issue #344 / #448: the `+N more lines` trailer closing a capped
    //! Preview is a click target that opens the full body in the reader
    //! modal, and it spells out the affordance so it stops reading as a
    //! dead end.
    use super::super::{PaneId, RightPane, TaskBodyView, more_lines_trailer};
    use chrono::Utc;
    use lazybox_core::{Task, TaskId, Workspace};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn task_with_body(body: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: "github:o/r#1".into(),
            },
            title: "an issue".into(),
            body: Some(body.into()),
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: "https://github.com/o/r/issues/1".into(),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: None,
            closes_issues: vec![],
        }
    }

    fn pane_showing(body: &str) -> RightPane {
        let ws = Workspace::from_task(task_with_body(body), Utc::now());
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws));
        pane
    }

    fn draw(pane: &mut RightPane, term: &mut Terminal<TestBackend>) {
        term.draw(|f| pane.render(Rect::new(0, 0, 80, 24), f, true))
            .unwrap();
    }

    // A body far taller than any Preview / Full pane in an 80x24
    // terminal, so truncation always produces the trailer.
    fn long_body() -> String {
        (0..40).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn preview_trailer_click_opens_reader_modal() {
        let mut pane = pane_showing(&long_body());
        pane.toggle_task_body(); // Collapsed → Preview
        assert_eq!(pane.task_body_view, TaskBodyView::Preview);

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut pane, &mut term);

        let row = pane
            .click_hits
            .body_more_row
            .expect("a capped preview registers the trailer as a click target");
        assert!(pane.handle_mouse_click(0, row));
        assert!(
            pane.take_open_description(),
            "clicking the trailer requests the full-description modal",
        );
        assert_eq!(
            pane.task_body_view,
            TaskBodyView::Collapsed,
            "opening the reader folds the inline teaser away",
        );
    }

    #[test]
    fn second_d_on_overflowing_preview_opens_reader_modal() {
        // `d` on an overflowing Preview reads the whole thing in the
        // modal rather than collapsing.
        let mut pane = pane_showing(&long_body());
        pane.toggle_task_body(); // Collapsed → Preview
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut pane, &mut term);
        assert!(pane.click_hits.body_more_row.is_some());

        pane.toggle_task_body(); // Preview + overflow → open modal
        assert!(pane.take_open_description());
        assert_eq!(pane.task_body_view, TaskBodyView::Collapsed);
    }

    #[test]
    fn short_body_toggles_without_opening_modal() {
        let mut pane = pane_showing("one line only");
        pane.toggle_task_body(); // Collapsed → Preview (nothing to truncate)
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut pane, &mut term);
        assert!(pane.click_hits.body_more_row.is_none());
        // A second toggle collapses (no modal, since it all fits).
        pane.toggle_task_body();
        assert!(!pane.take_open_description());
        assert_eq!(pane.task_body_view, TaskBodyView::Collapsed);
    }

    #[test]
    fn short_rich_body_opens_reader_modal() {
        // A tiny table fits inline (no `+N more` trailer) but the teaser
        // flattens it — so the reader is still offered on a second `d`.
        let mut pane = pane_showing("| A | B |\n| - | - |");
        pane.toggle_task_body(); // Collapsed → Preview
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut pane, &mut term);
        assert!(
            pane.click_hits.body_more_row.is_none(),
            "the table isn't truncated — this is the rich, not overflow, path",
        );
        pane.toggle_task_body(); // d again → open modal (rich), not collapse
        assert!(pane.take_open_description());
        assert_eq!(pane.task_body_view, TaskBodyView::Collapsed);
    }

    #[test]
    fn table_shaped_line_in_indented_code_is_not_treated_as_a_table() {
        // A `| --- |`-shaped line indented into a code block (4 spaces)
        // is literal text the teaser handles fine — it must NOT trip the
        // rich-modal heuristic, so a short body like this just collapses
        // on a second `d` rather than opening the reader.
        let mut pane = pane_showing("run this:\n\n    | --- | :--: |\n\ndone");
        pane.toggle_task_body(); // Collapsed → Preview
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        draw(&mut pane, &mut term);
        assert!(
            pane.click_hits.body_more_row.is_none(),
            "the short body isn't truncated",
        );
        pane.toggle_task_body(); // d again → collapse (not a real table)
        assert!(
            !pane.take_open_description(),
            "an indented code line must not be mistaken for a table",
        );
        assert_eq!(pane.task_body_view, TaskBodyView::Collapsed);
    }

    fn description_header_text(pane: &mut RightPane, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| pane.render(Rect::new(0, 0, w, h), f, true))
            .unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .find(|row| row.contains("Description"))
            .unwrap_or_default()
    }

    #[test]
    fn preview_header_hint_matches_what_d_does() {
        // Plain short body: `d` collapses, so the hint must say collapse.
        let mut plain = pane_showing("just one short line");
        plain.toggle_task_body();
        let header = description_header_text(&mut plain, 80, 24);
        assert!(
            header.contains("collapse") && !header.contains("read full"),
            "plain preview hint: {header}",
        );

        // Overflowing body: `d` opens the reader, so the hint must say so.
        let mut long = pane_showing(&long_body());
        long.toggle_task_body();
        let header = description_header_text(&mut long, 80, 24);
        assert!(
            header.contains("read full"),
            "overflow preview hint: {header}"
        );
    }

    #[test]
    fn click_row_matches_the_trailer_even_when_the_header_would_wrap() {
        // A pane narrower than the `▼ Description  (d · collapse)`
        // header: a wrapping Paragraph would push the trailer down a row
        // and desync the recorded click target from where the trailer
        // actually paints. The recorded row must equal the trailer's
        // real screen row, and clicking it must still open the modal.
        let mut pane = pane_showing(&long_body());
        pane.toggle_task_body(); // Collapsed → Preview
        let w = 24u16;
        let mut term = Terminal::new(TestBackend::new(w, 24)).unwrap();
        term.draw(|f| pane.render(Rect::new(0, 0, w, 24), f, true))
            .unwrap();

        let recorded = pane
            .click_hits
            .body_more_row
            .expect("a capped preview registers the trailer as a click target");
        let buf = term.backend().buffer();
        let painted = (0..24u16)
            .find(|&y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("more lines")
            })
            .expect("the trailer paints somewhere on screen");
        assert_eq!(
            recorded, painted,
            "click target must match the trailer's real screen row",
        );
        assert!(pane.handle_mouse_click(0, recorded));
        assert!(pane.take_open_description());
    }

    #[test]
    fn switching_workspace_resets_the_description_teaser() {
        // An open Preview on PR A must not silently expand PR B's
        // description the moment B is selected — the teaser state is
        // per-workspace, like every other per-workspace UI bit reset in
        // `set_workspace`.
        let mut pane = pane_showing(&long_body());
        pane.toggle_task_body(); // Collapsed → Preview on A
        assert_eq!(pane.task_body_view, TaskBodyView::Preview);

        // A distinct second workspace (different task key).
        let mut task_b = task_with_body("some other body");
        task_b.id.key = "github:o/r#2".into();
        let ws_b = Workspace::from_task(task_b, Utc::now());
        pane.set_workspace(Some(ws_b));

        assert_eq!(
            pane.task_body_view,
            TaskBodyView::Collapsed,
            "a new workspace starts with its description collapsed",
        );
    }

    #[test]
    fn trailer_spells_out_the_read_full_affordance() {
        let theme = crate::theme::current();
        let trailer = more_lines_trailer(44, theme);
        let text: String = trailer.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("+44 more lines"));
        assert!(
            text.contains("read full"),
            "the trailer names the read-full affordance: {text}",
        );
    }
}

#[cfg(test)]
mod linked_issue_modal_tests {
    use super::super::{PaneId, RightPane, TaskBodyView};
    use chrono::Utc;
    use lazybox_core::{Task, TaskId, Workspace};

    fn task(kind: &str, number: u64, body: &str) -> Task {
        Task {
            id: TaskId {
                source: "github".into(),
                key: format!("github:o/r#{number}"),
            },
            title: format!("a {kind}"),
            body: Some(body.into()),
            state: lazybox_core::TaskState::Open,
            role: lazybox_core::TaskRole::Author,
            ci: lazybox_core::CiStatus::None,
            review: lazybox_core::ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/o/r/{kind}/{number}"),
            repo: Some("o/r".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: Utc::now(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            kind: None,
            closes_issues: vec![],
        }
    }

    /// A PR workspace that has folded in its linked issue — the state
    /// after `x j` (join into PR) / auto-collapse.
    fn pr_with_linked_issue(pr_body: &str, issue_body: &str) -> RightPane {
        let mut ws = Workspace::from_task(task("pull", 100, pr_body), Utc::now());
        ws.attach_task(task("issues", 42, issue_body));
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws));
        pane
    }

    #[test]
    fn modal_source_shows_both_bodies_and_clickable_links() {
        let pane = pr_with_linked_issue("the pr context", "the original brief");
        let src = pane.task_body().expect("a PR-with-issue has a modal body");
        assert!(src.contains("the pr context"), "PR body present:\n{src}");
        assert!(
            src.contains("Linked issue #42"),
            "issue section header:\n{src}"
        );
        assert!(
            src.contains("the original brief"),
            "issue body present:\n{src}"
        );
        // Clickable markdown links to BOTH tasks — the modal renders
        // these as `Msg::OpenUrl` click targets.
        assert!(
            src.contains("](https://github.com/o/r/pull/100)"),
            "clickable PR link:\n{src}",
        );
        assert!(
            src.contains("](https://github.com/o/r/issues/42)"),
            "clickable issue link:\n{src}",
        );
    }

    #[test]
    fn pr_without_linked_issue_keeps_the_plain_body() {
        let ws = Workspace::from_task(task("pull", 100, "just the pr body"), Utc::now());
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws));
        // Unchanged behavior: the raw body, no link scaffolding.
        assert_eq!(pane.task_body().as_deref(), Some("just the pr body"));
    }

    #[test]
    fn issue_only_workspace_keeps_the_plain_body() {
        let ws = Workspace::from_task(task("issues", 7, "issue body"), Utc::now());
        let mut pane = RightPane::new(PaneId::new(0));
        pane.set_workspace(Some(ws));
        assert_eq!(pane.task_body().as_deref(), Some("issue body"));
    }

    #[test]
    fn linked_issue_offers_the_modal_even_for_a_short_plain_pr_body() {
        // A short, non-rich PR body would normally just collapse the
        // teaser; with a linked issue it must open the reader modal so
        // the issue description is reachable (#462).
        let mut pane = pr_with_linked_issue("short", "the issue brief");
        pane.task_body_view = TaskBodyView::Preview;
        pane.toggle_task_body();
        assert!(
            pane.take_open_description(),
            "toggling a linked-issue PR opens the reader modal",
        );
    }
}
