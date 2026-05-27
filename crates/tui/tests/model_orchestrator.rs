//! Orchestrator-level tests for `realm::Model`. These replace the
//! deleted `app_loop.rs` tests that exercised the legacy `App` struct
//! end-to-end. Each test names the behaviour it covers — Tab cycle,
//! q-q quit latch, splitter resize, preselect, modal mount, etc.
//!
//! The tests use `Model::new_for_test` (a cfg(test)-only constructor
//! that swaps `CrosstermTerminalAdapter` for `TestTerminalAdapter`)
//! so they don't need a real terminal or raw mode.

use chrono::Utc;
use crossterm::event::{KeyModifiers as CtKeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use pilot_core::{SessionKey, Workspace, WorkspaceKey};
use pilot_ipc::{Event as IpcEvent, channel};
use pilot_tui::realm::Model;
use pilot_tui::realm::model::{Id, PaneFocus, Preselect};
use tuirealm::event::{Key, KeyEvent, KeyModifiers};
use tuirealm::ratatui::layout::{Rect, Size};

fn build_model() -> Model<tuirealm::terminal::TestTerminalAdapter> {
    let (client, _server) = channel::pair();
    Model::new_for_test(client, Size::new(120, 40)).expect("model init")
}

fn key(code: Key) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
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
    // Tab belongs to the shell — use `]]` to exit. The fixture
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
    let project = pilot_core::Project::new(
        pilot_core::ProjectKey::github("owner", "repo"),
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

#[test]
fn remapped_help_binding_mounts_help_modal() {
    // Remap help to lowercase `h` and verify the modal opens.
    let mut m = build_model();
    let mut overrides = std::collections::BTreeMap::new();
    overrides.insert("open_help".to_string(), "h".to_string());
    m.apply_action_key_overrides(overrides);
    m.dispatch_key(key(Key::Char('h')));
    assert_eq!(m.top_modal(), Some(&Id::Help));
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
fn question_mark_mounts_help_modal() {
    // `dispatch_key` bypasses the run-loop's "modal is up" guard
    // and drives `handle_pane_key` directly, so this test verifies
    // the orchestrator-side wiring rather than the run-loop guard.
    let mut m = build_model();
    m.dispatch_key(key(Key::Char('?')));
    assert_eq!(m.top_modal(), Some(&Id::Help));
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
    let workspace = Workspace::empty(WorkspaceKey(target_key.to_string()), "main", Utc::now());
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
fn out_of_scope_with_active_session_queues_a_prompt() {
    // Phase 2 of the rescope flow: when the daemon sends a
    // `WorkspaceOutOfScope` event (a workspace fell out of the
    // filter while having running terminals), the model must
    // mount a Confirm modal asking the user before killing.
    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let target = "github:owner/repo#42";
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: pilot_core::WorkspaceKey::new(target),
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
        workspace_key: pilot_core::WorkspaceKey::new("github:o/a#1"),
        label: "o/a#1".into(),
        title: None,
        active_terminal_count: 1,
    });
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: pilot_core::WorkspaceKey::new("github:o/b#2"),
        label: "o/b#2".into(),
        title: None,
        active_terminal_count: 2,
    });
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    // Press Esc to dismiss the first prompt (= "no, keep it").
    m.update(pilot_tui::realm::Msg::ModalDismissed);
    // Next prompt should be live now.
    assert_eq!(m.top_modal(), Some(&Id::RemoveOutOfScope));
    m.update(pilot_tui::realm::Msg::ModalDismissed);
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
    // Open Help. The Help modal subscribes to the bus the same way
    // any modal does; we don't drive its construction directly so
    // we just verify the queue behavior via the run-through.
    m.dispatch_key(key(Key::Char('?')));
    assert_eq!(m.top_modal(), Some(&Id::Help));
    // Daemon sends an out-of-scope event while Help is up.
    m.handle_daemon_event(IpcEvent::WorkspaceOutOfScope {
        workspace_key: pilot_core::WorkspaceKey::new("github:o/r#1"),
        label: "o/r#1".into(),
        title: None,
        active_terminal_count: 1,
    });
    // Help still on top — the queued prompt hasn't surfaced yet.
    assert_eq!(m.top_modal(), Some(&Id::Help));
    // Dismiss Help. Now the prompt should mount.
    m.update(pilot_tui::realm::Msg::ModalDismissed);
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
        issue_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-71"),
        pr_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-141"),
        issue_label: "o/r#71".into(),
        pr_label: "o/r#141".into(),
        active_terminal_count: 1,
    });
    assert_eq!(m.top_modal(), Some(&Id::MergeConfirm));
}

#[test]
fn merge_confirm_yes_sends_accept_command() {
    use pilot_ipc::Command;
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::WorkspaceMergePending {
        issue_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-71"),
        pr_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-141"),
        issue_label: "o/r#71".into(),
        pr_label: "o/r#141".into(),
        active_terminal_count: 1,
    });
    m.update(pilot_tui::realm::Msg::Confirmed(true));
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
fn merge_confirm_esc_sends_reject_command() {
    use pilot_ipc::Command;
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    m.handle_daemon_event(IpcEvent::WorkspaceMergePending {
        issue_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-71"),
        pr_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-141"),
        issue_label: "o/r#71".into(),
        pr_label: "o/r#141".into(),
        active_terminal_count: 1,
    });
    m.update(pilot_tui::realm::Msg::ModalDismissed);
    let cmd = server
        .rx
        .try_recv()
        .expect("ConfirmMerge command emitted on Esc");
    match cmd {
        Command::ConfirmMerge { accept, .. } => assert!(!accept),
        other => panic!("expected ConfirmMerge, got {other:?}"),
    }
}

/// GitHub issue (not PR) — `url` carries `/issues/<n>`, no branch.
fn task_with_issue(key: &str, title: &str, body: Option<&str>) -> pilot_core::Task {
    use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
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
        labels: vec![],
        reviewers: vec![],
        assignees: vec![],
        auto_merge_enabled: false,
        is_in_merge_queue: false,
        mergeable: pilot_core::Mergeable::Unknown,
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
    use pilot_ipc::{Command, TerminalId, TerminalKind};
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
        terminal_id: TerminalId(7),
        session_key: SessionKey::from(&ws_key),
        kind: TerminalKind::Agent("claude".into()),
    });

    // TerminalSpawned auto-focuses the terminal pane. In real usage
    // the user Tab's back to the sidebar; do the same here so `w`
    // hits the catalog instead of being written into the PTY.
    while m.focus() != PaneFocus::Sidebar {
        m.dispatch_key(key(Key::Tab));
    }

    // Press `w`.
    m.dispatch_key(key(Key::Char('w')));

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
    let (terminal_id, prompt, fallback) =
        inject.unwrap_or_else(|| panic!("w on issue with running claude must emit InjectPrompt — got: {commands:#?}"));
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
        .filter(|c| matches!(c, Command::Spawn { kind: TerminalKind::Agent(_), .. }))
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
    use pilot_ipc::{Command, TerminalId, TerminalKind};
    let (client, mut server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    let issue = task_with_issue("o/r#42", "Migrate to Postgres 16", None);
    let ws = Workspace::from_task(issue, Utc::now());
    let ws_key = ws.key.clone();

    m.handle_daemon_event(IpcEvent::Snapshot {
        workspaces: vec![ws],
        terminals: vec![],
        projects: vec![],
    });
    m.handle_daemon_event(IpcEvent::TerminalSpawned {
        terminal_id: TerminalId(7),
        session_key: SessionKey::from(&ws_key),
        kind: TerminalKind::Agent("claude".into()),
    });

    // Get to the right pane. TerminalSpawned auto-focused terminals;
    // Tab to sidebar, Tab again to right pane.
    while m.focus() != PaneFocus::Right {
        m.dispatch_key(key(Key::Tab));
    }

    m.dispatch_key(key(Key::Char('w')));

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

fn task_with_pr(key: &str) -> pilot_core::Task {
    use pilot_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState};
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
        labels: vec![],
        reviewers: vec![],
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
    // Shift-A should be a no-op (no sessions to adopt).
    m.dispatch_key(key_with(Key::Char('A'), KeyModifiers::SHIFT));
    assert_eq!(
        m.top_modal(),
        None,
        "Shift-A on a session-less workspace must not mount a picker",
    );
}

#[test]
fn shift_a_with_sessions_mounts_adopt_picker() {
    use chrono::Duration;
    use pilot_core::{SessionKind, WorkspaceSession};

    let (client, _server) = channel::pair();
    let mut m = Model::new_for_test(client, Size::new(120, 40)).unwrap();
    // Sidebar sorts by `updated_at` desc within a repo group, so we
    // bias `source` slightly newer than `target` to make the cursor
    // (which starts at row 0) land on the source — the workspace
    // Shift-A is supposed to read from.
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
    // Shift-A to fire. If this fails, the picker test diagnoses the
    // selection state rather than the keybinding wiring.
    let selected = m.sidebar().selected_workspace().cloned();
    assert!(
        selected.as_ref().is_some_and(|w| !w.sessions.is_empty()),
        "fixture: cursor must land on the source workspace; got {selected:?}"
    );
    m.dispatch_key(key_with(Key::Char('A'), KeyModifiers::SHIFT));
    assert_eq!(
        m.top_modal(),
        Some(&Id::AdoptTarget),
        "Shift-A on a workspace with sessions must mount the picker",
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
            issue_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-71"),
            pr_workspace_key: pilot_core::WorkspaceKey::new("github-o-r-141"),
            issue_label: "o/r#71".into(),
            pr_label: "o/r#141".into(),
            active_terminal_count: 1,
        });
    }
    // Dismiss the active prompt — the queue should be empty, not full
    // of duplicates of the same prompt.
    m.update(pilot_tui::realm::Msg::ModalDismissed);
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
    use pilot_core::{Activity, ActivityKind, Workspace};
    use pilot_ipc::Command;
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
