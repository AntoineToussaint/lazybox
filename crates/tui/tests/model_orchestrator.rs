//! Orchestrator-level tests for `realm::Model`. These replace the
//! deleted `app_loop.rs` tests that exercised the legacy `App` struct
//! end-to-end. Each test names the behaviour it covers — Tab cycle,
//! q-q quit latch, splitter resize, preselect, modal mount, etc.
//!
//! The tests use `Model::new_for_test` (a cfg(test)-only constructor
//! that swaps `CrosstermTerminalAdapter` for `TestTerminalAdapter`)
//! so they don't need a real terminal or raw mode.
// Tests may block while crossing UI timing thresholds; the crate-wide
// blocking-call ban in clippy.toml targets production paths.
#![allow(clippy::disallowed_methods)]

use chrono::Utc;
use crossterm::event::{KeyModifiers as CtKeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use lazybox_core::{SessionKey, Workspace, WorkspaceKey};
use lazybox_ipc::{Event as IpcEvent, channel};
use lazybox_tui::realm::Model;
use lazybox_tui::realm::model::{Id, PaneFocus, Preselect};
use tuirealm::event::{Key, KeyEvent, KeyModifiers};
use tuirealm::ratatui::layout::{Rect, Size};

fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
    let (client, _server) = channel::pair();
    Model::new_for_test(client, Size::new(120, 40)).expect("model init")
}

fn key(code: Key) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Choose the default/running agent from the deterministic work menu.
/// The second `w` dispatches immediately; there is no ambiguity timeout.
fn press_default_work(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) {
    m.dispatch_key(key(Key::Char('w')));
    assert!(m.leader_pending().is_some(), "`w` opens the work menu");
    m.dispatch_key(key(Key::Char('w')));
    assert!(m.leader_pending().is_none(), "`w w` resolves the menu");
}

fn key_with(code: Key, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn fresh_model_focuses_sidebar() {
    let m = build_model();
    assert_eq!(m.focus(), PaneFocus::Sidebar);
}

#[test]
fn tab_cycles_focus_through_panes() {
    // Tab cycles Sidebar → Right → Terminals → Sidebar when there's
    // no PTY swallowing keys. Inside a terminal with a live PTY,
    // Tab belongs to the shell — use `]]]` to exit. The fixture
    // built by `build_model()` has no terminals running, so Tab
    // cycles all the way around.
    let mut m = build_model();
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Right);
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Terminals);
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Sidebar);
}

#[test]
fn remapped_quit_binding_fires_on_single_key_chord() {
    // User puts `ui.action_keys.quit: "Ctrl-q"` in YAML. A single
    // Ctrl-q must quit immediately — no double-tap latch since the
    // chord is one key.
    let mut m = build_model();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("quit".to_string(), "Ctrl-q".to_string());
    m.apply_action_key_overrides(overrides);
    m.dispatch_key(key_with(Key::Char('q'), KeyModifiers::CONTROL));
    assert!(m.quit, "single-key remap must fire on first press");
}

#[test]
fn remapped_reply_binding_mounts_reply_modal() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    // Seed a workspace so reply has something to target.
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![Workspace::empty(
            WorkspaceKey::new("github:o/r#1"),
            "main",
            Utc::now(),
        )],
        terminals: vec![],
        projects: vec![],
    });
    // Reply moved into the catalog (Section::Workspace) so its
    // remap now lives in `ui.action_keys`, not `Keybindings.reply`.
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("reply".to_string(), "Ctrl-r".to_string());
    m.apply_action_key_overrides(overrides);
    m.dispatch_key(key_with(Key::Char('r'), KeyModifiers::CONTROL));
    assert_eq!(m.top_modal(), Some(&Id::Reply));
}

#[test]
fn remapped_new_workspace_binding_mounts_input() {
    let mut m = build_model();
    // `n` requires a focused project (Stage 3 of the Project
    // refactor). Seed one + a workspace under it so the sidebar's
    // cursor has a project_key to resolve.
    let project = lazybox_core::Project::new(
        lazybox_core::ProjectKey::github("owner", "repo"),
        "owner/repo",
        Utc::now(),
    );
    let workspace = {
        let mut w = Workspace::empty(WorkspaceKey::new("github:owner/repo#1"), "main", Utc::now());
        w.project_key = Some(project.key.clone());
        w
    };
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![workspace],
        terminals: vec![],
        projects: vec![project],
    });
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("new_workspace".to_string(), "Ctrl-n".to_string());
    m.apply_action_key_overrides(overrides);
    m.dispatch_key(key_with(Key::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(m.top_modal(), Some(&Id::NewWorkspace));
}

/// Regression for the "new-project row is unreachable" UX bug. The
/// user presses `x p`, types a name, and submits → the daemon
/// creates the project + broadcasts `ProjectUpserted`. Pre-fix, the
/// new RepoHeader row appeared but the cursor stayed put and j/k
/// skips header rows, so `n` (new workspace) had no project to
/// target. Now: the matching upsert auto-focuses the header and
/// mounts the new-workspace input so the user can keep typing.
#[test]
fn create_project_auto_focuses_new_header_and_opens_workspace_input() {
    use lazybox_core::{Project, ProjectKey};
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();

    // Simulate the user typing a project name + submitting.
    m.modal_stack.push(Id::NewProject);
    let _cmds = m.handle_input_submitted("scratch".into());
    // The submit hand-off stashed the name we're waiting on; no
    // modal is up yet because the daemon hasn't responded.
    assert_eq!(m.top_modal(), None);

    // Daemon responds with ProjectUpserted matching the name.
    let project_key = ProjectKey::local("scratch");
    let project = Project::new(project_key.clone(), "scratch", Utc::now());
    m.handle_daemon_event(IpcEvent::ProjectUpserted(Box::new(project)));

    // The hand-off should have auto-mounted the new-workspace
    // input so the user can keep typing.
    assert_eq!(
        m.top_modal(),
        Some(&Id::NewWorkspace),
        "ProjectUpserted matching a just-submitted x p should auto-open the new-workspace input",
    );
}

/// Shift+J on an issue workspace whose closing PR is in local
/// state emits `CollapseIntoPr` so the daemon can fold the rows.
#[test]
fn shift_j_on_issue_with_claiming_pr_emits_collapse_command() {
    use lazybox_ipc::{Command, TerminalKind};
    let _ = TerminalKind::Shell; // appease unused-import warning if Command is re-exported lazily

    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();

    // Issue workspace (no PR) + PR workspace whose closes_issues
    // includes the issue's task id. The dispatcher's local lookup
    // must connect them.
    let issue = task_with_issue("o/r#71", "fix the thing", None);
    let issue_id = issue.id.clone();
    let mut pr = task_with_pr("o/r#141");
    pr.closes_issues = vec![issue_id.clone()];
    let issue_ws = Workspace::from_task(issue, Utc::now());
    let pr_ws = Workspace::from_task(pr, Utc::now());

    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![issue_ws.clone(), pr_ws],
        terminals: vec![],
        projects: vec![],
    });

    // Force cursor onto the issue row — default sort can place
    // the PR first, which would route `x j` at the PR (where
    // CollapseIntoPr isn't available because the workspace has a
    // PR).
    let issue_key: SessionKey = (&issue_ws.key).into();
    assert!(
        m.__test_sidebar_mut().focus_workspace_key(&issue_key),
        "test setup: failed to focus the issue row",
    );

    m.dispatch_key(key(Key::Char('x')));
    m.dispatch_key(key(Key::Char('j')));

    let mut commands: Vec<Command> = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    let collapse = commands
        .iter()
        .find(|c| matches!(c, Command::CollapseIntoPr { .. }));
    assert!(
        collapse.is_some(),
        "x j on issue with claiming PR must emit CollapseIntoPr, got: {commands:#?}",
    );
}

/// `x j` on an issue whose PR isn't in local state surfaces a
/// footer notice instead of firing a no-op IPC.
#[test]
fn shift_j_on_orphan_issue_surfaces_notice_no_ipc() {
    use lazybox_ipc::Command;
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();

    let issue = task_with_issue("o/r#71", "stray issue", None);
    let issue_ws = Workspace::from_task(issue, Utc::now());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![issue_ws],
        terminals: vec![],
        projects: vec![],
    });

    m.dispatch_key(key(Key::Char('x')));
    m.dispatch_key(key(Key::Char('j')));

    let mut commands: Vec<Command> = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    assert!(
        !commands
            .iter()
            .any(|c| matches!(c, Command::CollapseIntoPr { .. })),
        "no CollapseIntoPr should fire when no PR closes the issue",
    );
}

#[test]
fn create_project_with_no_matching_upsert_does_not_auto_open_input() {
    use lazybox_core::{Project, ProjectKey};
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();

    // A `ProjectUpserted` arriving outside the x p flow (e.g.
    // first-sight registration during polling) must not hijack the
    // user — no modal should mount.
    let project = Project::new(
        ProjectKey::github("acme", "widget"),
        "acme/widget",
        Utc::now(),
    );
    m.handle_daemon_event(IpcEvent::ProjectUpserted(Box::new(project)));
    assert_eq!(m.top_modal(), None);
}

#[test]
fn remapped_help_binding_mounts_ask_lazybox() {
    // Remap the discovery surface to lowercase `h` and verify Ask opens.
    let mut m = build_model();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("open_help".to_string(), "h".to_string());
    m.apply_action_key_overrides(overrides);
    m.dispatch_key(key(Key::Char('h')));
    assert_eq!(m.top_modal(), Some(&Id::HelpAsk));
}

#[test]
fn enter_on_sidebar_focuses_activity_pane() {
    // Used to be a dead binding (advertised "open" in the keymap
    // but never matched). Now it jumps the user from the row into
    // the activity feed, which is the natural read flow: pick a
    // workspace, hit Enter, read the comments.
    let mut m = build_model();
    assert_eq!(m.focus(), PaneFocus::Sidebar);
    m.dispatch_key(key(Key::Enter));
    assert_eq!(
        m.focus(),
        PaneFocus::Right,
        "Enter on the sidebar must focus the Activity pane",
    );
}

#[test]
fn enter_on_right_pane_does_not_move_focus() {
    // Enter inside the right pane toggles the activity section
    // collapse — it must NOT jump focus elsewhere. (The sidebar
    // Enter handler is gated on `focus == Sidebar`; verify the
    // guard works.)
    let mut m = build_model();
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Right);
    m.dispatch_key(key(Key::Enter));
    assert_eq!(
        m.focus(),
        PaneFocus::Right,
        "Enter in the Activity pane stays in the Activity pane",
    );
}

#[test]
fn single_q_arms_latch_does_not_quit() {
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('q')));
    assert!(!m.quit, "first q must not quit");
    assert!(m.q_arm_pending(), "first q arms the latch");
}

#[test]
fn double_q_within_window_quits() {
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('q')));
    m.dispatch_key(key(Key::Char('q')));
    assert!(m.quit, "second q within the window quits");
}

#[test]
fn other_key_disarms_q_latch() {
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('q')));
    m.dispatch_key(key(Key::Down));
    assert!(!m.q_arm_pending(), "any non-q key disarms the latch");
    m.dispatch_key(key(Key::Char('q')));
    assert!(!m.quit, "after disarm, single q does not quit");
}

#[test]
fn shift_left_shrinks_sidebar() {
    let mut m = build_model();
    let (start_sidebar, _) = m.split_pcts();
    m.dispatch_key(key_with(Key::Left, KeyModifiers::SHIFT));
    let (after, _) = m.split_pcts();
    assert!(
        after < start_sidebar,
        "Shift-Left shrinks sidebar ({start_sidebar}% → {after}%)"
    );
}

#[test]
fn shift_right_grows_sidebar() {
    let mut m = build_model();
    let (start_sidebar, _) = m.split_pcts();
    m.dispatch_key(key_with(Key::Right, KeyModifiers::SHIFT));
    let (after, _) = m.split_pcts();
    assert!(after > start_sidebar);
}

#[test]
fn shift_arrows_clamp_at_min_max() {
    let mut m = build_model();
    // Mash Shift-Left until clamped at min.
    for _ in 0..50 {
        m.dispatch_key(key_with(Key::Left, KeyModifiers::SHIFT));
    }
    let (lo, _) = m.split_pcts();
    assert!(lo >= 15, "sidebar pct stays >= SPLIT_MIN (got {lo})");
    // Mash Shift-Right until clamped at max.
    for _ in 0..50 {
        m.dispatch_key(key_with(Key::Right, KeyModifiers::SHIFT));
    }
    let (hi, _) = m.split_pcts();
    assert!(hi <= 80, "sidebar pct stays <= SPLIT_MAX (got {hi})");
}

#[test]
fn question_mark_mounts_ask_lazybox() {
    // `dispatch_key` bypasses the run-loop's "modal is up" guard
    // and drives `handle_pane_key` directly, so this test verifies
    // the orchestrator-side wiring rather than the run-loop guard.
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('?')));
    assert_eq!(m.top_modal(), Some(&Id::HelpAsk));
}

#[test]
fn handle_daemon_event_applies_preselect_on_first_snapshot() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let target_key = "github:owner/repo#42";
    let target = SessionKey::from(target_key);
    m = m.with_preselect(Preselect {
        workspace_key: target.clone(),
        session_id_raw: None,
    });
    // Build a snapshot with a single workspace matching the target.
    let workspace = Workspace::empty(WorkspaceKey(target_key.to_string()), "main", Utc::now());
    let snapshot = IpcEvent::Snapshot {
        workspaces: vec![workspace],
        terminals: Vec::new(),
        projects: vec![],
    };
    m.handle_daemon_event(snapshot);
    // Sidebar should now have the target workspace selected.
    assert_eq!(
        m.sidebar().selected_workspace_key().map(|k| k.as_str()),
        Some(target.as_str())
    );
}

#[test]
fn click_in_right_pane_changes_focus() {
    let mut m = build_model();
    // Splash modal blocks the run loop's crossterm path, but tests
    // bypass that. Click somewhere clearly in the right column.
    let area = Rect::new(0, 0, 100, 30);
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 80, // > 40% of 100 → outside sidebar
            row: 5,     // < 25% of 30 → in right-top
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    assert_eq!(m.focus(), PaneFocus::Right);
}

#[test]
fn click_in_sidebar_keeps_or_returns_focus_to_sidebar() {
    let mut m = build_model();
    // Move focus elsewhere first.
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Right);
    // Click in sidebar area.
    let area = Rect::new(0, 0, 100, 30);
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5, // well inside the 40% sidebar column
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    assert_eq!(m.focus(), PaneFocus::Sidebar);
}

#[test]
fn wheel_outside_terminal_pane_is_a_silent_noop() {
    // Scroll outside the terminal pane must NOT touch the active
    // terminal's viewport — sidebar / activity own their own scroll.
    // Pre-fix the wheel handler set a footer notice on every event;
    // this test pins the "silent bail" path that replaced it.
    let mut m = build_model();
    let area = Rect::new(0, 0, 100, 30);
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5, // sidebar column
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    // Sidebar focus is the start state; scroll outside doesn't shift focus.
    assert_eq!(m.focus(), PaneFocus::Sidebar);
    // No notice surfaced from the scroll path.
    // (The footer notice for scroll was retired in the cleanup commit.)
}

#[test]
fn wheel_over_sidebar_scrolls_display_without_changing_selection() {
    // A wheel event over the sidebar rect must only move the viewport
    // offset. The selection drives the right pane, terminal stack,
    // and focus, so a trackpad flick changing it yanked the user to a
    // different workspace (#290).
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let workspaces: Vec<_> = (1..=30)
        .map(|i| {
            Workspace::empty(
                WorkspaceKey(format!("github:owner/repo#{i}")),
                "main",
                Utc::now(),
            )
        })
        .collect();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces,
        terminals: Vec::new(),
        projects: vec![],
    });
    let selected = m.sidebar().selected_workspace_key().cloned();
    assert!(selected.is_some(), "snapshot selects a workspace");

    let area = Rect::new(0, 0, 100, 30);
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5, // sidebar column
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );

    assert_eq!(
        m.sidebar().selected_workspace_key().cloned(),
        selected,
        "wheel over the sidebar must not change the selected workspace"
    );
    assert!(
        m.sidebar().__test_scroll() > 0,
        "wheel over the sidebar moved the viewport offset"
    );
}

#[test]
fn drag_on_sidebar_splitter_changes_split() {
    let mut m = build_model();
    let (before, _) = m.split_pcts();
    let area = Rect::new(0, 0, 100, 30);
    // Down on the splitter line (col == sidebar.x + sidebar.width).
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: before, // splitter sits at this column
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    // Drag well into the right column.
    m.dispatch_mouse_in(
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 70,
            row: 10,
            modifiers: CtKeyModifiers::empty(),
        },
        area,
    );
    let (after, _) = m.split_pcts();
    assert!(
        after > before,
        "dragging right widens sidebar ({before}% → {after}%)"
    );
}

#[test]
fn r_mounts_reply_modal_from_sidebar() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let target_key = "github:owner/repo#42";
    let workspace = Workspace::empty(WorkspaceKey(target_key.to_string()), "main", Utc::now());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![workspace],
        terminals: Vec::new(),
        projects: vec![],
    });
    assert_eq!(m.focus(), PaneFocus::Sidebar);
    m.dispatch_key(key(Key::Char('r')));
    assert_eq!(m.top_modal(), Some(&Id::Reply));
}

#[test]
fn r_mounts_reply_modal_from_right_pane() {
    // Regression: `r` used to only fire when focus == Sidebar.
    // Users reading the Activity feed (focus == Right) hit `r`,
    // got nothing, and reported "r doesn't work."
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let target_key = "github:owner/repo#42";
    let mut workspace = Workspace::empty(WorkspaceKey(target_key.to_string()), "main", Utc::now());
    // Activity keeps the pane visible so it's focusable — the read
    // flow this test exercises (read a comment, hit `r` to reply).
    workspace.activity.push(lazybox_core::Activity {
        author: "alice".into(),
        body: "needs a tweak".into(),
        created_at: Utc::now(),
        kind: lazybox_core::ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    });
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![workspace],
        terminals: Vec::new(),
        projects: vec![],
    });
    // Tab to the Right pane (Activity).
    m.dispatch_key(key(Key::Tab));
    assert_eq!(m.focus(), PaneFocus::Right);
    m.dispatch_key(key(Key::Char('r')));
    assert_eq!(m.top_modal(), Some(&Id::Reply));
}

#[test]
fn slash_opens_sidebar_search_through_the_catalog() {
    // `/` migrated into the action catalog (Section::Sidebar, issue
    // #98). Pressing it from sidebar focus must dispatch through
    // `dispatch_action` → `Sidebar::open_search`, not the deleted
    // per-pane match arm. Proves the catalog absorbed the key without
    // regressing dispatch.
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let workspace = Workspace::empty(WorkspaceKey("github:o/r#1".into()), "main", Utc::now());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![workspace],
        terminals: Vec::new(),
        projects: vec![],
    });
    assert_eq!(m.focus(), PaneFocus::Sidebar);
    assert!(!m.sidebar().search_editing());
    m.dispatch_key(key(Key::Char('/')));
    assert!(
        m.sidebar().search_editing(),
        "`/` must open the sidebar search bar via the catalog",
    );
}

#[test]
fn remapped_search_binding_opens_search() {
    // The migration's payoff: a sidebar list key is now remappable
    // via `ui.action_keys`. Rebind `open_search` to Ctrl-f and assert
    // the chord opens search — impossible while `/` was hard-coded in
    // the pane handler.
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let workspace = Workspace::empty(WorkspaceKey("github:o/r#1".into()), "main", Utc::now());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![workspace],
        terminals: Vec::new(),
        projects: vec![],
    });
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("open_search".to_string(), "Ctrl-f".to_string());
    m.apply_action_key_overrides(overrides);
    m.dispatch_key(key_with(Key::Char('f'), KeyModifiers::CONTROL));
    assert!(
        m.sidebar().search_editing(),
        "remapped Ctrl-f must open search",
    );
}

#[test]
fn out_of_scope_with_active_session_queues_a_prompt() {
    // Phase 2 of the rescope flow: when the daemon sends a
    // `WorkspaceOutOfScope` event (a workspace fell out of the
    // filter while having running terminals), the model must
    // mount a Confirm modal asking the user before killing.
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let target = "github:owner/repo#42";
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: lazybox_core::WorkspaceKey::new(target),
        label: "owner/repo#42".into(),
        title: None,
        active_terminal_count: 1,
    });
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
}

#[test]
fn out_of_scope_prompts_queue_one_at_a_time() {
    // Two workspaces fall out of scope back-to-back. Only one
    // prompt is mounted at a time; the next surfaces when the
    // first is dismissed.
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: lazybox_core::WorkspaceKey::new("github:o/a#1"),
        label: "o/a#1".into(),
        title: None,
        active_terminal_count: 1,
    });
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: lazybox_core::WorkspaceKey::new("github:o/b#2"),
        label: "o/b#2".into(),
        title: None,
        active_terminal_count: 2,
    });
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    // Press Esc to dismiss the first prompt (= "no, keep it").
    m.update(lazybox_tui::realm::Msg::ModalDismissed);
    // Next prompt should be live now.
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    m.update(lazybox_tui::realm::Msg::ModalDismissed);
    // Queue drained.
    assert_eq!(m.top_modal(), None);
}

#[test]
fn confirm_modal_y_dismisses_through_channel_pipeline() {
    // Regression: prior tests called `m.update(Msg::Confirmed(_))`
    // directly, which bypassed the real channel → listener → app.tick
    // path. The user reported that Y / N do nothing on the
    // out-of-scope Confirm — only Esc works — so this test must drive
    // the keypress through `dispatch_modal_key`, the same path the
    // run loop uses.
    let mut m = build_model();
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: WorkspaceKey::new("github:o/r#1"),
        label: "o/r#1".into(),
        title: None,
        active_terminal_count: 1,
    });
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    m.dispatch_modal_key(key(Key::Char('y')));
    assert_eq!(
        m.top_modal(),
        None,
        "Y must dismiss the Confirm modal (Msg::Confirmed(true))",
    );
}

#[test]
fn confirm_modal_n_dismisses_through_channel_pipeline() {
    let mut m = build_model();
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: WorkspaceKey::new("github:o/r#1"),
        label: "o/r#1".into(),
        title: None,
        active_terminal_count: 1,
    });
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    m.dispatch_modal_key(key(Key::Char('n')));
    assert_eq!(
        m.top_modal(),
        None,
        "N must dismiss the Confirm modal (Msg::Confirmed(false))",
    );
}

#[test]
fn confirm_modal_esc_dismisses_through_channel_pipeline() {
    // Sanity: Esc works today per the user's bug report. Keep this
    // test alongside the Y / N tests so future regressions in *any*
    // of the three keypress paths are caught.
    let mut m = build_model();
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: WorkspaceKey::new("github:o/r#1"),
        label: "o/r#1".into(),
        title: None,
        active_terminal_count: 1,
    });
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    m.dispatch_modal_key(key(Key::Esc));
    assert_eq!(m.top_modal(), None, "Esc must dismiss the Confirm modal");
}

#[test]
fn out_of_scope_queued_during_help_modal_drains_on_help_dismiss() {
    // Bug previously: if the user had Help open when the daemon
    // emitted WorkspaceOutOfScope, the prompt sat in the queue
    // forever — only a removal-prompt dismissal triggered the
    // drain. Now ANY ModalDismissed retries the queue.
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    // Open Ask Lazybox. The help modal subscribes to the bus the same way
    // any modal does; we don't drive its construction directly so
    // we just verify the queue behavior via the run-through.
    m.dispatch_key(key(Key::Char('?')));
    assert_eq!(m.top_modal(), Some(&Id::HelpAsk));
    // Daemon sends an out-of-scope event while Help is up.
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: lazybox_core::WorkspaceKey::new("github:o/r#1"),
        label: "o/r#1".into(),
        title: None,
        active_terminal_count: 1,
    });
    // Ask still on top — the queued prompt hasn't surfaced yet.
    assert_eq!(m.top_modal(), Some(&Id::HelpAsk));
    // Dismiss Ask. Now the prompt should mount.
    m.update(lazybox_tui::realm::Msg::ModalDismissed);
    assert_eq!(
        m.top_modal(),
        Some(&Id::RemoveOutOfScope),
        "queued out-of-scope prompt must surface after Help dismisses"
    );
}

#[test]
fn merge_pending_event_mounts_confirm_modal() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::WorkspaceMergePending {
        issue_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-71"),
        pr_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-141"),
        issue_label: "o/r#71".into(),
        pr_label: "o/r#141".into(),
        active_terminal_count: 1,
    });
    assert_eq!(m.top_modal(), Some(&Id::MergeConfirm));
}

#[test]
fn merge_confirm_yes_sends_accept_command() {
    use lazybox_ipc::Command;
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::WorkspaceMergePending {
        issue_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-71"),
        pr_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-141"),
        issue_label: "o/r#71".into(),
        pr_label: "o/r#141".into(),
        active_terminal_count: 1,
    });
    m.update(lazybox_tui::realm::Msg::Confirmed(true));
    // Drain the IPC pipe — we expect a ConfirmMerge { accept: true }.
    let cmd = server.rx.try_recv().expect("ConfirmMerge command emitted");
    match cmd {
        Command::ConfirmMerge {
            issue_workspace_key,
            pr_workspace_key,
            accept,
        } => {
            assert_eq!(issue_workspace_key.as_str(), "github-o-r-71");
            assert_eq!(pr_workspace_key.as_str(), "github-o-r-141");
            assert!(accept);
        }
        other => panic!("expected ConfirmMerge, got {other:?}"),
    }
    assert_eq!(m.top_modal(), None, "modal dismisses on confirm");
}

#[test]
fn merge_confirm_esc_dismisses_silently_so_re_prompt_can_self_heal() {
    use lazybox_ipc::Command;
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::WorkspaceMergePending {
        issue_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-71"),
        pr_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-141"),
        issue_label: "o/r#71".into(),
        pr_label: "o/r#141".into(),
        active_terminal_count: 1,
    });
    m.update(lazybox_tui::realm::Msg::ModalDismissed);
    // Pre-fix Esc sent `ConfirmMerge { accept: false }`, which pinned
    // the issue in `rejected_merge` for the whole session — the user
    // never saw the prompt again until daemon restart. Now: silent
    // dismissal; the daemon re-fires after 5 min so the prompt
    // self-heals if the user wanted to act on it later.
    let mut commands: Vec<Command> = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    let confirm = commands
        .iter()
        .find(|c| matches!(c, Command::ConfirmMerge { .. }));
    assert!(
        confirm.is_none(),
        "Esc on merge modal must NOT signal ConfirmMerge, got: {commands:?}",
    );
}

/// GitHub issue (not PR) — `url` carries `/issues/<n>`, no branch.
fn task_with_issue(key: &str, title: &str, body: Option<&str>) -> lazybox_core::Task {
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
    Task {
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: title.into(),
        body: body.map(str::to_string),
        state: TaskState::Open,
        role: TaskRole::Assignee,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{path}/issues/{num}"),
        repo: Some("o/r".into()),
        branch: None,
        base_branch: None,
        updated_at: Utc::now(),
        created_at: None,
        closed_at: None,
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: lazybox_core::Mergeable::Unknown,
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

/// Pressing `w` on an issue with a claude already running for that
/// workspace must rewrite the Spawn into InjectPrompt — otherwise the
/// user's implement-issue prompt spawns a second claude tab instead
/// of being delivered to the one already on-screen. Regression: the
/// rewrite previously skipped the catalog path that handles
/// `Action::Work` on an issue.
#[test]
fn w_on_issue_with_running_claude_injects_implement_prompt() {
    use lazybox_ipc::{Command, TerminalId, TerminalKind};
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let issue = task_with_issue("o/r#42", "Migrate to Postgres 16", None);
    let ws = Workspace::from_task(issue, Utc::now());
    let ws_key = ws.key.clone();

    // Seed: workspace exists, and a claude is already running for it.
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });
    m.handle_daemon_event(IpcEvent::TerminalSpawned {
        model_label: None,
        terminal_id: TerminalId(7),
        session_key: SessionKey::from(&ws_key),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
    });

    // TerminalSpawned auto-focuses the terminal pane. In real usage
    // the user Tab's back to the sidebar; do the same here so `w`
    // hits the catalog instead of being written into the PTY.
    while m.focus() != PaneFocus::Sidebar {
        m.dispatch_key(key(Key::Tab));
    }

    // Choose default work (`w w` → running-or-default agent).
    press_default_work(&mut m);

    // Drain and look for the inject (NOT a duplicate spawn).
    let mut commands: Vec<Command> = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    let inject = commands.iter().find_map(|c| match c {
        Command::InjectPrompt {
            terminal_id,
            prompt,
            fallback_spawn,
        } => Some((*terminal_id, prompt.clone(), fallback_spawn.clone())),
        _ => None,
    });
    let (terminal_id, prompt, fallback) = inject.unwrap_or_else(|| {
        panic!("w on issue with running claude must emit InjectPrompt — got: {commands:#?}")
    });
    assert_eq!(terminal_id, TerminalId(7), "must target the running claude");
    assert!(
        prompt.contains("Implement GitHub issue #42"),
        "prompt should carry the implement-issue text, got: {prompt}",
    );
    assert!(
        fallback.is_some(),
        "InjectPrompt should always carry a SpawnFallback for the dead-id race",
    );

    // And there must NOT be a duplicate Spawn alongside the inject —
    // otherwise the user gets two claude tabs instead of one.
    let spawn_count = commands
        .iter()
        .filter(|c| {
            matches!(
                c,
                Command::Spawn {
                    kind: TerminalKind::Agent(_),
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        spawn_count, 0,
        "must rewrite to inject — no duplicate Spawn should fire alongside",
    );
    let _ = prompt; // keep `prompt` in scope for assertions above
}

/// Same as above but the user presses `w` from the right (activity)
/// pane. The right pane has its own `w` handler that emits a Spawn
/// into the orchestrator's cmd queue — the rewrite must catch that
/// path too, not only the catalog dispatch path.
#[test]
fn w_on_issue_from_right_pane_also_injects() {
    use lazybox_ipc::{Command, TerminalId, TerminalKind};
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    // A description keeps the Activity pane visible (and focusable)
    // for an issue with no comment activity yet.
    let issue = task_with_issue("o/r#42", "Migrate to Postgres 16", Some("Move the store."));
    let ws = Workspace::from_task(issue, Utc::now());
    let ws_key = ws.key.clone();

    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });
    m.handle_daemon_event(IpcEvent::TerminalSpawned {
        model_label: None,
        terminal_id: TerminalId(7),
        session_key: SessionKey::from(&ws_key),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
    });

    // Get to the right pane. TerminalSpawned auto-focused terminals;
    // Tab to sidebar, Tab again to right pane.
    while m.focus() != PaneFocus::Right {
        m.dispatch_key(key(Key::Tab));
    }

    press_default_work(&mut m);

    let mut commands: Vec<Command> = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    let inject_found = commands.iter().any(|c| {
        matches!(
            c,
            Command::InjectPrompt {
                terminal_id: TerminalId(7),
                ..
            }
        )
    });
    assert!(
        inject_found,
        "right-pane `w` on issue with running claude should InjectPrompt, got: {commands:#?}",
    );
}

/// `w` must honor a multi-selected activity row regardless of which
/// pane has focus. The selection lives in the right pane, but a user
/// who selects a comment (right pane), then Tabs back to the sidebar
/// and presses `w`, must still get the address-comments spawn — not
/// the focus-dependent "fix CI" fallback. Regression for #77.
#[test]
fn sidebar_w_honors_activity_selection() {
    use chrono::Utc;
    use lazybox_core::{Activity, ActivityKind, CiStatus, Workspace};
    use lazybox_ipc::Command;

    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    // PR with failing CI — without a selection, `w` would spawn the
    // "fix CI" prompt. A selected comment must take priority.
    let mut task = task_with_pr("o/r#1");
    task.ci = CiStatus::Failure;
    let mut ws = Workspace::from_task(task, Utc::now());
    ws.activity.push(Activity {
        author: "alice".into(),
        body: "nit on line 4".into(),
        created_at: Utc::now(),
        kind: ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    });
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });

    // Select the focused activity row from the right pane with Space,
    // then return focus to the sidebar.
    while m.focus() != PaneFocus::Right {
        m.dispatch_key(key(Key::Tab));
    }
    m.dispatch_key(key(Key::Char(' ')));
    while m.focus() != PaneFocus::Sidebar {
        m.dispatch_key(key(Key::Tab));
    }

    // Choose default work (`w w`) from the sidebar.
    press_default_work(&mut m);

    let mut commands: Vec<Command> = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    let prompt = commands
        .iter()
        .find_map(|c| match c {
            Command::Spawn {
                initial_prompt: Some(p),
                on_main: false,
                ..
            } => Some(p.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("sidebar `w` must spawn an agent, got: {commands:#?}"));
    assert!(
        prompt.contains("Address the following review comments"),
        "sidebar `w` must honor the selected activity (address-comments), not the \
         focus-dependent fix-CI fallback; got:\n{prompt}",
    );
}

fn task_with_pr(key: &str) -> lazybox_core::Task {
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
    let (path, num) = key.rsplit_once('#').unwrap_or((key, "1"));
    Task {
        id: TaskId {
            source: "github".into(),
            key: key.into(),
        },
        title: format!("PR {key}"),
        body: None,
        state: TaskState::Open,
        role: TaskRole::Author,
        ci: CiStatus::None,
        review: ReviewStatus::None,
        checks: vec![],
        unread_count: 0,
        url: format!("https://github.com/{path}/pull/{num}"),
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
        closes_issues: vec![],
    }
}

#[test]
fn shift_a_with_no_sessions_does_not_mount_picker() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });
    // `x a` should be a no-op (no sessions to adopt).
    m.dispatch_key(key(Key::Char('x')));
    m.dispatch_key(key(Key::Char('a')));
    assert_eq!(
        m.top_modal(),
        None,
        "x a on a session-less workspace must not mount a picker",
    );
}

#[test]
fn shift_a_with_sessions_mounts_adopt_picker() {
    use chrono::Duration;
    use lazybox_core::{SessionKind, WorkspaceSession};

    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    // Sidebar sorts by `updated_at` desc within a repo group, so we
    // bias `source` slightly newer than `target` to make the cursor
    // (which starts at row 0) land on the source — the workspace
    // `x a` is supposed to read from.
    let now = Utc::now();
    let mut src_task = task_with_pr("o/r#1");
    src_task.updated_at = now + Duration::seconds(1);
    let mut source = Workspace::from_task(src_task, now);
    source.add_session(WorkspaceSession::new(
        source.key.clone(),
        SessionKind::Shell,
        std::path::PathBuf::from("/tmp/x"),
        now,
    ));
    let mut tgt_task = task_with_pr("o/r#2");
    tgt_task.updated_at = now;
    let target = Workspace::from_task(tgt_task, now);
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![source, target],
        terminals: vec![],
        projects: vec![],
    });
    // Sanity: the cursor must be on a workspace with sessions for
    // `x a` to fire. If this fails, the picker test diagnoses the
    // selection state rather than the keybinding wiring.
    let selected = m.sidebar().selected_workspace().cloned();
    assert!(
        selected.as_ref().is_some_and(|w| !w.sessions.is_empty()),
        "fixture: cursor must land on the source workspace; got {selected:?}"
    );
    m.dispatch_key(key(Key::Char('x')));
    m.dispatch_key(key(Key::Char('a')));
    assert_eq!(
        m.top_modal(),
        Some(&Id::AdoptTarget),
        "x a on a workspace with sessions must mount the picker",
    );
}

#[test]
fn merge_pending_dedupes_re_emits_for_same_issue() {
    // The daemon retries `WorkspaceMergePending` on every poll until
    // confirmed; the TUI must NOT requeue duplicates. Otherwise the
    // user would dismiss the modal only to see it re-mount from the
    // queue.
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    for _ in 0..3 {
        m.handle_daemon_event(IpcEvent::WorkspaceMergePending {
            issue_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-71"),
            pr_workspace_key: lazybox_core::WorkspaceKey::new("github-o-r-141"),
            issue_label: "o/r#71".into(),
            pr_label: "o/r#141".into(),
            active_terminal_count: 1,
        });
    }
    // Dismiss the active prompt — the queue should be empty, not full
    // of duplicates of the same prompt.
    m.update(lazybox_tui::realm::Msg::ModalDismissed);
    assert_eq!(m.top_modal(), None, "queue must not requeue duplicates");
}

#[test]
fn tick_right_drives_auto_mark_and_emits_command() {
    // Regression: `Model::tick_right` was added because the run loop
    // never called `right.tick()` — auto-mark-read literally never
    // fired, so the unread badge stayed stuck on the sidebar even
    // after the user navigated past every comment. This test pins
    // that wiring: ticker drives the inner pane AND the daemon
    // gets `Command::MarkActivityRead` so the read state persists.
    use chrono::Utc;
    use lazybox_core::{Activity, ActivityKind, Workspace};
    use lazybox_ipc::Command;
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let mut ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    // One unread activity row — cursor lands on it on first paint.
    ws.activity.push(Activity {
        author: "alice".into(),
        body: "needs your attention".into(),
        created_at: Utc::now(),
        kind: ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    });
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });
    // Auto-mark-read fires after `ui.auto_mark_delay` (default 1s).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    m.tick_right();
    // The lazy PR-details fetch also queues here; we just need to
    // find the MarkActivityRead among the queued commands. The
    // assertion is "this command was emitted at all" — order with
    // other commands isn't the contract.
    let mut commands: Vec<Command> = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    let marked = commands.iter().find_map(|c| match c {
        Command::MarkActivityRead { session_key, index } => Some((session_key.clone(), *index)),
        _ => None,
    });
    let (session_key, index) =
        marked.expect("tick_right must emit Command::MarkActivityRead after the delay");
    assert_eq!(session_key.as_str(), "github-o-r-1");
    assert_eq!(index, 0);
}

// ── Grouped two-step (leader-key) chords — issue #126 ────────────────

/// `g` in the sidebar arms the leader (the `g …` prefix) — the
/// which-key popup's pending state — without firing anything or
/// mounting a modal. Which entries continue the prefix is a pure
/// function of the catalog (`seq_continuations`).
#[test]
fn leader_g_arms_github_group_when_workspace_selected() {
    use lazybox_tui_core::action::{ChordCode, KeyStroke};
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![Workspace::from_task(task_with_pr("o/r#1"), Utc::now())],
        terminals: vec![],
        projects: vec![],
    });

    m.dispatch_key(key(Key::Char('g')));
    assert_eq!(
        m.leader_pending(),
        Some(KeyStroke::new(false, false, false, ChordCode::Char('g'))),
    );
    assert_eq!(m.top_modal(), None, "arming must not mount a modal");
}

/// The github leader's second key resolves a real action: `g` then
/// `m` routes MergePr through the unified destructive gate, which
/// mounts the confirm modal, and clears the latch.
#[test]
fn leader_g_then_m_mounts_merge_confirm() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let pr_ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    let pr_key: SessionKey = (&pr_ws.key).into();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![pr_ws],
        terminals: vec![],
        projects: vec![],
    });
    assert!(m.__test_sidebar_mut().focus_workspace_key(&pr_key));

    m.dispatch_key(key(Key::Char('g')));
    m.dispatch_key(key(Key::Char('m')));
    assert_eq!(m.leader_pending(), None, "second key must clear the latch");
    assert_eq!(m.top_modal(), Some(&Id::ActionConfirm));
}

/// Esc after the leader cancels the chord cleanly — no action fires,
/// no modal mounts.
#[test]
fn leader_g_then_esc_cancels() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![Workspace::from_task(task_with_pr("o/r#1"), Utc::now())],
        terminals: vec![],
        projects: vec![],
    });

    m.dispatch_key(key(Key::Char('g')));
    assert!(m.leader_pending().is_some());
    m.dispatch_key(key(Key::Esc));
    assert_eq!(m.leader_pending(), None);
    assert_eq!(m.top_modal(), None);
}

/// A key with no binding in the group cancels the chord but is then
/// re-dispatched normally instead of being silently swallowed (issue
/// #165). `g` arms the github group; `z` isn't a github in-group key,
/// so the chord cancels and `z` runs its own action (ToggleSnooze),
/// mounting the snooze duration picker.
#[test]
fn leader_g_then_unmapped_key_redispatches() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let pr_ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    let pr_key: SessionKey = (&pr_ws.key).into();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![pr_ws],
        terminals: vec![],
        projects: vec![],
    });
    assert!(m.__test_sidebar_mut().focus_workspace_key(&pr_key));

    m.dispatch_key(key(Key::Char('g')));
    m.dispatch_key(key(Key::Char('z'))); // 'z' isn't a github in-group key
    assert_eq!(m.leader_pending(), None, "chord must cancel");
    assert_eq!(
        m.top_modal(),
        Some(&Id::SnoozeDuration),
        "the re-dispatched 'z' must run ToggleSnooze, not vanish"
    );
}

/// Arming is purely catalog-driven: `g` arms the leader from sidebar
/// focus even with no workspace selected — the chord resolves at the
/// keyboard layer regardless of target. The completed chord then
/// no-ops in `dispatch_action` if nothing is actionable — same
/// contract as every other workspace action.
#[test]
fn leader_g_arms_from_sidebar_without_workspace() {
    use lazybox_tui_core::action::{ChordCode, KeyStroke};
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.dispatch_key(key(Key::Char('g')));
    assert_eq!(
        m.leader_pending(),
        Some(KeyStroke::new(false, false, false, ChordCode::Char('g'))),
    );
}

// ── Per-agent spawn keys are catalog rows (#102 P2) ──────────────────

/// The agent id of the first `Spawn(Agent)` command in `cmds`.
fn first_spawned_agent(cmds: &[lazybox_ipc::Command]) -> Option<String> {
    use lazybox_ipc::{Command, TerminalKind};
    cmds.iter().find_map(|c| match c {
        Command::Spawn {
            kind: TerminalKind::Agent(a),
            ..
        } => Some(a.clone()),
        _ => None,
    })
}

/// `a c` on the selected workspace spawns the `claude` agent — the
/// generated `SpawnAgent` row under the `a` leader (#304), dispatched
/// by the Model, not a sidebar arm.
#[test]
fn spawn_agent_c_spawns_claude() {
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let pr_ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    let pr_key: SessionKey = (&pr_ws.key).into();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![pr_ws],
        terminals: vec![],
        projects: vec![],
    });
    assert!(m.__test_sidebar_mut().focus_workspace_key(&pr_key));
    while server.rx.try_recv().is_ok() {}
    m.dispatch_key(key(Key::Char('a')));
    assert!(
        m.leader_pending().is_some(),
        "`a` arms the agent leader (which-key popup)",
    );
    m.dispatch_key(key(Key::Char('c')));
    let cmds: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
    assert_eq!(first_spawned_agent(&cmds).as_deref(), Some("claude"));
}

/// `a x` spawns codex — the second generated agent row.
#[test]
fn spawn_agent_x_spawns_codex() {
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let pr_ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    let pr_key: SessionKey = (&pr_ws.key).into();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![pr_ws],
        terminals: vec![],
        projects: vec![],
    });
    assert!(m.__test_sidebar_mut().focus_workspace_key(&pr_key));
    while server.rx.try_recv().is_ok() {}
    m.dispatch_key(key(Key::Char('a')));
    m.dispatch_key(key(Key::Char('x')));
    let cmds: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
    assert_eq!(first_spawned_agent(&cmds).as_deref(), Some("codex"));
}

/// An agent row is remappable through `ui.action_keys` keyed by
/// `spawn_agent.<id>`: after remapping claude to `Ctrl-j`, the default
/// `a c` chord no longer spawns it and `Ctrl-j` does.
#[test]
fn spawn_agent_key_is_remappable() {
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("spawn_agent.claude".to_string(), "Ctrl-j".to_string());
    m.apply_action_key_overrides(overrides);
    let pr_ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    let pr_key: SessionKey = (&pr_ws.key).into();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![pr_ws],
        terminals: vec![],
        projects: vec![],
    });
    assert!(m.__test_sidebar_mut().focus_workspace_key(&pr_key));
    while server.rx.try_recv().is_ok() {}

    // `a c` is no longer claude's chord — nothing spawns.
    m.dispatch_key(key(Key::Char('a')));
    m.dispatch_key(key(Key::Char('c')));
    let after_c: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
    assert_eq!(first_spawned_agent(&after_c), None, "a c was remapped away");

    // The remapped chord spawns claude.
    m.dispatch_key(KeyEvent::new(Key::Char('j'), KeyModifiers::CONTROL));
    let after_remap: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
    assert_eq!(first_spawned_agent(&after_remap).as_deref(), Some("claude"));
}

// ── Long-snooze is a Confirm-guarded catalog row (#102 P3) ───────────

/// `x z` long-snooze is `Confirm`-guarded: it mounts the unified
/// ActionConfirm modal instead of the old sidebar two-press latch, and
/// only snoozes (~1 year) once the user confirms with `y`.
#[test]
fn long_snooze_confirms_then_snoozes_a_year_out() {
    use lazybox_ipc::Command;
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let pr_ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    let pr_key: SessionKey = (&pr_ws.key).into();
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![pr_ws],
        terminals: vec![],
        projects: vec![],
    });
    assert!(m.__test_sidebar_mut().focus_workspace_key(&pr_key));
    while server.rx.try_recv().is_ok() {}

    // The workspace-menu chord mounts the confirm — no snooze yet.
    m.dispatch_key(key(Key::Char('x')));
    m.dispatch_key(key(Key::Char('z')));
    assert_eq!(
        m.top_modal(),
        Some(&Id::ActionConfirm),
        "x z must mount the Confirm modal, not snooze immediately",
    );
    let before_confirm: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
    assert!(
        !before_confirm
            .iter()
            .any(|c| matches!(c, Command::Snooze { .. })),
        "no snooze before confirming",
    );

    // Confirming fires the ~1-year snooze.
    m.dispatch_modal_key(key(Key::Char('y')));
    let after: Vec<_> = std::iter::from_fn(|| server.rx.try_recv().ok()).collect();
    let snooze = after.iter().find_map(|c| match c {
        Command::Snooze { until, .. } => Some(*until),
        _ => None,
    });
    let until = snooze.expect("confirming x z snoozes");
    let days = (until - Utc::now()).num_days();
    assert!(
        (360..=370).contains(&days),
        "expected ~1 year, got {days} days"
    );
}

/// Flatten the current frame buffer into a newline-joined string of
/// cell symbols, so a render test can assert on what's actually on
/// screen.
fn render_to_string(m: &mut Model<tuirealm::terminal::TestTerminalAdapter>) -> String {
    use tuirealm::terminal::TerminalAdapter;
    m.view();
    let buf = m.terminal.raw().backend().buffer().clone();
    let area = buf.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Regression for #35: `g r` on a PR with no candidate reviewers must
/// surface a framed empty-state over the still-visible panes — NOT a
/// blank/black screen. Previously the empty case only fired a footer
/// flash, and an empty `Choice` (when it was mounted) rendered as a
/// full-height blank rectangle.
#[test]
fn gr_with_no_candidate_reviewers_shows_framed_empty_state() {
    let mut m = build_model();
    let ws = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });

    m.dispatch_key(key(Key::Char('g')));
    m.dispatch_key(key(Key::Char('r')));

    // A framed picker is mounted (not a bare flash).
    assert_eq!(
        m.top_modal(),
        Some(&Id::RequestReviewers),
        "empty-candidate g r must mount the reviewers picker, not bail with only a flash",
    );

    let screen = render_to_string(&mut m);
    // The empty-state explains itself inside its bordered box…
    assert!(
        screen.contains("No candidate reviewers yet"),
        "empty state must explain why there's nothing to pick:\n{screen}",
    );
    assert!(
        screen.contains("Add reviewers"),
        "the picker keeps its framed title:\n{screen}",
    );
    // …and the panes behind it are STILL drawn — the bug was a
    // black/blank screen, so the sidebar PR row + footer must survive.
    assert!(
        screen.contains("PR o/r#1"),
        "panes must remain visible behind the empty state:\n{screen}",
    );

    // Enter dismisses the empty picker (an empty `require_one` multi
    // would otherwise latch the "pick at least one" hint forever).
    m.dispatch_modal_key(key(Key::Enter));
    assert_eq!(
        m.top_modal(),
        None,
        "Enter on the empty picker must close it",
    );
}

// ───────────────────────────────────────────────────────────────────
// Right-pane chord shadowing regressions: with the activity pane
// focused, `G` / `z` / `m` used to resolve to the Workspace section's
// catalog entries (assignees picker / snooze picker / mark-ALL-read)
// before the pane's own bindings ever saw the key.
// ───────────────────────────────────────────────────────────────────

fn activity(author: &str, body: &str, age_minutes: i64) -> lazybox_core::Activity {
    lazybox_core::Activity {
        author: author.into(),
        body: body.into(),
        created_at: Utc::now() - chrono::Duration::minutes(age_minutes),
        kind: lazybox_core::ActivityKind::Comment,
        node_id: None,
        path: None,
        line: None,
        diff_hunk: None,
        thread_id: None,
    }
}

/// PR workspace with two unread activity rows and a GraphQL node id
/// (so a mis-dispatched AddAssignees would really mount its picker).
fn model_on_activity_pane() -> (
    lazybox_tui::realm::Model<tuirealm::terminal::TestTerminalAdapter>,
    lazybox_ipc::Connection,
) {
    let (client, server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let mut task = task_with_pr("o/r#1");
    task.node_id = Some("PR_node".into());
    task.recent_activity = vec![
        activity("alice", "newest comment", 1),
        activity("bob", "older comment", 5),
    ];
    let ws = Workspace::from_task(task, Utc::now());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });
    // Enter on the sidebar row moves focus to the activity pane.
    m.dispatch_key(key(Key::Enter));
    assert_eq!(m.focus(), PaneFocus::Right, "test setup: focus right pane");
    (m, server)
}

fn drain_cmds(server: &mut lazybox_ipc::Connection) -> Vec<lazybox_ipc::Command> {
    let mut commands = Vec::new();
    while let Ok(cmd) = server.rx.try_recv() {
        commands.push(cmd);
    }
    commands
}

#[test]
fn right_focus_shift_g_scrolls_to_bottom_not_assignees_picker() {
    let (mut m, _server) = model_on_activity_pane();
    assert_eq!(m.__test_right().comment_cursor(), 0);

    m.dispatch_key(key_with(Key::Char('G'), KeyModifiers::SHIFT));

    assert_eq!(
        m.top_modal(),
        None,
        "G on the activity pane must not open the assignees picker",
    );
    assert_eq!(
        m.__test_right().comment_cursor(),
        1,
        "G must jump the activity cursor to the last row",
    );
}

#[test]
fn right_focus_z_undoes_mark_read_not_snooze_picker() {
    use lazybox_ipc::Command;
    let (mut m, mut server) = model_on_activity_pane();

    // `m` marks the cursor row read (and records the undo target).
    m.dispatch_key(key(Key::Char('m')));
    let cmds = drain_cmds(&mut server);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::MarkActivityRead { index: 0, .. })),
        "test setup: per-row mark must fire first, got {cmds:#?}",
    );

    // `z` must undo it — not open the snooze picker.
    m.dispatch_key(key(Key::Char('z')));
    assert_eq!(
        m.top_modal(),
        None,
        "z on the activity pane must not open the snooze picker",
    );
    let cmds = drain_cmds(&mut server);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::UnmarkActivityRead { index: 0, .. })),
        "z must emit UnmarkActivityRead for the just-marked row, got {cmds:#?}",
    );
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::Snooze { .. } | Command::Unsnooze { .. })),
        "z must not touch snooze state from the activity pane",
    );
}

#[test]
fn right_focus_m_marks_cursor_row_not_whole_workspace() {
    use lazybox_ipc::Command;
    let (mut m, mut server) = model_on_activity_pane();

    m.dispatch_key(key(Key::Char('m')));

    let cmds = drain_cmds(&mut server);
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::MarkActivityRead { index: 0, .. })),
        "m with the cursor on an activity row marks THAT row, got {cmds:#?}",
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::MarkRead { .. })),
        "m on a focused row must not bulk-mark the workspace",
    );
}

#[test]
fn sidebar_focus_m_still_marks_whole_workspace() {
    use lazybox_ipc::Command;
    let (mut m, mut server) = model_on_activity_pane();
    // Return to the sidebar: workspace-wide semantics apply there.
    m.dispatch_key(key(Key::Tab)); // Right → Terminals (empty)
    m.dispatch_key(key(Key::Tab)); // Terminals → Sidebar
    assert_eq!(m.focus(), PaneFocus::Sidebar);
    drain_cmds(&mut server);

    m.dispatch_key(key(Key::Char('m')));

    let cmds = drain_cmds(&mut server);
    assert!(
        cmds.iter().any(|c| matches!(c, Command::MarkRead { .. })),
        "sidebar m keeps the workspace-wide mark, got {cmds:#?}",
    );
}

#[test]
fn j_and_k_navigate_the_sidebar() {
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let newer = Workspace::from_task(task_with_pr("o/r#1"), Utc::now());
    let mut older_task = task_with_pr("o/r#2");
    older_task.updated_at = Utc::now() - chrono::Duration::hours(2);
    let older = Workspace::from_task(older_task, Utc::now());
    let (newer_key, older_key) = (newer.key.clone(), older.key.clone());
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![newer, older],
        terminals: vec![],
        projects: vec![],
    });
    assert_eq!(
        m.__test_sidebar_mut().selected_workspace().map(|w| &w.key),
        Some(&newer_key),
    );

    m.dispatch_key(key(Key::Char('j')));
    assert_eq!(
        m.__test_sidebar_mut().selected_workspace().map(|w| &w.key),
        Some(&older_key),
        "j must move the sidebar cursor down (and not be eaten by the catalog)",
    );

    m.dispatch_key(key(Key::Char('k')));
    assert_eq!(
        m.__test_sidebar_mut().selected_workspace().map(|w| &w.key),
        Some(&newer_key),
        "k must move the sidebar cursor back up",
    );
}

#[test]
fn j_and_k_navigate_the_activity_feed() {
    let (mut m, _server) = model_on_activity_pane();
    assert_eq!(m.__test_right().comment_cursor(), 0);

    m.dispatch_key(key(Key::Char('j')));
    assert_eq!(m.__test_right().comment_cursor(), 1, "j moves down");

    m.dispatch_key(key(Key::Char('k')));
    assert_eq!(m.__test_right().comment_cursor(), 0, "k moves back up");
}

// ── Worktree-provisioning progress modal (#172) ────────────────────

#[test]
fn worktree_progress_event_mounts_the_checklist_modal() {
    use lazybox_ipc::{WorktreeStep, WorktreeStepStatus};
    let mut m = build_model();
    let sk = SessionKey::new("github:o/r#42");
    assert!(m.modal_stack.is_empty());
    m.handle_daemon_event(IpcEvent::WorktreeProgress {
        session_key: sk,
        step: WorktreeStep::Clone,
        status: WorktreeStepStatus::Started,
    });
    assert_eq!(
        m.modal_stack.last(),
        Some(&Id::WorktreeProgress),
        "first progress event mounts the checklist",
    );
}

#[test]
fn progress_events_remount_in_place_without_stacking() {
    use lazybox_ipc::{WorktreeStep, WorktreeStepStatus};
    let mut m = build_model();
    let sk = SessionKey::new("github:o/r#42");
    for step in [
        WorktreeStep::Clone,
        WorktreeStep::Fetch,
        WorktreeStep::WorktreeAdd,
    ] {
        m.handle_daemon_event(IpcEvent::WorktreeProgress {
            session_key: sk.clone(),
            step,
            status: WorktreeStepStatus::Started,
        });
    }
    m.handle_daemon_event(IpcEvent::WorktreeProgress {
        session_key: sk,
        step: WorktreeStep::Setup,
        status: WorktreeStepStatus::Started,
    });
    assert_eq!(
        m.modal_stack
            .iter()
            .filter(|id| *id == &Id::WorktreeProgress)
            .count(),
        1,
        "the checklist must advance in place, not pile duplicate modals",
    );
}

#[test]
fn terminal_spawned_mid_checklist_walks_every_step_before_dismissing() {
    use lazybox_ipc::{TerminalId, TerminalKind, WorktreeStep, WorktreeStepStatus};
    use lazybox_tui::realm::Msg;
    use lazybox_tui::realm::components::worktree_progress::MIN_STEP_DWELL;
    let mut m = build_model();
    let sk = SessionKey::new("github:o/r#42");
    // The provision has only just started (clone kicked off) when the
    // session goes live — `TerminalSpawned` arrives mid-checklist.
    m.handle_daemon_event(IpcEvent::WorktreeProgress {
        session_key: sk.clone(),
        step: WorktreeStep::Clone,
        status: WorktreeStepStatus::Started,
    });
    assert_eq!(m.modal_stack.last(), Some(&Id::WorktreeProgress));
    m.handle_daemon_event(IpcEvent::TerminalSpawned {
        model_label: None,
        terminal_id: TerminalId(1),
        session_key: sk,
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
    });
    assert!(
        m.modal_stack.contains(&Id::WorktreeProgress),
        "a mid-checklist spawn must hold the checklist, not flash it away",
    );
    // Ticks past each step's dwell walk the display through, and only
    // after every step has had its dwell does the modal tear down.
    // Bounded so a regression that never dismisses fails instead of
    // hanging. Count the ticks survived to prove the checklist actually
    // advanced step-by-step rather than dismissing early.
    let mut ticks_survived = 0;
    let mut torn_down = false;
    for _ in 0..(STEP_COUNT_FOR_TEST + 2) {
        std::thread::sleep(MIN_STEP_DWELL + std::time::Duration::from_millis(50));
        m.update(Msg::WorktreeProgressTick);
        if !m.modal_stack.contains(&Id::WorktreeProgress) {
            torn_down = true;
            break;
        }
        ticks_survived += 1;
    }
    assert!(
        torn_down,
        "the checklist must tear down once every step has had its dwell",
    );
    assert!(
        ticks_survived >= STEP_COUNT_FOR_TEST - 1,
        "expected the display to walk all {STEP_COUNT_FOR_TEST} rows \
         (one dwell each) before dismissing; only survived {ticks_survived} ticks",
    );
}

#[test]
fn snapshot_terminal_backstops_worktree_progress_dismissal() {
    use lazybox_ipc::{
        TerminalId, TerminalKind, TerminalSnapshot, WorktreeStep, WorktreeStepStatus,
    };
    use lazybox_tui::realm::Msg;
    use lazybox_tui::realm::components::worktree_progress::MIN_STEP_DWELL;
    let mut m = build_model();
    let sk = SessionKey::new("github:o/r#42");

    m.handle_daemon_event(IpcEvent::WorktreeProgress {
        session_key: sk.clone(),
        step: WorktreeStep::Clone,
        status: WorktreeStepStatus::Started,
    });
    assert_eq!(m.modal_stack.last(), Some(&Id::WorktreeProgress));

    // Simulate a lag/reconnect path where the terminal is visible in
    // the live snapshot, but the specific TerminalSpawned completion
    // event never reaches this client. The same snapshot also stands in
    // for the per-stage WorktreeProgress updates that would have walked
    // the checklist forward. A snapshot whose terminal proves the
    // session is live must not hang on "Cloning repository" forever
    // (#219/#221) — but per #253 it also must not vanish on the spot:
    // it *queues* a graceful dismiss so the remaining stages still walk
    // for their dwell before the modal closes.
    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![],
        terminals: vec![TerminalSnapshot {
            model_label: None,
            terminal_id: TerminalId(1),
            session_key: sk,
            kind: TerminalKind::Agent("claude".into()),
            replay: Vec::new(),
            last_seq: 0,
            no_permission: false,
            on_main: false,
            last_user_message: None,
        }],
        projects: vec![],
    });
    assert!(
        m.modal_stack.contains(&Id::WorktreeProgress),
        "the snapshot queues a graceful dismiss; the checklist stays up to walk its stages",
    );

    // Ticks past each step's dwell walk the display through, and only
    // after every step has been shown does the modal tear down. Bounded
    // so a regression that never dismisses fails instead of hanging.
    let mut ticks_survived = 0;
    let mut torn_down = false;
    for _ in 0..(STEP_COUNT_FOR_TEST + 2) {
        std::thread::sleep(MIN_STEP_DWELL + std::time::Duration::from_millis(50));
        m.update(Msg::WorktreeProgressTick);
        if !m.modal_stack.contains(&Id::WorktreeProgress) {
            torn_down = true;
            break;
        }
        ticks_survived += 1;
    }
    assert!(
        torn_down,
        "the queued checklist must tear down once every step has had its dwell",
    );
    assert!(
        ticks_survived >= STEP_COUNT_FOR_TEST - 1,
        "expected the display to walk the rows (one dwell each) before dismissing; \
         only survived {ticks_survived} ticks",
    );
}

/// Checklist row count (prepare, worktree-add, setup, agent), mirrored
/// here so the bounded walk above can't spin forever.
const STEP_COUNT_FOR_TEST: usize = 4;

#[test]
fn instant_resume_does_not_flash_the_progress_modal() {
    use lazybox_ipc::{TerminalId, TerminalKind};
    let mut m = build_model();
    // No WorktreeProgress events precede this spawn — the existing
    // worktree was reused, so the checklist must never appear.
    m.handle_daemon_event(IpcEvent::TerminalSpawned {
        model_label: None,
        terminal_id: TerminalId(1),
        session_key: SessionKey::new("github:o/r#42"),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
    });
    assert!(
        !m.modal_stack.contains(&Id::WorktreeProgress),
        "an instant resume provisions nothing and shows no modal",
    );
}

#[test]
fn failed_step_keeps_the_checklist_up_past_terminal_spawned() {
    use lazybox_ipc::{TerminalId, TerminalKind, WorktreeStep, WorktreeStepStatus};
    let mut m = build_model();
    let sk = SessionKey::new("github:o/r#42");
    m.handle_daemon_event(IpcEvent::WorktreeProgress {
        session_key: sk.clone(),
        step: WorktreeStep::Clone,
        status: WorktreeStepStatus::Failed("remote unreachable".into()),
    });
    // The daemon's empty-dir fallback still spawns a terminal.
    m.handle_daemon_event(IpcEvent::TerminalSpawned {
        model_label: None,
        terminal_id: TerminalId(1),
        session_key: sk,
        kind: TerminalKind::Shell,
        no_permission: false,
        on_main: false,
    });
    assert_eq!(
        m.modal_stack.last(),
        Some(&Id::WorktreeProgress),
        "a failed step holds the modal so the error is read, not silently dismissed",
    );
}
