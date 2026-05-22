//! Sidebar behavior tests. Pinned model: **Repo → Workspace → Session
//! → Terminal**. The sidebar consumes WORKSPACE events from the
//! daemon and renders rows grouped by repo. Each test names the layer
//! it's exercising so a regression on one rung of the hierarchy is
//! easy to spot.
//!
//! Coverage:
//!
//! - Event handling (Snapshot / WorkspaceUpserted / WorkspaceRemoved).
//! - Repo grouping: header rows above their workspace rows; the
//!   cursor never lands on a header.
//! - Visibility filtering (Inbox vs Snoozed, merged/closed hidden).
//! - Sort order (updated_at desc within each repo group).
//! - Cursor preservation across re-sort / upsert / remove.
//! - All keybindings — each emits the expected Command.
//! - Kill two-press confirmation.
//! - Render output via ratatui's TestBackend.

use chrono::{DateTime, Duration, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pilot_core::{
    CiStatus, ReviewStatus, SessionKey, Task, TaskId, TaskRole, TaskState, Workspace, WorkspaceKey,
};
use pilot_ipc::{Command, Event, TerminalKind};
use pilot_tui::PaneId;
use pilot_tui::components::{Mailbox, Sidebar, sidebar::VisibleRow};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

// ── Fixtures ───────────────────────────────────────────────────────────

fn make_task(repo: &str, key: &str, updated: DateTime<Utc>) -> Task {
    // The URL must contain `/pull/` for `Workspace::classify` to put
    // this task in the workspace's PR slot — issue paths land in
    // `gh_issues` instead and the assertions on `workspace.pr` fail.
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
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
        url: format!("https://github.com/{path}/pull/{num}"),
        repo: Some(repo.into()),
        branch: Some("main".into()),
        base_branch: None,
        updated_at: updated,
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

fn make_workspace(repo: &str, key: &str, updated: DateTime<Utc>) -> Workspace {
    Workspace::from_task(make_task(repo, key, updated), updated)
}

/// Resolve the wire-side selection key for `task_key`. This is the
/// sanitized form `pilot_core::workspace_key_for` produces — tests
/// assert against this so they stay accurate when the sanitizer
/// changes.
fn expected_session_key(task_key: &str) -> String {
    pilot_core::workspace_key_for(&make_task("", task_key, Utc::now()))
}

fn key_code(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn shift_char(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

fn ws_key(workspace: &Workspace) -> SessionKey {
    SessionKey::new(workspace.key.as_str())
}

// ── Event handling ─────────────────────────────────────────────────────

#[test]
fn snapshot_populates_workspaces() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w1 = make_workspace("owner/repo", "o/r#1", now);
    let w2 = make_workspace("owner/repo", "o/r#2", now - Duration::hours(1));
    s.on_event(&Event::Snapshot {
        workspaces: vec![w1.clone(), w2],
        terminals: vec![],
    });
    assert_eq!(s.workspace_count(), 2);
    assert_eq!(s.selected_session_key(), Some(&ws_key(&w1)));
}

#[test]
fn workspace_upserted_inserts_then_updates_in_place() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w = make_workspace("owner/repo", "o/r#1", now);
    s.on_event(&Event::WorkspaceUpserted(Box::new(w)));
    assert_eq!(s.workspace_count(), 1);

    // Same key, newer timestamp, renamed: same row, name updated.
    let mut updated = make_workspace("owner/repo", "o/r#1", now + Duration::minutes(5));
    updated.name = "renamed".into();
    s.on_event(&Event::WorkspaceUpserted(Box::new(updated.clone())));
    assert_eq!(s.workspace_count(), 1);
    assert_eq!(
        s.selected_workspace().map(|w| w.name.as_str()),
        Some("renamed")
    );
}

#[test]
fn workspace_removed_prunes_and_clamps_cursor() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w1 = make_workspace("owner/repo", "o/r#1", now);
    let w2 = make_workspace("owner/repo", "o/r#2", now - Duration::hours(1));
    s.on_event(&Event::Snapshot {
        workspaces: vec![w1, w2.clone()],
        terminals: vec![],
    });
    // Move cursor to second workspace row.
    s.handle_key(key_code(KeyCode::Down), &mut Vec::new());
    assert_eq!(s.selected_session_key(), Some(&ws_key(&w2)));

    s.on_event(&Event::WorkspaceRemoved(w2.key.clone()));
    assert_eq!(s.workspace_count(), 1);
    // Cursor falls back to the only remaining workspace.
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#1"))
    );
}

#[test]
fn cursor_follows_workspace_key_across_resort() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w1 = make_workspace("owner/repo", "o/r#1", now);
    let w2 = make_workspace("owner/repo", "o/r#2", now - Duration::hours(1));
    let w3 = make_workspace("owner/repo", "o/r#3", now - Duration::hours(2));
    s.on_event(&Event::Snapshot {
        workspaces: vec![w1, w2.clone(), w3.clone()],
        terminals: vec![],
    });
    // Cursor on #2.
    s.handle_key(key_code(KeyCode::Down), &mut Vec::new());
    assert_eq!(s.selected_session_key(), Some(&ws_key(&w2)));

    // #3 jumps to top with a new updated_at — cursor stays on #2.
    let mut bumped = w3.clone();
    if let Some(t) = bumped.pr.as_mut() {
        t.updated_at = now + Duration::hours(1);
    }
    s.on_event(&Event::WorkspaceUpserted(Box::new(bumped)));
    assert_eq!(
        s.selected_session_key(),
        Some(&ws_key(&w2)),
        "cursor follows the workspace key across re-sort"
    );
}

#[test]
fn merged_workspace_hidden() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    // updated_at well outside the 30-minute grace window — the
    // "freshly merged stays in Inbox briefly" path doesn't apply
    // here, so the merged workspace must be filtered out as
    // expected.
    let merged_at = now - Duration::hours(2);
    let mut merged = make_workspace("owner/repo", "o/r#1", merged_at);
    if let Some(t) = merged.pr.as_mut() {
        t.state = TaskState::Merged;
    }
    let live = make_workspace("owner/repo", "o/r#2", now);
    s.on_event(&Event::Snapshot {
        workspaces: vec![merged, live.clone()],
        terminals: vec![],
    });
    assert_eq!(s.workspace_count(), 1);
    assert_eq!(s.selected_session_key(), Some(&ws_key(&live)));
}

// ── Repo grouping (the hierarchy) ──────────────────────────────────────

#[test]
fn rows_are_grouped_by_repo_with_headers() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            make_workspace("owner/alpha", "alpha#1", now),
            make_workspace("owner/beta", "beta#1", now),
            make_workspace("owner/alpha", "alpha#2", now - Duration::hours(1)),
        ],
        terminals: vec![],
    });
    let rows = s.visible_rows();
    // Hierarchy: alpha header → its 2 workspaces → beta header → its 1.
    let header_indexes: Vec<_> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| matches!(r, VisibleRow::RepoHeader(_)).then_some(i))
        .collect();
    assert_eq!(header_indexes, vec![0, 3], "headers at expected positions");
    match &rows[0] {
        VisibleRow::RepoHeader(name) => assert_eq!(name, "owner/alpha"),
        _ => panic!("expected alpha header first"),
    }
    match &rows[3] {
        VisibleRow::RepoHeader(name) => assert_eq!(name, "owner/beta"),
        _ => panic!("expected beta header second"),
    }
}

#[test]
fn cursor_walks_through_repo_headers() {
    // j/k now stop on repo headers too — needed so users can land
    // on a collapsed header and Space-to-expand. Header rows have
    // no session key (selected_session_key is None on them).
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            make_workspace("owner/alpha", "alpha#1", now),
            make_workspace("owner/beta", "beta#1", now),
        ],
        terminals: vec![],
    });
    // Layout: [alpha header, alpha#1, beta header, beta#1]. Cursor
    // starts on alpha#1. j → beta header → beta#1.
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("alpha#1"))
    );
    s.handle_key(key_code(KeyCode::Down), &mut Vec::new());
    assert!(s.selected_session_key().is_none(), "cursor on beta header");
    s.handle_key(key_code(KeyCode::Down), &mut Vec::new());
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("beta#1"))
    );
}

// ── Mailbox ────────────────────────────────────────────────────────────

#[test]
fn snoozed_workspace_hidden_from_inbox() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut snoozed = make_workspace("owner/repo", "o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        workspaces: vec![snoozed, make_workspace("owner/repo", "o/r#2", now)],
        terminals: vec![],
    });
    assert_eq!(s.workspace_count(), 1);
    assert_eq!(s.mailbox(), Mailbox::Inbox);
}

#[test]
fn toggle_mailbox_cycles_inbox_inactive_snoozed() {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut snoozed = make_workspace("owner/repo", "o/r#1", now);
    snoozed.snoozed_until = Some(now + Duration::hours(4));
    s.on_event(&Event::Snapshot {
        workspaces: vec![snoozed, make_workspace("owner/repo", "o/r#2", now)],
        terminals: vec![],
    });
    // Cycle: Inbox → Inactive → Snoozed → Inbox.
    assert_eq!(s.mailbox(), Mailbox::Inbox);
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Inactive);
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Snoozed);
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#1"))
    );
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Inbox);
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#2"))
    );
}

#[test]
fn inactive_mailbox_shows_merged_and_closed_workspaces() {
    // The whole point of Inactive: surface workspaces whose primary
    // task is merged or closed. Without this view those rows just
    // disappeared from the inbox after a merge.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    // Past the 30-min grace window — these are "permanently
    // inactivated" workspaces. The freshly-merged grace path is
    // covered separately.
    let stale = now - Duration::hours(2);
    let mut merged = make_workspace("owner/repo", "merged#1", stale);
    if let Some(t) = merged.pr.as_mut() {
        t.state = TaskState::Merged;
    }
    let mut closed = make_workspace("owner/repo", "closed#1", stale);
    if let Some(t) = closed.pr.as_mut() {
        t.state = TaskState::Closed;
    }
    let live = make_workspace("owner/repo", "live#1", now);
    s.on_event(&Event::Snapshot {
        workspaces: vec![merged, closed, live],
        terminals: vec![],
    });
    // Inbox has only the live workspace.
    assert_eq!(s.workspace_count(), 1);

    // Inactive surfaces both the merged and the closed.
    s.handle_key(shift_char('S'), &mut Vec::new());
    assert_eq!(s.mailbox(), Mailbox::Inactive);
    assert_eq!(s.workspace_count(), 2);
}

// ── Keybindings → commands ─────────────────────────────────────────────

fn populated_sidebar() -> Sidebar {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        workspaces: vec![
            make_workspace("owner/repo", "o/r#1", now),
            make_workspace("owner/repo", "o/r#2", now - Duration::hours(1)),
        ],
        terminals: vec![],
    });
    s
}

#[test]
fn c_emits_spawn_claude_for_selected() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('c')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Spawn {
            session_key,
            kind: TerminalKind::Agent(agent),
            ..
        } => {
            assert_eq!(session_key.to_string(), expected_session_key("o/r#1"));
            assert_eq!(agent, "claude");
        }
        other => panic!("expected Spawn Agent(claude), got {other:?}"),
    }
}

#[test]
fn x_emits_spawn_codex_for_selected() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('x')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Spawn {
            session_key,
            kind: TerminalKind::Agent(agent),
            ..
        } => {
            assert_eq!(session_key.to_string(), expected_session_key("o/r#1"));
            assert_eq!(agent, "codex", "x maps to Codex by default");
        }
        other => panic!("expected Spawn Agent(codex), got {other:?}"),
    }
}

#[test]
fn custom_agent_shortcuts_override_defaults() {
    let mut s = Sidebar::new(PaneId::new(1))
        .with_agent_shortcuts([('c', "claude".into()), ('a', "aider".into())]);
    let now = Utc::now();
    s.on_event(&Event::WorkspaceUpserted(Box::new(make_workspace(
        "owner/repo",
        "o/r#1",
        now,
    ))));

    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('a')), &mut cmds);
    match cmds.as_slice() {
        [
            Command::Spawn {
                kind: TerminalKind::Agent(agent),
                ..
            },
        ] => assert_eq!(agent, "aider"),
        _ => panic!("expected Spawn Agent(aider), got {cmds:?}"),
    }

    // `x` is no longer mapped in the custom set — bubbles up.
    let mut cmds = Vec::new();
    let outcome = s.handle_key(key_code(KeyCode::Char('x')), &mut cmds);
    assert_eq!(
        outcome,
        pilot_tui::PaneOutcome::Pass,
        "unmapped key bubbles, doesn't spawn a random default"
    );
    assert!(cmds.is_empty());
}

#[test]
fn c_on_empty_sidebar_emits_nothing() {
    let mut s = Sidebar::new(PaneId::new(1));
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('c')), &mut cmds);
    assert!(cmds.is_empty());
}

#[test]
fn s_emits_spawn_shell() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('s')), &mut cmds);
    assert!(matches!(
        cmds.as_slice(),
        [Command::Spawn {
            kind: TerminalKind::Shell,
            ..
        }]
    ));
}

#[test]
fn m_emits_mark_read() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Char('m')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::MarkRead { session_key } => {
            assert_eq!(session_key.to_string(), expected_session_key("o/r#1"));
        }
        other => panic!("expected MarkRead, got {other:?}"),
    }
}

// ── Snooze semantics ───────────────────────────────────────────────────
// (lowercase `z` snooze/unsnooze migrated to `Action::ToggleSnooze`
// in the catalog — exercised via `Model::dispatch_action` in
// tests/model_orchestrator.rs. `Shift-Z` long-snooze is still inline
// here pending its own catalog migration.)

#[test]
fn shift_z_archives_a_year_out() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    let before = Utc::now();
    // Two presses now — first arms, second fires. The 1-year
    // snooze is irreversible enough to deserve a confirmation,
    // same as Shift-X / Shift-M.
    s.handle_key(shift_char('Z'), &mut cmds);
    assert!(cmds.is_empty(), "first Shift-Z arms, doesn't fire");
    s.handle_key(shift_char('Z'), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Snooze { until, .. } => {
            let min = before + Duration::days(364);
            assert!(*until >= min, "archive snooze should be roughly a year");
        }
        other => panic!("expected Snooze, got {other:?}"),
    }
}

// ── Navigation bounds ─────────────────────────────────────────────────
//
// (Shift-X / Shift-M no longer have sidebar-level tests — both are
// catalog actions now, dispatched by `Model::dispatch_action` and
// gated by the `ActionConfirm` modal. The sidebar's inline handlers
// for those keys were deleted; the equivalent behavior is exercised
// at the model layer in `tests/model_orchestrator.rs`.)

#[test]
fn j_stops_at_last_workspace() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    for _ in 0..10 {
        s.handle_key(key_code(KeyCode::Down), &mut cmds);
    }
    assert_eq!(
        s.selected_session_key().map(|k| k.to_string()),
        Some(expected_session_key("o/r#2"))
    );
}

#[test]
fn k_stops_at_top_row() {
    // After repeatedly pressing k from any row, the cursor lands
    // on the top of the visible list. With the collapse-aware nav
    // that's the repo header — assert via `cursor_on_repo_header`
    // because `selected_session_key` is None on a header.
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    s.handle_key(key_code(KeyCode::Down), &mut cmds);
    for _ in 0..10 {
        s.handle_key(key_code(KeyCode::Up), &mut cmds);
    }
    assert!(
        s.cursor_on_repo_header(),
        "k repeatedly should leave the cursor on the top repo header, not a workspace"
    );
}

// ── Bubble-up ──────────────────────────────────────────────────────────

#[test]
fn unknown_key_bubbles_up() {
    let mut s = populated_sidebar();
    let mut cmds = Vec::new();
    let outcome = s.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut cmds);
    assert_eq!(outcome, pilot_tui::PaneOutcome::Pass);
    assert!(cmds.is_empty());
}

// ── Render ─────────────────────────────────────────────────────────────

fn render_to_string(s: &mut Sidebar, width: u16, height: u16, focused: bool) -> String {
    let backend = TestBackend::new(width, height);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|frame| {
        s.render(Rect::new(0, 0, width, height), frame, focused);
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

#[test]
fn render_smoke_has_mailbox_label_and_grouped_rows() {
    let mut s = populated_sidebar();
    // Width 60 leaves breathing room for the `[PR]`/`[I]` type
    // marker, role char, and the status/time trailers without
    // truncating the title — this test is about presence, not
    // density.
    let rendered = render_to_string(&mut s, 60, 12, true);
    // V1-style brand label: `PILOT` for the Inbox mailbox.
    assert!(rendered.contains("PILOT"));
    assert!(rendered.contains('2'), "row count in title");
    assert!(rendered.contains("owner/repo"), "repo header rendered");
    assert!(rendered.contains("task: o/r#1"), "first workspace visible");
    assert!(
        rendered.contains("[PR]"),
        "PR rows carry an explicit [PR] type marker",
    );
}

#[test]
fn render_shows_cursor_marker_on_selected_workspace() {
    let mut s = populated_sidebar();
    let rendered = render_to_string(&mut s, 60, 10, true);
    let cursor_line = rendered
        .lines()
        .find(|l| l.contains('▸'))
        .unwrap_or_else(|| panic!("expected cursor marker; got:\n{rendered}"));
    assert!(cursor_line.contains("o/r#1"));
}

#[test]
fn render_mailbox_toggles_title() {
    let mut s = populated_sidebar();
    // PILOT → INACTIVE → SNOOZED; uppercase brand label per V1.
    s.handle_key(shift_char('S'), &mut Vec::new());
    let rendered = render_to_string(&mut s, 40, 12, true);
    assert!(rendered.contains("INACTIVE"));
    s.handle_key(shift_char('S'), &mut Vec::new());
    let rendered = render_to_string(&mut s, 40, 12, true);
    assert!(rendered.contains("SNOOZED"));
}

// ── Hierarchy invariant: WorkspaceKey ↔ SessionKey conversions ────────

#[test]
fn workspace_key_round_trips_through_session_key() {
    // The wire-side selection key is `SessionKey`, but the values
    // flowing through it are workspace keys. Round-trip both ways
    // because every Sidebar lookup hits this conversion.
    let wk = WorkspaceKey::new("owner/repo:42");
    let sk: SessionKey = (&wk).into();
    assert_eq!(sk.as_str(), wk.as_str());
}

// ── Workspace ↔ Session expansion (the user-facing rule) ─────────────

use pilot_core::{SessionKind, WorkspaceSession};
use std::path::PathBuf;

fn add_session(workspace: &mut Workspace, name: &str) -> pilot_core::SessionId {
    let mut s = WorkspaceSession::new(
        workspace.key.clone(),
        SessionKind::Shell,
        PathBuf::from(format!("/tmp/{name}")),
        Utc::now(),
    );
    s.name = name.into();
    workspace.add_session(s)
}

#[test]
fn workspace_with_one_session_does_not_show_a_subrow() {
    // 99% of workspaces have a single session — duplicating it as
    // its own row is visual noise. The runner badge on the workspace
    // row already conveys "this workspace has a live session".
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "claude");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    let session_rows = s
        .visible_rows()
        .iter()
        .filter(|r| matches!(r, VisibleRow::Session { .. }))
        .count();
    assert_eq!(session_rows, 0, "one session → no separate sub-row");
}

#[test]
fn workspace_with_two_sessions_expands_into_subrows() {
    // Crossing the threshold from 1 → 2 sessions makes the workspace
    // visually expand: the workspace row stays, plus one Session
    // sub-row per session.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "claude");
    add_session(&mut w, "shell");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    let session_rows: Vec<_> = s
        .visible_rows()
        .iter()
        .filter(|r| matches!(r, VisibleRow::Session { .. }))
        .collect();
    assert_eq!(session_rows.len(), 2, "two Session sub-rows for 2 sessions");
}

#[test]
fn cursor_can_land_on_a_session_subrow() {
    // With 2+ sessions, j moves the cursor through the session
    // sub-rows. selected_session_id surfaces which one.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    let s0 = add_session(&mut w, "claude");
    let s1 = add_session(&mut w, "shell");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    // Cursor starts on the workspace row. Down once → session 0.
    s.handle_key(key_code(KeyCode::Down), &mut Vec::new());
    assert_eq!(s.selected_session_id(), Some(s0));
    s.handle_key(key_code(KeyCode::Down), &mut Vec::new());
    assert_eq!(s.selected_session_id(), Some(s1));
    // Workspace row's selected_session_id is None — the daemon
    // resolves which session to use.
    s.handle_key(key_code(KeyCode::Up), &mut Vec::new());
    s.handle_key(key_code(KeyCode::Up), &mut Vec::new());
    assert_eq!(s.selected_session_id(), None);
}

#[test]
fn session_created_event_expands_into_subrows_at_two() {
    // The user has a workspace with 1 session, hits `c` to spawn
    // Claude into a second session. The daemon emits SessionCreated;
    // the sidebar crosses the 1→2 threshold and now shows one Session
    // sub-row per session so the user can pick between them.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "shell");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w.clone()],
        terminals: vec![],
    });
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        0,
        "single-session workspaces collapse — runner badge handles them"
    );

    let new_session = WorkspaceSession::new(
        w.key.clone(),
        SessionKind::Agent {
            agent_id: "claude".into(),
        },
        PathBuf::from("/tmp/claude"),
        Utc::now(),
    );
    s.on_event(&Event::SessionCreated(Box::new(new_session)));
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        2,
        "expanded to two sub-rows once the workspace had two sessions"
    );
}

#[test]
fn session_ended_event_collapses_back_below_two() {
    // 2 → 1 sessions: the workspace drops back to a single workspace
    // row with no Session sub-rows. The remaining session is implicit.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut w = make_workspace("owner/repo", "o/r#1", Utc::now());
    add_session(&mut w, "shell");
    let claude_id = add_session(&mut w, "claude");
    s.on_event(&Event::Snapshot {
        workspaces: vec![w.clone()],
        terminals: vec![],
    });
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        2
    );

    s.on_event(&Event::SessionEnded {
        workspace_key: w.key.clone(),
        session_id: claude_id,
    });
    assert_eq!(
        s.visible_rows()
            .iter()
            .filter(|r| matches!(r, VisibleRow::Session { .. }))
            .count(),
        0,
        "single survivor → workspace row alone, no sub-rows"
    );
}

#[test]
fn subscribed_repo_with_no_workspace_still_renders_a_header() {
    // The "I added a repo but the sidebar is empty" UX bug: until
    // polling finds open PRs/issues, no workspace exists for the
    // new repo, so the old render code emitted no row at all.
    // After apply_subscribed_scopes, an empty header should appear.
    let mut s = Sidebar::new(PaneId::new(1));

    // Empty snapshot — no workspaces at all.
    s.on_event(&Event::Snapshot {
        workspaces: vec![],
        terminals: vec![],
    });
    let scopes: std::collections::BTreeSet<String> = ["github:fresh-org/new-repo".to_string()]
        .into_iter()
        .collect();
    s.apply_subscribed_scopes(&scopes);

    // The visible list should contain a RepoHeader for the new
    // repo even though there's no workspace under it.
    let names: Vec<&str> = s
        .visible_rows()
        .iter()
        .filter_map(|r| match r {
            pilot_tui::components::sidebar::VisibleRow::RepoHeader(name) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        names.contains(&"fresh-org/new-repo"),
        "expected an empty header for the subscribed repo, got: {names:?}"
    );
}

#[test]
fn subscribed_org_level_scope_does_not_render_a_header() {
    // Subscribing to a whole org (no slash in the scope id after
    // the provider prefix) means "all repos under this org" — we
    // can't render a repo header for it because we don't know the
    // repo names. Headers materialize as polling discovers them.
    let mut s = Sidebar::new(PaneId::new(1));
    s.on_event(&Event::Snapshot {
        workspaces: vec![],
        terminals: vec![],
    });
    let scopes: std::collections::BTreeSet<String> =
        ["github:some-org".to_string()].into_iter().collect();
    s.apply_subscribed_scopes(&scopes);

    let headers = s
        .visible_rows()
        .iter()
        .filter(|r| matches!(r, pilot_tui::components::sidebar::VisibleRow::RepoHeader(_)))
        .count();
    assert_eq!(headers, 0, "org-level scope should NOT produce a header");
}

// ── `f` / `w` agent-spawn targeting ──────────────────────────────────

fn issue_task(repo: &str, key: &str, body: Option<&str>) -> Task {
    let mut t = make_task(repo, key, Utc::now());
    let num = key.rsplit_once('#').map(|(_, n)| n).unwrap_or("1");
    t.url = format!("https://github.com/{repo}/issues/{num}");
    t.body = body.map(str::to_string);
    t
}

fn pr_task_with_ci(repo: &str, key: &str, ci: CiStatus) -> Task {
    let mut t = make_task(repo, key, Utc::now());
    t.ci = ci;
    t
}

// ── State × action key matrix ─────────────────────────────────────────
//
// Pins the behavior of every action-y sidebar key against the row
// state under the cursor. Same shape as the `w` bug we shipped a fix
// for: the handler dispatches differently depending on what's under
// the cursor, and we want EVERY (state, key) cell verified so a
// silent regression fails loudly.

/// Build a sidebar with one repo and one workspace whose primary
/// task is shaped by `mutate`. Cursor lands on the workspace row
/// (recompute_visible falls the initial cursor past the repo
/// header).
fn sidebar_with_pr<F: FnOnce(&mut Task)>(mutate: F) -> Sidebar {
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let mut task = make_task("owner/repo", "o/r#1", now);
    mutate(&mut task);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(task, now)],
        terminals: vec![],
    });
    s
}

#[test]
fn w_on_ci_failing_pr_emits_fix_ci_spawn() {
    let mut s = sidebar_with_pr(|t| t.ci = CiStatus::Failure);
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(key_code(KeyCode::Char('w')), &mut cmds);
    let prompt = match cmds.first() {
        Some(Command::Spawn { initial_prompt, .. }) => initial_prompt
            .clone()
            .expect("Spawn must carry an initial_prompt"),
        other => panic!("expected Spawn(fix-CI), got {other:?}"),
    };
    assert!(prompt.contains("CI is failing"), "{prompt}");
}

#[test]
fn w_on_ready_pr_is_noop() {
    let mut s = sidebar_with_pr(|t| {
        t.review = ReviewStatus::Approved;
        t.ci = CiStatus::Success;
    });
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(key_code(KeyCode::Char('w')), &mut cmds);
    // READY = nothing to "work on" — the user should be merging,
    // not spawning a fix-CI agent.
    assert!(
        cmds.is_empty(),
        "w on a READY PR must not spawn anything: {cmds:?}"
    );
}

#[test]
fn w_on_healthy_open_pr_is_noop() {
    // Open + pending review + green CI = nothing actionable.
    let mut s = sidebar_with_pr(|t| {
        t.ci = CiStatus::Success;
        t.review = ReviewStatus::Pending;
    });
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(key_code(KeyCode::Char('w')), &mut cmds);
    assert!(cmds.is_empty(), "{cmds:?}");
}

#[test]
fn shift_m_on_non_ready_pr_is_noop() {
    // Belt-and-braces: the match guard already gates on
    // merge_target_for_cursor; pin it.
    let mut s = sidebar_with_pr(|t| t.ci = CiStatus::Failure);
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(shift_char('M'), &mut cmds);
    s.handle_key(shift_char('M'), &mut cmds);
    assert!(
        cmds.is_empty(),
        "Shift-M on a CI-failing PR must not fire: {cmds:?}"
    );
}

#[test]
fn action_keys_on_repo_header_are_silent_noops() {
    // Cursor walked back onto a repo header — every action key
    // either targets `selected_workspace()` (None on a header) or
    // `selected_session_key()` (None on a header). They must all
    // be silent no-ops; the footer's contextual hints should also
    // hide the bindings, but the handlers are the safety net.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    s.on_event(&Event::Snapshot {
        workspaces: vec![make_workspace("owner/repo", "o/r#1", now)],
        terminals: vec![],
    });
    // Move up onto the repo header (row 0).
    s.handle_key(key_code(KeyCode::Up), &mut Vec::new());
    assert!(
        s.cursor_on_repo_header(),
        "fixture: cursor must land on the repo header for this test",
    );

    for key in [
        KeyCode::Char('w'),
        KeyCode::Char('s'),
        KeyCode::Char('c'),
        KeyCode::Char('m'),
        KeyCode::Char('z'),
    ] {
        let mut cmds: Vec<Command> = Vec::new();
        s.handle_key(key_code(key), &mut cmds);
        assert!(
            cmds.is_empty(),
            "{key:?} on a repo header must emit no command, got {cmds:?}",
        );
    }

    for shift_key in ['M', 'X', 'Z', 'A'] {
        let mut cmds: Vec<Command> = Vec::new();
        s.handle_key(shift_char(shift_key), &mut cmds);
        assert!(
            cmds.is_empty(),
            "Shift-{shift_key} on a repo header must emit no command, got {cmds:?}",
        );
    }
}

#[test]
fn s_on_workspace_emits_shell_spawn() {
    let mut s = sidebar_with_pr(|_| {});
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(key_code(KeyCode::Char('s')), &mut cmds);
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    match &cmds[0] {
        Command::Spawn { kind, .. } => assert!(
            matches!(kind, TerminalKind::Shell),
            "s must spawn Shell, got {kind:?}",
        ),
        other => panic!("expected Spawn, got {other:?}"),
    }
}

#[test]
fn shift_z_requires_two_presses_to_emit_long_snooze() {
    // 1-year snooze is effectively "hide forever" — a single
    // fat-fingered Shift-Z used to mute the row with no obvious
    // undo. Two-press latch mirrors Shift-X / Shift-M.
    let mut s = sidebar_with_pr(|_| {});
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(shift_char('Z'), &mut cmds);
    assert!(
        cmds.is_empty(),
        "first Shift-Z must arm the latch, not fire",
    );
    s.handle_key(shift_char('Z'), &mut cmds);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Snooze { until, .. } => {
            // ~365 days into the future is a long snooze.
            let delta = (*until - chrono::Utc::now()).num_days();
            assert!(
                (360..=370).contains(&delta),
                "Shift-Z must snooze ~1 year, got {delta} days",
            );
        }
        other => panic!("expected Snooze, got {other:?}"),
    }
}

#[test]
fn unrelated_key_disarms_shift_z_latch() {
    // Any key other than Shift-Z disarms the long-snooze prompt
    // (same pattern as Shift-X / Shift-M).
    let mut s = sidebar_with_pr(|_| {});
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(shift_char('Z'), &mut cmds);
    s.handle_key(key_code(KeyCode::Char('s')), &mut cmds); // disarms
    cmds.clear();
    s.handle_key(shift_char('Z'), &mut cmds);
    assert!(
        cmds.is_empty(),
        "after disarming, single Shift-Z must NOT fire snooze",
    );
}

#[test]
fn m_on_workspace_emits_mark_read() {
    let mut s = sidebar_with_pr(|_| {});
    let mut cmds: Vec<Command> = Vec::new();
    s.handle_key(key_code(KeyCode::Char('m')), &mut cmds);
    assert_eq!(cmds.len(), 1);
    assert!(matches!(cmds[0], Command::MarkRead { .. }), "{:?}", cmds[0]);
}

#[test]
fn contextual_bindings_surface_merge_on_ready_pr() {
    // The whole point of contextual bindings: the user sees the
    // merge shortcut in the footer at the moment it's actually
    // available, not buried in a static alphabet of every key.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut pr = make_task("o/r", "o/r#1", Utc::now());
    pr.review = ReviewStatus::Approved;
    pr.ci = CiStatus::Success;
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    let labels: Vec<&str> = s.contextual_bindings().iter().map(|b| b.label).collect();
    assert!(
        labels.contains(&"merge PR"),
        "READY PR must surface the merge binding, got {labels:?}",
    );
}

#[test]
fn contextual_bindings_surface_fix_ci_when_red() {
    let mut s = Sidebar::new(PaneId::new(1));
    let mut pr = make_task("o/r", "o/r#1", Utc::now());
    pr.ci = CiStatus::Failure;
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    let labels: Vec<&str> = s.contextual_bindings().iter().map(|b| b.label).collect();
    assert!(
        labels.contains(&"fix CI"),
        "CI-failing PR must surface fix CI, got {labels:?}",
    );
    assert!(
        !labels.contains(&"merge"),
        "merge must NOT show when CI is failing, got {labels:?}",
    );
}

#[test]
fn merge_target_fires_when_pr_is_ready() {
    // READY = approved + green CI (or no CI). The merge key should
    // only advertise itself for rows GitHub will actually let us
    // merge.
    let mut s = Sidebar::new(PaneId::new(1));
    let mut pr = make_task("o/r", "o/r#1", Utc::now());
    pr.review = ReviewStatus::Approved;
    pr.ci = CiStatus::Success;
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    assert!(s.merge_target_for_cursor().is_some());
}

#[test]
fn merge_target_is_hidden_without_approval() {
    let mut s = Sidebar::new(PaneId::new(1));
    let mut pr = make_task("o/r", "o/r#1", Utc::now());
    pr.review = ReviewStatus::Pending;
    pr.ci = CiStatus::Success;
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    assert!(s.merge_target_for_cursor().is_none());
}

#[test]
fn merge_target_is_hidden_when_ci_failing() {
    let mut s = Sidebar::new(PaneId::new(1));
    let mut pr = make_task("o/r", "o/r#1", Utc::now());
    pr.review = ReviewStatus::Approved;
    pr.ci = CiStatus::Failure;
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    assert!(s.merge_target_for_cursor().is_none());
}

#[test]
fn fix_target_fires_only_when_ci_is_failing() {
    // `f` is the narrow CI-fix mnemonic. PRs with green / running
    // CI must NOT advertise the binding — otherwise the hint bar
    // would lie and pressing `f` would no-op.
    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#1", CiStatus::Success);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    assert!(s.fix_target_for_cursor().is_none());

    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#2", CiStatus::Failure);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    let (_, prompt) = s.fix_target_for_cursor().expect("Failure CI must fire");
    assert!(prompt.contains("CI is failing"), "prompt: {prompt}");
}

#[test]
fn work_target_fires_for_ci_failure_same_as_fix() {
    // `w` is the polymorphic "work on this" key — it should subsume
    // the CI-failure case so users can use one key everywhere.
    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#3", CiStatus::Failure);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    let fix = s.fix_target_for_cursor();
    let work = s.work_target_for_cursor();
    assert!(work.is_some());
    assert_eq!(
        work.map(|(_, p)| p),
        fix.map(|(_, p)| p),
        "w on a CI-failing PR must produce the same prompt as f",
    );
}

#[test]
fn work_target_fires_for_issue_with_implement_prompt() {
    let mut s = Sidebar::new(PaneId::new(1));
    let issue = issue_task("o/r", "o/r#42", Some("Stack overflow when …"));
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(issue, Utc::now())],
        terminals: vec![],
    });
    let (_, prompt) = s
        .work_target_for_cursor()
        .expect("issue must produce a work target");
    assert!(
        prompt.contains("Implement GitHub issue #42"),
        "prompt: {prompt}"
    );
    assert!(
        prompt.contains("Closes #42"),
        "prompt must instruct the agent to close the issue: {prompt}"
    );
    assert!(
        prompt.contains("Stack overflow when"),
        "prompt must include the issue body: {prompt}"
    );
}

#[test]
fn work_target_skips_passing_pr_with_no_action() {
    // PR exists, CI green, no review issues — nothing to "work
    // on". `w` must hide itself so the hint bar stays honest.
    let mut s = Sidebar::new(PaneId::new(1));
    let pr = pr_task_with_ci("o/r", "o/r#5", CiStatus::Success);
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(pr, Utc::now())],
        terminals: vec![],
    });
    assert!(s.work_target_for_cursor().is_none());
}

#[test]
fn merged_closed_hidden_from_inbox_by_default() {
    // Default: Inbox is actionable-only. Merged + Closed go to the
    // Inactive mailbox, not Inbox. updated_at past the grace
    // window (30 min) so the freshly-merged-stays-in-inbox path
    // doesn't apply; we're testing the steady-state behavior.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let stale = now - Duration::hours(2);
    let mut merged = make_task("o/r", "o/r#1", stale);
    merged.state = pilot_core::TaskState::Merged;
    let mut closed = make_task("o/r", "o/r#2", stale);
    closed.state = pilot_core::TaskState::Closed;
    let open = make_task("o/r", "o/r#3", now);

    s.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(merged, now),
            Workspace::from_task(closed, now),
            Workspace::from_task(open, now),
        ],
        terminals: vec![],
    });

    let visible_keys: Vec<String> = s
        .visible_rows()
        .iter()
        .filter_map(|r| match r {
            VisibleRow::Workspace(k) => Some(k.as_str().to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        visible_keys.len(),
        1,
        "only the open PR should be in the default Inbox: got {visible_keys:?}",
    );
}

#[test]
fn show_inactive_in_inbox_surfaces_merged_and_closed() {
    // Toggle on → merged + closed appear in the Inbox alongside open
    // work. Verifies both the config plumbing and the filter switch.
    use std::collections::{BTreeSet, HashMap};

    let mut s = Sidebar::new(PaneId::new(1));
    let display = pilot_config::DisplayConfig {
        show_inactive_in_inbox: true,
        ..pilot_config::DisplayConfig::default()
    };
    s.apply_config(
        pilot_config::AttentionConfig::default(),
        BTreeSet::new(),
        HashMap::new(),
        None,
        &display,
        &pilot_config::UiDefaults::default(),
    );

    let now = Utc::now();
    let mut merged = make_task("o/r", "o/r#1", now);
    merged.state = pilot_core::TaskState::Merged;
    let mut closed = make_task("o/r", "o/r#2", now);
    closed.state = pilot_core::TaskState::Closed;
    let open = make_task("o/r", "o/r#3", now);

    s.on_event(&Event::Snapshot {
        workspaces: vec![
            Workspace::from_task(merged, now),
            Workspace::from_task(closed, now),
            Workspace::from_task(open, now),
        ],
        terminals: vec![],
    });

    let visible_count = s
        .visible_rows()
        .iter()
        .filter(|r| matches!(r, VisibleRow::Workspace(_)))
        .count();
    assert_eq!(
        visible_count, 3,
        "show_inactive_in_inbox=true must surface all three rows in Inbox",
    );
}

#[test]
fn work_key_emits_spawn_command_on_issue() {
    // End-to-end: pressing `w` on an issue row emits a Spawn(Agent)
    // command with the implement-issue prompt baked in.
    let mut s = Sidebar::new(PaneId::new(1));
    let issue = issue_task("o/r", "o/r#7", Some("Migrate to Postgres 16"));
    s.on_event(&Event::Snapshot {
        workspaces: vec![Workspace::from_task(issue, Utc::now())],
        terminals: vec![],
    });

    let mut cmds: Vec<Command> = Vec::new();
    let _ = s.handle_key(key_code(KeyCode::Char('w')), &mut cmds);

    assert_eq!(cmds.len(), 1, "exactly one Spawn must fire");
    match &cmds[0] {
        Command::Spawn {
            kind,
            initial_prompt,
            ..
        } => {
            assert!(
                matches!(kind, TerminalKind::Agent(_)),
                "must spawn an agent (not shell), got {kind:?}",
            );
            let prompt = initial_prompt.as_deref().unwrap_or("");
            assert!(prompt.contains("Implement GitHub issue #7"), "{prompt}");
        }
        other => panic!("expected Spawn, got {other:?}"),
    }
}

// ── Event::AgentState wiring ──────────────────────────────────────────
//
// The daemon broadcasts `Event::AgentState { Asking }` when Claude /
// Codex hits a yes-no prompt. Pilot tracks this in a sidebar-local
// asking-set (NOT on `workspace.sessions[i].state`, which gets
// blown away every poll cycle when `WorkspaceUpserted` reloads
// the workspace from the persisted store). These tests pin:
//
//   1. AgentState event → asking-set updated → row pill renders
//      (verified via the externally-visible jump-to-asking method).
//   2. WorkspaceUpserted between two AgentState events does NOT
//      clobber the set — the silent-clobber bug fix's regression
//      guard.
//   3. Notification fires once per Active→Asking edge, not on
//      repeat broadcasts.

fn agent_workspace(repo: &str, key: &str, now: DateTime<Utc>) -> Workspace {
    use pilot_core::{SessionKind, WorkspaceSession};
    use std::path::PathBuf;

    let mut w = make_workspace(repo, key, now);
    w.sessions.push(WorkspaceSession {
        id: pilot_core::SessionId::new(),
        workspace_key: w.key.clone(),
        name: "claude".into(),
        kind: SessionKind::Agent {
            agent_id: "claude".into(),
        },
        state: pilot_core::SessionRunState::Active,
        worktree_path: PathBuf::from("/tmp/x"),
        created_at: now,
        last_output_at: None,
        layout: Default::default(),
    });
    w
}

#[test]
fn agent_state_asking_makes_workspace_findable_by_bang() {
    // The row pill + header counter + `!` jump all read from the
    // sidebar's asking-set. We can't peek at it directly (private
    // field) but `focus_next_asking_workspace` is its public
    // observer. Use that as the proof the wiring worked end-to-end.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w = agent_workspace("owner/repo", "o/r#1", now);
    let key = ws_key(&w);
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });

    // Before the AgentState event: `!` finds nothing.
    assert!(
        !s.focus_next_asking_workspace(),
        "no asking workspace before the event",
    );

    s.on_event(&Event::AgentState {
        session_key: key.clone(),
        state: pilot_ipc::AgentState::Asking,
    });

    // After: `!` can find it.
    assert!(
        s.focus_next_asking_workspace(),
        "Event::AgentState {{ Asking }} must register in the asking-set",
    );
    assert_eq!(s.selected_session_key(), Some(&key));
}

#[test]
fn workspace_upserted_does_not_clobber_asking_state() {
    // REGRESSION for the silent-clobber bug: when polling runs
    // between Asking detection and the user looking at the
    // sidebar, the workspace is reloaded from the store and
    // re-broadcast as WorkspaceUpserted. The asking signal must
    // survive that re-broadcast — otherwise the `?` pill flashes
    // on for a second then disappears at the next poll tick.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w = agent_workspace("owner/repo", "o/r#1", now);
    let key = ws_key(&w);
    s.on_event(&Event::Snapshot {
        workspaces: vec![w.clone()],
        terminals: vec![],
    });

    // 1. Agent goes Asking.
    s.on_event(&Event::AgentState {
        session_key: key.clone(),
        state: pilot_ipc::AgentState::Asking,
    });
    assert!(s.focus_next_asking_workspace(), "asking after the event");

    // 2. Polling re-broadcasts the workspace (fresh from store —
    //    no transient asking state).
    s.on_event(&Event::WorkspaceUpserted(Box::new(w)));

    // 3. The asking-set must STILL hold the entry.
    s.focus_workspace_key(&ws_key(&make_workspace("owner/repo", "o/r#1", now))); // re-anchor cursor
    // focus_next_asking_workspace walks from after-current; reset
    // to None by re-snapshotting the cursor.
    assert!(
        s.focus_next_asking_workspace(),
        "WorkspaceUpserted must not clobber the asking state",
    );
}

#[test]
fn agent_state_asking_queues_a_desktop_notification() {
    // The Active → Asking transition must enqueue exactly one
    // notification (drained + fired by the wrapper). Repeated
    // broadcasts of the same state must NOT re-notify — the
    // pure-transition helper protects against banner spam when
    // the daemon re-emits on every output chunk.
    //
    // This test asserts the queue contents, never firing a real
    // `osascript` — that's the whole point of the drain split.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w = agent_workspace("owner/repo", "o/r#1", now);
    let key = ws_key(&w);
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    // Drain any setup-time notifications so the assertion is clean.
    let _ = s.drain_pending_notifications();

    s.on_event(&Event::AgentState {
        session_key: key.clone(),
        state: pilot_ipc::AgentState::Asking,
    });
    let queued = s.drain_pending_notifications();
    assert_eq!(queued.len(), 1, "first transition must enqueue once");
    assert!(
        queued[0].title.contains("needs input"),
        "title should signal urgency: {}",
        queued[0].title
    );

    // Repeat broadcast — no new notification.
    s.on_event(&Event::AgentState {
        session_key: key,
        state: pilot_ipc::AgentState::Asking,
    });
    let queued = s.drain_pending_notifications();
    assert!(
        queued.is_empty(),
        "Asking → Asking must not re-notify, got {queued:?}"
    );
}

#[test]
fn bang_jumps_to_next_asking_workspace() {
    // Three workspaces, only #2 is asking. Cursor starts on #1.
    // Calling `focus_next_asking_workspace` (what the `!` global key
    // invokes) should land on #2.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w1 = agent_workspace("owner/repo", "o/r#1", now);
    let w2 = agent_workspace("owner/repo", "o/r#2", now - Duration::seconds(1));
    let w3 = agent_workspace("owner/repo", "o/r#3", now - Duration::seconds(2));
    let k2 = ws_key(&w2);
    s.on_event(&Event::Snapshot {
        workspaces: vec![w1, w2, w3],
        terminals: vec![],
    });

    s.on_event(&Event::AgentState {
        session_key: k2.clone(),
        state: pilot_ipc::AgentState::Asking,
    });

    let moved = s.focus_next_asking_workspace();
    assert!(moved, "must report a move when a target exists");
    assert_eq!(s.selected_session_key(), Some(&k2));
}

#[test]
fn bang_is_a_noop_when_nothing_is_asking() {
    // The hint bar / discoverability story: pressing `!` with no
    // asking workspaces must not move the cursor or panic. Returns
    // false so the caller can skip the redraw + focus-switch.
    let mut s = Sidebar::new(PaneId::new(1));
    let now = Utc::now();
    let w = agent_workspace("owner/repo", "o/r#1", now);
    let starting_key = ws_key(&w);
    s.on_event(&Event::Snapshot {
        workspaces: vec![w],
        terminals: vec![],
    });
    let before = s.selected_session_key().cloned();
    assert_eq!(before.as_ref(), Some(&starting_key));

    let moved = s.focus_next_asking_workspace();
    assert!(!moved);
    assert_eq!(s.selected_session_key().cloned(), before);
}
