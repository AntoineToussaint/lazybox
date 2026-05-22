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
use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};
use pilot_ipc::Event;
use pilot_tui::PaneId;
use pilot_tui::components::Sidebar;
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
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        has_conflicts: false,
        is_behind_base: false,
        node_id: None,
        needs_reply: false,
        last_commenter: None,
        recent_activity: vec![],
        additions: 0,
        deletions: 0,
        closes_issues: vec![],
    }
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
    let mut s = Sidebar::new(PaneId::new(1));
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

#[test]
fn sidebar_golden_render_unfocused() {
    let mut s = Sidebar::new(PaneId::new(1));
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
    let mut s = Sidebar::new(PaneId::new(1));
    let rendered = render_to_string(&mut s, 40, 5, true);
    insta::assert_snapshot!("sidebar_empty", rendered);
}
