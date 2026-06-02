//! Tests for RightPane. The hierarchy step under test is **Workspace
//! → primary task** — the right pane projects the selected workspace
//! down to its headline task and its merged activity feed.
//!
//! Coverage:
//! - Selection (set_workspace stores, replaces, clears).
//! - Cursor preservation when re-setting the same workspace.
//! - Comment navigation (j/k/g/G, bounds).
//! - Event handling (WorkspaceUpserted refreshes the current row).
//! - Render: header (state + branch only) and activity list.

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{
    Activity, ActivityKind, CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace,
};
use pilot_ipc::Event;
use pilot_tui::PaneId;
use pilot_tui::components::RightPane;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

fn make_task(key: &str) -> Task {
    Task {
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("PR {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Reviewer,
        ci: CiStatus::Success,
        review: ReviewStatus::Pending,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{key}"),
        repo: Some("owner/repo".into()),
        branch: Some("feature/x".into()),
        base_branch: Some("main".into()),
        updated_at: Utc::now(),
        closed_at: None,
        labels: vec![],
        reviewers: vec!["alice".into(), "bob".into()],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: pilot_core::Mergeable::Mergeable,
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

fn activity(author: &str, body: &str, kind: ActivityKind) -> Activity {
    Activity {
        author: author.into(),
        body: body.into(),
        created_at: Utc::now(),
        kind,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    }
}

fn workspace_with_n_activities(key: &str, n: usize) -> Workspace {
    let mut w = Workspace::from_task(make_task(key), Utc::now());
    for i in 0..n {
        w.activity.push(activity(
            &format!("user{i}"),
            &format!("comment body {i}"),
            ActivityKind::Comment,
        ));
    }
    w.sort_activity();
    w
}

fn render_to_string(rp: &mut RightPane, width: u16, height: u16, focused: bool) -> String {
    let backend = TestBackend::new(width, height);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        rp.render(Rect::new(0, 0, width, height), frame, focused);
    })
    .unwrap();
    let buffer = term.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Selection ─────────────────────────────────────────────────────────

// ── `w` (work-on-this) dispatch regression tests ─────────────────
//
// The right pane's `w` used to auto-pick the cursor-row activity
// when nothing was selected, which surprised users on a CI-failing
// PR: pressing `w` would fire an "address these comments" prompt
// against a random activity row instead of the fix-CI prompt the
// sidebar `w` produces. These tests pin the new dispatch:
//   - selected comments → address them
//   - no selection → fall through to fix-CI / implement-issue / etc.

fn pr_with_ci_fail() -> Task {
    let mut t = make_task("o/r#42");
    t.ci = CiStatus::Failure;
    t.url = "https://github.com/o/r/pull/42".into();
    t
}

#[test]
fn w_with_ci_failing_and_no_selection_spawns_fix_ci_not_address_comments() {
    use pilot_ipc::Command;
    let mut rp = RightPane::new(PaneId::new(1));
    let mut ws = Workspace::from_task(pr_with_ci_fail(), Utc::now());
    ws.activity
        .push(activity("alice", "lgtm", ActivityKind::Comment));
    ws.activity
        .push(activity("bob", "looks good", ActivityKind::Comment));
    rp.set_workspace(Some(ws));
    let mut cmds: Vec<Command> = Vec::new();
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        &mut cmds,
    );
    let prompt = match cmds.first() {
        Some(Command::Spawn { initial_prompt, .. }) => initial_prompt
            .clone()
            .expect("Spawn must carry an initial_prompt"),
        other => panic!("expected a Spawn command, got {other:?}"),
    };
    assert!(
        prompt.contains("CI is failing"),
        "w on a CI-failing PR (no selection) must dispatch the fix-CI prompt, \
         got:\n{prompt}",
    );
    assert!(
        !prompt.contains("Address the following review comments"),
        "must NOT silently address random comments when nothing is selected, \
         got:\n{prompt}",
    );
}

#[test]
fn w_with_selected_comments_addresses_them() {
    // Sanity: when comments ARE selected, the address-comments
    // path still fires. This is the original `f` flow we preserved.
    use pilot_ipc::Command;
    let mut rp = RightPane::new(PaneId::new(1));
    let mut ws = Workspace::from_task(pr_with_ci_fail(), Utc::now());
    ws.activity
        .push(activity("alice", "nit on line 4", ActivityKind::Comment));
    ws.activity
        .push(activity("bob", "needs more tests", ActivityKind::Comment));
    rp.set_workspace(Some(ws));
    // Toggle a selection on the first comment via `v`.
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    let mut cmds: Vec<Command> = Vec::new();
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        &mut cmds,
    );
    let prompt = match cmds.first() {
        Some(Command::Spawn { initial_prompt, .. }) => initial_prompt
            .clone()
            .expect("Spawn must carry an initial_prompt"),
        other => panic!("expected a Spawn command, got {other:?}"),
    };
    assert!(
        prompt.contains("Address the following review comments"),
        "selected comments → address-comments prompt; got:\n{prompt}",
    );
}

#[test]
fn w_on_author_healthy_pr_no_unread_is_noop() {
    // Sanity: a PR I AUTHORED with green CI, no conflicts, no
    // unread activity has nothing actionable to do. Pressing `w`
    // here is a no-op (don't pre-emptively spawn an agent just
    // because the user pressed a key).
    //
    // Reviewer + healthy now fires ReviewCode (the user explicitly
    // wants `w` to mean "review this" when you're the reviewer);
    // this test covers the author side.
    use pilot_ipc::Command;
    let mut rp = RightPane::new(PaneId::new(1));
    let mut t = make_task("o/r#1");
    t.role = pilot_core::TaskRole::Author;
    t.ci = CiStatus::Success;
    t.url = "https://github.com/o/r/pull/1".into();
    let mut ws = Workspace::from_task(t, Utc::now());
    ws.activity
        .push(activity("alice", "lgtm", ActivityKind::Comment));
    ws.mark_read_all(); // no unread activity
    rp.set_workspace(Some(ws));
    let mut cmds: Vec<Command> = Vec::new();
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        &mut cmds,
    );
    assert!(
        cmds.is_empty(),
        "w on a healthy PR I authored with no unread activity must NOT spawn anything, \
         got {cmds:?}",
    );
}

#[test]
fn w_on_reviewer_healthy_pr_fires_review_prompt() {
    // The new "w on a reviewer's PR" contract: pressing `w` while
    // landed on a PR you're assigned to review should pre-load
    // claude with a review prompt — no need to manually type
    // "review this PR" each time.
    use pilot_ipc::Command;
    let mut rp = RightPane::new(PaneId::new(1));
    let mut t = make_task("o/r#1");
    t.role = pilot_core::TaskRole::Reviewer;
    t.ci = CiStatus::Success;
    t.url = "https://github.com/o/r/pull/1".into();
    let mut ws = Workspace::from_task(t, Utc::now());
    // The right-pane `w` handler skips when activity is collapsed
    // (auto-collapses on empty workspaces). Seed one row so the
    // section auto-opens AND so the workspace looks plausible.
    ws.activity
        .push(activity("alice", "lgtm", ActivityKind::Comment));
    rp.set_workspace(Some(ws));
    rp.set_activity_collapsed(false);
    let mut cmds: Vec<Command> = Vec::new();
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE),
        &mut cmds,
    );
    let prompt = match cmds.first() {
        Some(Command::Spawn { initial_prompt, .. }) => initial_prompt
            .clone()
            .expect("Spawn must carry an initial_prompt"),
        other => panic!("expected a Spawn command, got {other:?}"),
    };
    assert!(
        prompt.contains("Review") && prompt.contains("reviewer"),
        "reviewer w → review prompt; got:\n{prompt}",
    );
}

#[test]
fn set_workspace_stores_it() {
    let mut rp = RightPane::new(PaneId::new(1));
    assert!(rp.selected_workspace().is_none());
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    assert!(rp.selected_workspace().is_some());
}

#[test]
fn set_workspace_to_different_resets_cursor() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 5)));
    for _ in 0..3 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 3);

    rp.set_workspace(Some(workspace_with_n_activities("o/r#2", 5)));
    assert_eq!(
        rp.comment_cursor(),
        0,
        "cursor resets when workspace changes"
    );
}

#[test]
fn set_workspace_to_same_preserves_cursor() {
    let mut rp = RightPane::new(PaneId::new(1));
    let ws = workspace_with_n_activities("o/r#1", 5);
    rp.set_workspace(Some(ws.clone()));
    for _ in 0..2 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 2);

    rp.set_workspace(Some(ws));
    assert_eq!(rp.comment_cursor(), 2);
}

#[test]
fn set_workspace_none_clears() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.set_workspace(None);
    assert!(rp.selected_workspace().is_none());
}

// ── Comment navigation ────────────────────────────────────────────────

#[test]
fn j_moves_cursor_down_bounded() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    for _ in 0..10 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 2);
}

#[test]
fn k_moves_cursor_up_bounded() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.handle_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    for _ in 0..10 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 0);
}

#[test]
fn g_jumps_to_top() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 10)));
    for _ in 0..5 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 0);
}

#[test]
fn shift_g_jumps_to_bottom() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 7)));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 6);
}

#[test]
fn j_without_workspace_bubbles_up() {
    let mut rp = RightPane::new(PaneId::new(1));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_tui::PaneOutcome::Pass);
}

#[test]
fn unknown_key_bubbles_up_even_with_workspace() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_tui::PaneOutcome::Pass);
}

#[test]
fn j_on_empty_activity_list_passes_through_when_collapsed() {
    // Empty activity → auto-collapsed → j has nothing to scroll
    // through and bubbles up so the parent can handle it.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    assert!(rp.activity_collapsed(), "empty workspace auto-collapses");
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_tui::PaneOutcome::Pass);
    assert_eq!(rp.comment_cursor(), 0);
}

#[test]
fn j_on_expanded_empty_activity_consumes() {
    // User toggled the empty section open. j now lands on the
    // pane (no row to move to but it's still consumed).
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    rp.set_activity_collapsed(false);
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_tui::PaneOutcome::Consumed);
    assert_eq!(rp.comment_cursor(), 0);
}

// ── Events ────────────────────────────────────────────────────────────

#[test]
fn workspace_upserted_for_current_updates_state() {
    let mut rp = RightPane::new(PaneId::new(1));
    let mut ws = workspace_with_n_activities("o/r#1", 3);
    rp.set_workspace(Some(ws.clone()));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 2);

    ws.activity
        .push(activity("bob", "new one", ActivityKind::Comment));
    ws.sort_activity();
    rp.on_event(&Event::WorkspaceUpserted(Box::new(ws)));
    // Activity grew 3 → 4 (newest-first, so the new item lands at
    // index 0). `ActivityFeed::adjust_for_length_change` shifts
    // every existing index up by the delta — the user was reading
    // index 2; that comment is now at index 3. Stays focused on
    // the same comment instead of jumping or collapsing.
    assert_eq!(rp.comment_cursor(), 3);
    assert_eq!(rp.selected_workspace().unwrap().activity.len(), 4);
}

#[test]
fn workspace_upserted_shrinks_clamps_cursor() {
    let mut rp = RightPane::new(PaneId::new(1));
    let ws = workspace_with_n_activities("o/r#1", 5);
    rp.set_workspace(Some(ws));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT),
        &mut Vec::new(),
    );
    assert_eq!(rp.comment_cursor(), 4);

    let smaller = workspace_with_n_activities("o/r#1", 2);
    rp.on_event(&Event::WorkspaceUpserted(Box::new(smaller)));
    assert_eq!(rp.comment_cursor(), 1);
}

#[test]
fn workspace_upserted_for_different_workspace_is_ignored() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.on_event(&Event::WorkspaceUpserted(Box::new(
        workspace_with_n_activities("o/r#99", 10),
    )));
    assert_eq!(
        rp.selected_workspace()
            .and_then(|w| w.primary_task())
            .map(|t| t.id.key.as_str()),
        Some("o/r#1"),
        "unrelated upsert ignored"
    );
}

#[test]
fn unrelated_events_do_not_perturb_state() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.on_event(&Event::Notification {
        title: "hi".into(),
        body: "there".into(),
    });
    rp.on_event(&Event::ProviderError {
        source: "x".into(),
        message: "y".into(),
        detail: String::new(),
        kind: String::new(),
    });
    assert_eq!(rp.comment_cursor(), 0);
    assert!(rp.selected_workspace().is_some());
}

// ── handle_key never emits commands ───────────────────────────────────

#[test]
fn key_handling_never_emits_commands() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    let mut cmds = Vec::new();
    for c in ['j', 'k', 'g', 'G', 'x'] {
        rp.handle_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &mut cmds,
        );
    }
    assert!(cmds.is_empty());
}

// ── Rendering ─────────────────────────────────────────────────────────

#[test]
fn render_empty_shows_placeholder() {
    let mut rp = RightPane::new(PaneId::new(1));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("no session selected"));
}

#[test]
fn render_shows_header_fields() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 2)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("OPEN"), "state tag");
    assert!(rendered.contains("PR o/r#1"), "title");
    assert!(rendered.contains("feature/x"), "branch");
    // Reviewers row added back (see `b7dbb8a`). The fixture's
    // `make_pr` sets `reviewers = ["alice", "bob"]`, so both
    // logins appear with the `@` prefix in the header.
    assert!(rendered.contains("Reviewers:"), "reviewers label visible");
    assert!(rendered.contains("@alice"), "alice listed");
    assert!(rendered.contains("@bob"), "bob listed");
}

#[test]
fn render_shows_activity_count_in_title() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 7)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    // Modern title format: `Activity  7` (label + count separated by
    // spaces, no parens).
    assert!(rendered.contains("Activity"));
    assert!(rendered.contains('7'));
}

#[test]
fn render_lists_comments() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("user0"));
    assert!(rendered.contains("comment body 0"));
}

#[test]
fn render_shows_scrollbar_when_activity_overflows() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 40)));
    // Short pane: 40 comments can't fit, so the indicator shows.
    let rendered = render_to_string(&mut rp, 60, 10, true);
    assert!(
        rendered.contains('█'),
        "overflowing activity shows a scrollbar thumb; got:\n{rendered}"
    );
}

#[test]
fn render_hides_scrollbar_when_activity_fits() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 2)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(
        !rendered.contains('█'),
        "scrollbar auto-hides when activity fits; got:\n{rendered}"
    );
}

#[test]
fn render_empty_activity_collapses_to_header() {
    // Empty activity defaults to collapsed: just the header row, no
    // "(no activity)" placeholder taking up space below it.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("Activity"), "header still rendered");
    assert!(
        !rendered.contains("no activity"),
        "body placeholder hidden when collapsed"
    );
    // Collapsed glyph in header.
    assert!(rendered.contains('▸'), "collapsed glyph shown");
}

#[test]
fn render_expanded_empty_activity_shows_placeholder() {
    // User toggled it open — now the empty placeholder appears in the
    // body area below the header.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    rp.set_activity_collapsed(false);
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("no activity"));
    assert!(rendered.contains('▾'), "expanded glyph shown");
}

// ── Collapse / expand ─────────────────────────────────────────────────

#[test]
fn empty_workspace_auto_collapses() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    assert!(rp.activity_collapsed());
}

#[test]
fn workspace_with_activity_auto_expands() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    assert!(!rp.activity_collapsed());
}

#[test]
fn enter_toggles_collapse() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    assert!(!rp.activity_collapsed());
    let outcome = rp.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert_eq!(outcome, pilot_tui::PaneOutcome::Consumed);
    assert!(rp.activity_collapsed(), "Enter collapses");
    rp.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert!(!rp.activity_collapsed(), "Enter again expands");
}

#[test]
fn space_toggles_row_selection_not_collapse() {
    // Space is the discoverable multi-select key (the list-UI
    // convention) — it toggles the focused row's selection and must
    // NOT collapse the section (that's Enter's job).
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.handle_key(
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert!(
        !rp.activity_collapsed(),
        "Space must not collapse the section",
    );
    assert_eq!(
        rp.selected_activity_indices(),
        vec![0],
        "Space selects the focused row",
    );
    // Pressing Space again deselects it.
    rp.handle_key(
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert!(
        rp.selected_activity_indices().is_empty(),
        "Space again deselects the focused row",
    );
}

#[test]
fn switching_workspaces_re_applies_auto_collapse() {
    // The user toggled empty→open on workspace A. Switching to a
    // different empty workspace shouldn't carry that override over —
    // each workspace gets its own auto-default.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 0)));
    rp.set_activity_collapsed(false); // user expands
    assert!(!rp.activity_collapsed());

    rp.set_workspace(Some(workspace_with_n_activities("o/r#2", 0)));
    assert!(
        rp.activity_collapsed(),
        "fresh workspace re-applies the empty=collapsed default"
    );
}

#[test]
fn re_setting_same_workspace_does_not_reset_user_override() {
    // Re-setting the SAME workspace (poll refresh) must preserve the
    // user's collapse choice — otherwise every poll would un-collapse
    // a section the user just closed.
    let mut rp = RightPane::new(PaneId::new(1));
    let ws = workspace_with_n_activities("o/r#1", 3);
    rp.set_workspace(Some(ws.clone()));
    assert!(!rp.activity_collapsed());
    rp.set_activity_collapsed(true); // user collapses
    rp.set_workspace(Some(ws)); // poll re-delivers same workspace
    assert!(
        rp.activity_collapsed(),
        "user collapse survives same-workspace re-set"
    );
}

#[test]
fn render_unread_badge_when_unread_present() {
    // Workspace with 5 activities, none marked seen → all unread →
    // header shows "● 5 new".
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 5)));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(rendered.contains("5 new"), "badge shows unread count");
    assert!(rendered.contains('●'), "badge glyph present");
}

#[test]
fn render_no_badge_when_all_read() {
    let mut rp = RightPane::new(PaneId::new(1));
    let mut ws = workspace_with_n_activities("o/r#1", 5);
    ws.mark_read_all();
    rp.set_workspace(Some(ws));
    let rendered = render_to_string(&mut rp, 60, 20, true);
    assert!(
        !rendered.contains("new"),
        "no 'new' badge when everything is read"
    );
}

// ── Auto-mark-read on hover ───────────────────────────────────────────
//
// Cursor on an unread row + pane focused → 1-second timer arms.
// On expiry the activity is flipped to read, an `m` (MarkRead) command
// is queued for persistence, and `z` undoes the most recent flip.

fn workspace_with_unread_at(key: &str, count: usize) -> Workspace {
    workspace_with_n_activities(key, count)
    // mark_read_all hasn't been called → all rows are unread by
    // default (read_indices empty, seen_count 0).
}

#[test]
fn workspace_with_unread_arms_mark_timer() {
    // Updated contract: set_workspace arms the timer when the
    // cursor lands on an unread row, regardless of pane focus.
    // The old contract gated arming on focus, but in practice the
    // user navigates the sidebar (focus stays there) while reading
    // the right pane, and gating-on-focus meant the timer never
    // fired for them. Now arming follows the row, not the focus.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_unread_at("o/r#1", 3)));
    assert!(
        rp.auto_mark_progress().is_some(),
        "unread row under cursor arms the timer eagerly",
    );
}

#[test]
fn focus_loss_disarms_timer() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_unread_at("o/r#1", 3)));
    rp.notify_focus_changed(true);
    assert!(rp.auto_mark_progress().is_some());
    rp.notify_focus_changed(false);
    assert!(
        rp.auto_mark_progress().is_none(),
        "leaving the pane disarms"
    );
}

#[test]
fn cursor_move_resets_timer_to_zero() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_unread_at("o/r#1", 3)));
    rp.notify_focus_changed(true);
    let before = rp.auto_mark_progress().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    rp.handle_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    let after = rp.auto_mark_progress().unwrap();
    assert!(
        after < before + 0.05,
        "cursor move re-arms; new ratio shouldn't include the 50ms wait"
    );
}

#[test]
fn tick_after_delay_marks_activity_read() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_unread_at("o/r#1", 3)));
    rp.notify_focus_changed(true);
    let before_unread = rp.selected_workspace().unwrap().unread_count();
    assert_eq!(before_unread, 3);

    // Wait past the delay then tick.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let fired = rp.tick(true);
    assert!(fired.is_some(), "tick fires after the 1s window");

    let after_unread = rp.selected_workspace().unwrap().unread_count();
    assert_eq!(after_unread, 2, "exactly one row was flipped");
    assert!(rp.can_undo_mark_read());
}

#[test]
fn tick_before_delay_does_nothing() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_unread_at("o/r#1", 3)));
    rp.notify_focus_changed(true);
    // No sleep — tick fires immediately, well before 1s.
    assert!(rp.tick(true).is_none());
    assert_eq!(rp.selected_workspace().unwrap().unread_count(), 3);
}

#[test]
fn z_undoes_most_recent_auto_mark() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_unread_at("o/r#1", 3)));
    rp.notify_focus_changed(true);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    rp.tick(true);
    assert_eq!(rp.selected_workspace().unwrap().unread_count(), 2);

    // Undo via `z`. Re-flips the row to unread locally + persists
    // via `Command::UnmarkActivityRead` so the daemon's stored read
    // state matches.
    let mut cmds = Vec::new();
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        &mut cmds,
    );
    assert_eq!(rp.selected_workspace().unwrap().unread_count(), 3);
    assert!(!rp.can_undo_mark_read());
    assert!(
        cmds.iter()
            .any(|c| matches!(c, pilot_ipc::Command::UnmarkActivityRead { .. })),
        "z emits UnmarkActivityRead so the daemon writes the partial undo"
    );
}

#[test]
fn z_with_no_pending_undo_is_a_noop() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    rp.notify_focus_changed(true);
    let mut cmds = Vec::new();
    rp.handle_key(
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
        &mut cmds,
    );
    assert!(cmds.is_empty(), "z with no pending mark is a clean no-op");
}

#[test]
fn cursor_move_clears_undo_target() {
    // Don't allow undoing a mark from two rows ago — that's a
    // surprising footgun. Once you navigate past the marked row the
    // undo affordance disappears.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_unread_at("o/r#1", 3)));
    rp.notify_focus_changed(true);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    rp.tick(true);
    assert!(rp.can_undo_mark_read());

    rp.handle_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert!(
        !rp.can_undo_mark_read(),
        "moving the cursor invalidates undo"
    );
}

#[test]
fn cursor_walks_long_thread_with_scroll_following() {
    // Long PR thread (40 comments). Render at a small height that
    // fits ~8 cards. The first frame establishes `last_visible_cards`
    // by rendering at the small height. Then j past the visible
    // window — comment_scroll must advance so the cursor stays
    // on-screen.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 40)));
    // Force the cards loop by rendering once at a small body height.
    // Activity body takes about ~12 lines per card collapsed +
    // header + breathing; at height 20 we get ~1-2 cards visible.
    let _ = render_to_string(&mut rp, 80, 20, true);
    let visible_after_first_render = rp.comment_scroll();
    assert_eq!(visible_after_first_render, 0, "scroll starts at top");
    // Walk down 15 rows. The cursor should still be on-screen,
    // meaning scroll has advanced.
    for _ in 0..15 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 15, "cursor advanced 15 times");
    assert!(
        rp.comment_scroll() > 0,
        "scroll must have advanced past 0 to keep the cursor visible; got {}",
        rp.comment_scroll()
    );
    assert!(
        rp.comment_cursor() >= rp.comment_scroll(),
        "cursor never falls behind the top of the viewport"
    );
}

#[test]
fn mouse_wheel_scrolls_activity_when_buffer_exceeds_viewport() {
    // Regression: `scroll_activity` was computing `max_scroll` in
    // activity-count units while `comment_scroll` is a line offset.
    // Any height shorter than `activity.len()` lines is enough to
    // demonstrate scroll is actually needed; with 40 activities and
    // a height of 12 (~9 content lines) the viewport must scroll
    // down or the wheel does nothing.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 40)));
    let _ = render_to_string(&mut rp, 80, 12, true);
    assert_eq!(rp.comment_scroll(), 0, "starts at top");
    let moved = rp.scroll_activity(8);
    assert!(moved, "wheel-down should report movement");
    assert!(
        rp.comment_scroll() > 0,
        "wheel-down advances comment_scroll; got {}",
        rp.comment_scroll()
    );
    let mid = rp.comment_scroll();
    let moved_up = rp.scroll_activity(-8);
    assert!(moved_up, "wheel-up should report movement");
    assert!(
        rp.comment_scroll() < mid,
        "wheel-up reduces comment_scroll; got {} (was {})",
        rp.comment_scroll(),
        mid
    );
}

#[test]
fn mouse_wheel_clamps_to_total_lines() {
    // Wheel can't scroll past the last line of the virtual buffer.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 40)));
    let _ = render_to_string(&mut rp, 80, 12, true);
    // Slam the wheel down a lot — should saturate, not panic.
    for _ in 0..200 {
        rp.scroll_activity(8);
    }
    let saturated = rp.comment_scroll();
    let no_op = rp.scroll_activity(8);
    assert!(!no_op, "saturated wheel returns false (no redraw needed)");
    assert_eq!(
        rp.comment_scroll(),
        saturated,
        "scroll cap holds at saturation"
    );
}

#[test]
fn mouse_wheel_does_nothing_when_everything_fits() {
    // Small list rendered at huge height — every card fits, wheel
    // should report no movement (and not panic on underflow).
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 3)));
    let _ = render_to_string(&mut rp, 80, 80, true);
    assert!(!rp.scroll_activity(8), "nothing to scroll → false");
    assert_eq!(rp.comment_scroll(), 0);
}

#[test]
fn page_down_jumps_by_visible_window() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 40)));
    let _ = render_to_string(&mut rp, 80, 20, true);
    let start = rp.comment_cursor();
    rp.handle_key(
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert!(
        rp.comment_cursor() > start,
        "PageDown moves the cursor down"
    );
}

#[test]
fn page_up_walks_back() {
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 40)));
    let _ = render_to_string(&mut rp, 80, 20, true);
    for _ in 0..15 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    let mid = rp.comment_cursor();
    rp.handle_key(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
        &mut Vec::new(),
    );
    assert!(rp.comment_cursor() < mid, "PageUp walks back");
}

#[test]
fn clamp_scroll_to_cursor_does_not_underflow_on_tiny_viewport() {
    // Regression: `(cursor + 1) - last_visible_cards` underflowed
    // to a giant usize when `last_visible_cards > cursor + 1` (the
    // window had never rendered or rendered into a tiny rect, so
    // `last_visible_cards` was still the conservative default but
    // the cursor was at 0). The scroll viewport then jumped to
    // ~18 quintillion, painting nothing.
    let mut rp = RightPane::new(PaneId::new(1));
    rp.set_workspace(Some(workspace_with_n_activities("o/r#1", 5)));
    // Render at a height too small to fit even one card cleanly.
    // The renderer sets `last_visible_cards` to the line budget
    // (which here is 0-1 after the header + divider). With cursor
    // at 0 and last_visible_cards ≥ 2, the old code underflowed.
    let _ = render_to_string(&mut rp, 80, 6, true);
    // Walk to cursor=0 explicitly + clamp. No panic + scroll stays
    // sane.
    for _ in 0..10 {
        rp.handle_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut Vec::new(),
        );
    }
    assert_eq!(rp.comment_cursor(), 0);
    assert_eq!(rp.comment_scroll(), 0, "scroll clamps to 0, not usize::MAX");
}
