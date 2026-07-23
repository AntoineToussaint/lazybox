//! Golden render snapshots via `insta`. One canonical Sidebar render
//! is locked here; other components will add their own snapshots as
//! they grow visual complexity (task #76).
//!
//! When the UI intentionally changes:
//!
//!   cargo install cargo-insta
//!   cargo insta review
//!
//! Accept with `a`, reject with `r`. Rejected changes fail CI —
//! that's the point.

use chrono::{Duration, TimeZone, Utc};
use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
use lazybox_ipc::Event;
use lazybox_tui::PaneId;
use lazybox_tui::components::Sidebar;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

fn fixed_time() -> chrono::DateTime<Utc> {
    // A stable "now" so snapshots don't drift with wall-clock time.
    Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
}

fn make_task(key: &str, minutes_old: i64) -> Task {
    Task {
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("task: {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{key}"),
        repo: Some("owner/repo".into()),
        branch: Some("main".into()),
        base_branch: None,
        updated_at: fixed_time() - Duration::minutes(minutes_old),
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

/// A sidebar whose clock is pinned to [`fixed_time`] from the start, so
/// every time-dependent path — the visible-set classification driven by
/// `on_event` *and* the relative timestamps produced by render — sees
/// the same fixed instant. Pinning here (before any event) rather than
/// just before render keeps the snapshots stable no matter when the test
/// runs; otherwise the ages drift with wall-clock time and the golden
/// files rot (e.g. `1mo` silently becomes `2mo` a month later).
fn sidebar() -> Sidebar {
    let mut s = Sidebar::new(PaneId::new(1));
    s.set_now_override(fixed_time());
    s
}

fn render_to_string(component: &mut Sidebar, w: u16, h: u16, focused: bool) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        component.render(Rect::new(0, 0, w, h), frame, focused);
    })
    .unwrap();
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            // Trim trailing whitespace — it's noise in the snapshot.
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn sidebar_golden_render_focused() {
    let mut s = sidebar();
    // Build three workspaces with known ages so sort order is
    // deterministic in the snapshot.
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(make_task("o/r#1", 10), fixed_time()),
            Workspace::from_task(make_task("o/r#2", 60), fixed_time()),
            Workspace::from_task(make_task("o/r#3", 120), fixed_time()),
        ],
        terminals: vec![],
        projects: vec![],
    });
    let rendered = render_to_string(&mut s, 40, 10, true);
    insta::assert_snapshot!("sidebar_focused_3_sessions", rendered);
}

/// Issue #65 golden: a list whose rows carry 1-, 2-, and 3-digit
/// numbers. The type glyph must sit flush against the number on EVERY
/// row (`○7`, `○42`, `○312`) — the regression was a right-aligned
/// number column that left-padded the shorter numbers, opening an
/// inconsistent gap after the glyph. Left-aligning pads the deficit on
/// the right instead, keeping the flush spacing from drifting back.
#[test]
fn sidebar_golden_render_mixed_number_widths() {
    let mut s = sidebar();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(make_task("o/r#7", 10), fixed_time()),
            Workspace::from_task(make_task("o/r#42", 60), fixed_time()),
            Workspace::from_task(make_task("o/r#312", 120), fixed_time()),
        ],
        terminals: vec![],
        projects: vec![],
    });
    let rendered = render_to_string(&mut s, 40, 10, true);
    insta::assert_snapshot!("sidebar_mixed_number_widths", rendered);
}

/// Regression guard for issue #37: the `[split]` sort mode must
/// render PRs and Issues as visually distinct sections per repo.
/// Mixes one PR (`/pull/` URL) and two issues (`/issues/` URL)
/// under the same repo and locks the resulting layout.
#[test]
fn sidebar_golden_render_split_pr_vs_issue() {
    let mut s = sidebar();
    let mut pr = make_task("o/r#10", 5);
    pr.url = "https://github.com/o/r/pull/10".into();
    let mut issue_a = make_task("o/r#11", 30);
    issue_a.url = "https://github.com/o/r/issues/11".into();
    let mut issue_b = make_task("o/r#12", 90);
    issue_b.url = "https://github.com/o/r/issues/12".into();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(pr, fixed_time()),
            Workspace::from_task(issue_a, fixed_time()),
            Workspace::from_task(issue_b, fixed_time()),
        ],
        terminals: vec![],
        projects: vec![],
    });
    // Sidebar starts in the default `ByRoleSplit` (chip label
    // `split`) sort mode; render directly without cycling so the
    // snapshot captures the default user experience.
    let rendered = render_to_string(&mut s, 40, 12, true);
    insta::assert_snapshot!("sidebar_split_pr_vs_issue", rendered);
}

/// Companion to the split-mode snapshot: same fixture but in
/// `Recent` mode, which suppresses kind headers. Pairs with the
/// split snapshot so a regression that wipes out the headers in
/// split mode would still produce visibly different output here.
#[test]
fn sidebar_golden_render_recent_pr_and_issue_mixed() {
    use lazybox_tui::components::sidebar::SortMode;
    let mut s = sidebar();
    while s.sort_mode() != SortMode::Recent {
        s.cycle_sort_mode();
    }
    let mut pr = make_task("o/r#10", 5);
    pr.url = "https://github.com/o/r/pull/10".into();
    let mut issue_a = make_task("o/r#11", 30);
    issue_a.url = "https://github.com/o/r/issues/11".into();
    let mut issue_b = make_task("o/r#12", 90);
    issue_b.url = "https://github.com/o/r/issues/12".into();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(pr, fixed_time()),
            Workspace::from_task(issue_a, fixed_time()),
            Workspace::from_task(issue_b, fixed_time()),
        ],
        terminals: vec![],
        projects: vec![],
    });
    let rendered = render_to_string(&mut s, 40, 12, true);
    insta::assert_snapshot!("sidebar_recent_pr_and_issue_mixed", rendered);
}

/// Regression for issue #231: at a small terminal size the row's
/// horizontal budget goes to content, not to empty gutters. The
/// selection caret is a single shared column (lpad + `▸`), so the
/// type glyph starts at column 2 — not pushed in by a 2-col caret —
/// and a long title fills the row right up to the lone scrollbar
/// gutter instead of leaving dead margin on either side.
#[test]
fn sidebar_tight_gutters_leave_room_for_content_at_small_width() {
    let mut s = sidebar();
    let mut t = make_task("o/r#1", 10);
    t.title = "A very long pull request title that keeps going".into();
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(t, fixed_time())],
        terminals: vec![],
        projects: vec![],
    });
    let w: u16 = 30;
    let rendered = render_to_string(&mut s, w, 10, true);
    let row = rendered
        .lines()
        .find(|l| l.trim_start().starts_with('▸'))
        .expect("a cursor workspace row");
    let chars: Vec<char> = row.chars().collect();
    // Left gutter: lpad(1) + 1-col caret, then the type glyph — no
    // 2-col-per-depth caret padding ahead of it.
    assert_eq!(chars[0], ' ', "row: {row:?}");
    assert_eq!(chars[1], '▸', "row: {row:?}");
    assert_ne!(
        chars[2], ' ',
        "type glyph should sit right after the single caret: {row:?}"
    );
    // Right side: only the scrollbar column is reserved, so a long
    // title fills the row to within a column of the edge rather than
    // truncating early and leaving a dead right gutter.
    let used = lazybox_tui::util::visual_width(row);
    assert!(
        used >= (w - 2) as usize,
        "long title left a dead right gutter (used {used} of {w}): {row:?}",
    );
}

#[test]
fn sidebar_golden_render_unfocused() {
    let mut s = sidebar();
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(make_task("o/r#1", 10), fixed_time())],
        terminals: vec![],
        projects: vec![],
    });
    let rendered = render_to_string(&mut s, 40, 6, false);
    insta::assert_snapshot!("sidebar_unfocused_1_session", rendered);
}

#[test]
fn sidebar_golden_render_empty() {
    let mut s = sidebar();
    let rendered = render_to_string(&mut s, 40, 5, true);
    insta::assert_snapshot!("sidebar_empty", rendered);
}

/// The build version sits next to the brand name so a running instance
/// is identifiable. Asserted against the live `CARGO_PKG_VERSION` so a
/// release bump can't silently drop the tag (the golden snapshots pin
/// the literal version and only verify placement).
#[test]
fn sidebar_header_shows_build_version() {
    let mut s = sidebar();
    let rendered = render_to_string(&mut s, 40, 5, true);
    let expected = concat!("v", env!("CARGO_PKG_VERSION"));
    assert!(
        rendered.contains(expected),
        "sidebar header {rendered:?} should contain {expected:?}"
    );
}

#[test]
fn sidebar_header_fits_narrow_width() {
    let mut s = sidebar();
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(make_task("o/r#1", 10), fixed_time())],
        terminals: vec![],
        projects: vec![],
    });

    let rendered = render_to_string(&mut s, 24, 5, true);
    let first_line = rendered.lines().next().unwrap_or_default();
    assert!(
        lazybox_tui::util::visual_width(first_line) <= 24,
        "header overflowed narrow sidebar: {first_line:?}"
    );
    assert!(
        first_line.contains("LAZYBOX"),
        "narrow header lost the brand: {first_line:?}"
    );
}

/// Issue #100: a genuinely empty inbox swaps the blank content area
/// for a getting-started panel that names the next actions, leading
/// with the worktree-session flow. Rendered tall enough to fit the
/// panel (the 5-row `sidebar_empty` case has no room and stays blank).
#[test]
fn sidebar_golden_render_empty_getting_started() {
    let mut s = sidebar();
    let rendered = render_to_string(&mut s, 40, 26, true);
    insta::assert_snapshot!("sidebar_empty_getting_started", rendered);
}

/// Which-key popup for the `g` leader (#126, #102). Locks the panel
/// layout — title row plus one `key  label` row per continuation,
/// anchored bottom-left above the footer. The rows are derived from
/// the catalog (every entry whose chord is a `g …` sequence), the same
/// way the model builds them at render time.
#[test]
fn which_key_github_group_golden_render() {
    use lazybox_tui::realm::components::which_key;
    use lazybox_tui_core::action::{ActionDef, Chord, ChordCode, KeyStroke};
    let g = KeyStroke::new(false, false, false, ChordCode::Char('g'));
    let rows: Vec<(String, String)> = ActionDef::all()
        .flat_map(|def| {
            def.default_chords()
                .into_iter()
                .filter_map(move |c| match c {
                    Chord::Seq(s) if s.len() == 2 && s[0] == g => {
                        Some((s[1].display(), def.label.to_string()))
                    }
                    _ => None,
                })
        })
        .collect();
    let backend = TestBackend::new(40, 13);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        which_key::render(
            frame,
            Rect::new(0, 0, 40, 13),
            g,
            Some("github"),
            &rows,
            None,
        );
    })
    .unwrap();
    let buf = term.backend().buffer();
    let rendered = (0..buf.area.height)
        .map(|y| {
            let mut row = String::new();
            for x in 0..buf.area.width {
                row.push_str(buf[(x, y)].symbol());
            }
            row.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("which_key_github_group", rendered);
}
