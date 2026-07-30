//! TerminalStack tests: event-driven state machine, tab management,
//! key → Write routing, ANSI strip, render.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_core::SessionKey;
use lazybox_ipc::{
    AgentState, Command, Event, PromptSource, TerminalId, TerminalKind, TerminalSnapshot,
    UserPrompt,
};

/// One-entry `Typed` prompt history from an optional last message, for
/// snapshot literals that used to carry a single `last_user_message`.
fn hist(text: Option<&str>) -> Vec<UserPrompt> {
    text.map(|t| UserPrompt {
        text: t.to_string(),
        timestamp_ms: 0,
        source: PromptSource::Typed,
    })
    .into_iter()
    .collect()
}
use lazybox_tui::components::TerminalStack;
use lazybox_tui::components::terminal_stack::{COMPOSING_CAP, RECENT_OUTPUT_CAP, strip_ansi};
use lazybox_tui::{PaneId, PaneOutcome};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::prelude::Rect;

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
fn code(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn sk(s: &str) -> SessionKey {
    s.into()
}

fn spawned(id: u64, session: &str, kind: TerminalKind) -> Event {
    Event::TerminalSpawned {
        model_label: None,
        terminal_id: TerminalId(id),
        session_key: sk(session),
        kind,
        no_permission: false,
        on_main: false,
    }
}

// ── Event-driven state ─────────────────────────────────────────────────

#[test]
fn spawn_event_creates_slot() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    assert_eq!(t.terminal_count(), 1);
}

#[test]
fn terminals_filtered_by_active_session() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#2", TerminalKind::Shell));

    t.set_active_session(Some(sk("o/r#1")));
    assert_eq!(t.visible_terminals().len(), 1);
    assert_eq!(t.visible_terminals()[0], TerminalId(1));

    t.set_active_session(Some(sk("o/r#2")));
    assert_eq!(t.visible_terminals().len(), 1);
    assert_eq!(t.visible_terminals()[0], TerminalId(2));

    t.set_active_session(None);
    assert!(t.visible_terminals().is_empty());
}

#[test]
fn output_event_appends_to_recent_buffer() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"hello world\n".to_vec(),
        first_seq: 1,
        seq: 1,
    });
    let content = t.active_content().unwrap();
    assert_eq!(content, b"hello world\n");
}

#[test]
fn output_for_unknown_terminal_is_dropped() {
    let mut t = TerminalStack::new(PaneId::new(1));
    // No spawn — output arrives for a terminal we don't know about.
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(999),
        bytes: b"nobody home".to_vec(),
        first_seq: 1,
        seq: 1,
    });
    assert_eq!(t.terminal_count(), 0);
}

#[test]
fn output_preserves_raw_escapes_for_inspection() {
    // active_content() is the raw recent-bytes buffer used for tests
    // and pattern detection — the libghostty-vt parser is what
    // turns these into a rendered cell grid at draw time.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let raw = b"\x1b[31mred\x1b[0m text".to_vec();
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: raw.clone(),
        first_seq: 1,
        seq: 1,
    });
    assert_eq!(t.active_content().unwrap(), raw.as_slice());
    // And strip_ansi still works as a standalone helper for callers
    // that want a clean preview without the libghostty machinery.
    assert_eq!(strip_ansi(t.active_content().unwrap()), b"red text");
}

#[test]
fn exit_event_closes_the_terminal_window() {
    // When the inner process exits (user types `exit`, ^D, etc.) the
    // terminal window goes away — same model as every other terminal
    // emulator. Keeping a "dead" tab around was confusing and made
    // the user manually clean up.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&Event::TerminalExited {
        terminal_id: TerminalId(1),
        exit_code: Some(0),
        last_output: None,
    });
    assert_eq!(t.terminal_count(), 0, "exit removes the slot");
}

#[test]
fn recent_buffer_is_capped() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let chunk = vec![b'A'; 4096];
    for seq in 1..=10 {
        t.on_event(&Event::TerminalOutput {
            terminal_id: TerminalId(1),
            bytes: chunk.clone(),
            first_seq: seq,
            seq,
        });
    }
    let content = t.active_content().unwrap();
    assert!(
        content.len() <= RECENT_OUTPUT_CAP,
        "recent {} must be capped at {}",
        content.len(),
        RECENT_OUTPUT_CAP
    );
    // Last bytes are preserved (tail semantics).
    assert!(content.iter().all(|b| *b == b'A'));
}

#[test]
fn workspace_removed_prunes_all_its_terminals() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(3, "o/r#2", TerminalKind::Shell));
    t.on_event(&Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(
        "o/r#1",
    )));
    assert_eq!(t.terminal_count(), 1, "only o/r#2's terminal remains");
}

#[test]
fn snapshot_replaces_all_terminals() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    // Snapshot arrives with different set — prior gets wiped.
    t.on_event(&Event::Snapshot {
        workspaces: vec![],
        terminals: vec![TerminalSnapshot {
            model_label: None,
            terminal_id: TerminalId(99),
            session_key: sk("o/r#3"),
            kind: TerminalKind::Agent("codex".into()),
            replay: b"\x1b[0mhi\n".to_vec(),
            last_seq: 42,
            replay_available: true,
            no_permission: false,
            on_main: false,
            prompt_history: Vec::new(),
            composing_buffer: None,
            agent_state: Some(AgentState::Working),
            authenticating: false,
        }],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });
    assert_eq!(t.terminal_count(), 1);
    t.set_active_session(Some(sk("o/r#3")));
    assert!(t.displays_agent_state(&sk("o/r#3"), AgentState::Working));
    // The recent buffer is post-feed bytes from the replay payload.
    // Snapshot replay goes into the libghostty parser (not into the
    // recent buffer), so the buffer is empty until live output starts.
    assert!(t.active_content().unwrap().is_empty());
}

// ── Tab navigation ─────────────────────────────────────────────────────

#[test]
fn tab_idx_starts_at_zero() {
    let t = TerminalStack::new(PaneId::new(1));
    assert_eq!(t.active_tab_idx(), 0);
}

#[test]
fn cycle_tab_forward_wraps() {
    let mut t = TerminalStack::new(PaneId::new(1));
    for i in 1..=3 {
        t.on_event(&spawned(i, "o/r#1", TerminalKind::Shell));
    }
    t.set_active_session(Some(sk("o/r#1")));
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 1);
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 2);
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 0, "wraps");
}

#[test]
fn cycle_tab_backward_wraps() {
    let mut t = TerminalStack::new(PaneId::new(1));
    for i in 1..=3 {
        t.on_event(&spawned(i, "o/r#1", TerminalKind::Shell));
    }
    t.set_active_session(Some(sk("o/r#1")));
    t.cycle_tab_backward();
    assert_eq!(t.active_tab_idx(), 2, "wraps to end");
}

#[test]
fn set_active_session_resets_tab_idx() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(3, "o/r#2", TerminalKind::Shell));

    t.set_active_session(Some(sk("o/r#1")));
    t.cycle_tab_forward();
    assert_eq!(t.active_tab_idx(), 1);

    t.set_active_session(Some(sk("o/r#2")));
    assert_eq!(t.active_tab_idx(), 0, "reset on session change");
}

#[test]
fn agents_order_before_shells_regardless_of_spawn_order() {
    let mut t = TerminalStack::new(PaneId::new(1));
    // Shell spawns first (lower id), agent second — agent must still
    // land on the far left.
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(3, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    assert_eq!(
        t.visible_terminals(),
        vec![TerminalId(2), TerminalId(1), TerminalId(3)],
        "agent first, then shells by id"
    );
}

#[test]
fn returning_to_session_restores_last_focused_pane() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(3, "o/r#2", TerminalKind::Shell));

    t.set_active_session(Some(sk("o/r#1")));
    // Focus the shell (tab 1) in the first workspace.
    t.set_active_tab(1);
    assert_eq!(t.active_terminal_id(), Some(TerminalId(2)));

    // Leave for another workspace, then come back.
    t.set_active_session(Some(sk("o/r#2")));
    t.set_active_session(Some(sk("o/r#1")));

    assert_eq!(
        t.active_terminal_id(),
        Some(TerminalId(2)),
        "focus returns to the shell, not the first pane"
    );
}

#[test]
fn restored_focus_tracks_terminal_not_index() {
    // The remembered pane is keyed by terminal id, so a reorder of the
    // visible set (here: an agent spawning to the left after we leave)
    // doesn't drag focus onto a different terminal.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(9, "o/r#2", TerminalKind::Shell));

    t.set_active_session(Some(sk("o/r#1")));
    t.set_active_tab(1); // focus shell id 2 (tab 1)
    assert_eq!(t.active_terminal_id(), Some(TerminalId(2)));

    t.set_active_session(Some(sk("o/r#2")));
    // A late-arriving agent jumps to the far left of o/r#1, shifting
    // every shell's index by one.
    t.on_event(&spawned(5, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    assert_eq!(
        t.active_terminal_id(),
        Some(TerminalId(2)),
        "still focused on shell id 2 despite the index shift"
    );
}

#[test]
fn removed_workspace_forgets_its_remembered_focus() {
    // Returning to a re-created workspace must not restore a pane from
    // a previous incarnation — even when a fresh terminal happens to
    // reuse the remembered id at a different position.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(3, "o/r#2", TerminalKind::Shell));

    t.set_active_session(Some(sk("o/r#1")));
    t.set_active_tab(1); // focus shell id 2 (tab 1)
    assert_eq!(t.active_terminal_id(), Some(TerminalId(2)));

    // Leave (records o/r#1 -> id 2), then the workspace is removed.
    t.set_active_session(Some(sk("o/r#2")));
    t.on_event(&Event::WorkspaceRemoved(lazybox_core::WorkspaceKey::new(
        "o/r#1",
    )));

    // A new o/r#1 appears; id 2 is reused, this time as the shell that
    // sorts to the right of the agent (tab 1). A stale memory would
    // drag focus onto it.
    t.on_event(&spawned(99, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    assert_eq!(t.active_tab_idx(), 0, "stale focus was forgotten");
    assert_eq!(
        t.active_terminal_id(),
        Some(TerminalId(99)),
        "fresh workspace lands on the first pane, not the reused id"
    );
}

// ── Key routing ────────────────────────────────────────────────────────

#[test]
fn char_key_emits_write_to_active_terminal() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(42, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    let outcome = t.handle_key(ch('a'), &mut cmds);
    assert_eq!(outcome, PaneOutcome::Consumed);
    assert_eq!(cmds.len(), 1);
    match &cmds[0] {
        Command::Write {
            terminal_id,
            bytes,
            intent,
        } => {
            assert_eq!(*terminal_id, TerminalId(42));
            assert_eq!(bytes, b"a");
            assert_eq!(*intent, lazybox_ipc::TerminalInputIntent::Compose);
        }
        other => panic!("expected Write, got {other:?}"),
    }
}

#[test]
fn enter_emits_cr_to_terminal() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    match &cmds[0] {
        Command::Write { bytes, intent, .. } => {
            assert_eq!(bytes, b"\r");
            assert_eq!(*intent, lazybox_ipc::TerminalInputIntent::Submit);
        }
        _ => panic!(),
    }
}

#[test]
fn shift_enter_emits_alt_enter() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    t.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &mut cmds,
    );
    match &cmds[0] {
        Command::Write { bytes, intent, .. } => {
            assert_eq!(bytes, b"\x1b\r");
            assert_eq!(*intent, lazybox_ipc::TerminalInputIntent::Compose);
        }
        _ => panic!(),
    }
}

#[test]
fn ctrl_letter_emits_control_byte() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    t.handle_key(ctrl('a'), &mut cmds);
    match &cmds[0] {
        Command::Write { bytes, .. } => assert_eq!(bytes, &[0x01]),
        _ => panic!(),
    }
}

#[test]
fn ctrl_bracket_flows_to_agent_too() {
    // The terminal escape moved from `Ctrl-]` to a configurable
    // typed sequence handled at the app dispatcher level (default
    // `]]`). The terminal stack itself no longer owns ANY escape
    // shortcut — every key flows to the agent.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(ctrl(']'), &mut cmds);
    assert_eq!(outcome, PaneOutcome::Consumed);
    // Ctrl-] encodes as 0x1d.
    assert!(matches!(
        cmds.first(),
        Some(Command::Write { bytes, .. }) if bytes == &[0x1du8]
    ));
}

#[test]
fn ctrl_o_flows_to_agent() {
    // The terminal stack has no escape shortcut at all — every
    // keystroke flows to the agent. Lazybox's escape latch (default
    // `]]`) lives at the app dispatcher level.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(ctrl('o'), &mut cmds);
    assert_eq!(outcome, PaneOutcome::Consumed);
    // Ctrl-O encodes as 0x0f.
    assert!(matches!(
        cmds.first(),
        Some(Command::Write { bytes, .. }) if bytes == &[0x0fu8]
    ));
}

#[test]
fn tab_flows_to_agent_for_autocomplete() {
    // Tab is essential inside a shell / Claude prompt for completion.
    // The terminal stack must NOT swallow it as a focus-cycle key —
    // that's a job for the app-level handler, gated on focus.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(code(KeyCode::Tab), &mut cmds);
    assert_eq!(outcome, PaneOutcome::Consumed);
    // Should produce a Write with a literal \t byte.
    assert!(matches!(
        cmds.first(),
        Some(Command::Write { bytes, .. }) if bytes == b"\t"
    ));
}

#[test]
fn keys_without_active_terminal_bubble_up() {
    let mut t = TerminalStack::new(PaneId::new(1));
    // No spawned terminals for the active session.
    t.set_active_session(Some(sk("o/r#1")));
    let mut cmds = Vec::new();
    let outcome = t.handle_key(ch('x'), &mut cmds);
    assert_eq!(outcome, PaneOutcome::Pass);
    assert!(cmds.is_empty());
}

// ── ANSI strip ─────────────────────────────────────────────────────────

#[test]
fn strip_ansi_removes_csi() {
    assert_eq!(strip_ansi(b"\x1b[31mred\x1b[0m"), b"red");
    assert_eq!(strip_ansi(b"\x1b[1;32;40mmulti\x1b[m"), b"multi");
}

#[test]
fn strip_ansi_removes_osc() {
    // OSC terminated by BEL.
    assert_eq!(strip_ansi(b"before\x1b]0;title\x07after"), b"beforeafter");
    // OSC terminated by ST (ESC \).
    assert_eq!(strip_ansi(b"x\x1b]0;title\x1b\\y"), b"xy");
}

#[test]
fn strip_ansi_drops_bell() {
    assert_eq!(strip_ansi(b"ding\x07dong"), b"dingdong");
}

#[test]
fn strip_ansi_preserves_newlines_and_utf8() {
    assert_eq!(strip_ansi(b"line1\nline2\r\n"), b"line1\nline2\r\n");
    // "é" in UTF-8 is C3 A9 — both bytes should survive.
    assert_eq!(strip_ansi("café".as_bytes()), "café".as_bytes());
}

#[test]
fn strip_ansi_handles_stray_esc_at_end() {
    // ESC at end of buffer — no panic.
    let input = b"text\x1b";
    let out = strip_ansi(input);
    // Either "text" or "text\x1b"; not crashing is the contract.
    assert!(out.starts_with(b"text"));
}

// ── Render ─────────────────────────────────────────────────────────────

fn render_to_string(t: &mut TerminalStack, w: u16, h: u16, focused: bool) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| t.render(Rect::new(0, 0, w, h), f, focused))
        .unwrap();
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn render_empty_shows_placeholder() {
    let mut t = TerminalStack::new(PaneId::new(1));
    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        out.contains("no terminals"),
        "empty state visible; got:\n{out}"
    );
}

#[test]
fn render_shows_tab_bar_and_content() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"first line\nsecond line\n".to_vec(),
        first_seq: 1,
        seq: 1,
    });

    let out = render_to_string(&mut t, 60, 10, true);
    assert!(out.contains("claude"), "first tab label; got:\n{out}");
    assert!(out.contains("shell"), "second tab label; got:\n{out}");
    assert!(
        out.contains("first line") && out.contains("second line"),
        "active terminal content; got:\n{out}"
    );
}

#[test]
fn render_shows_scrollbar_when_terminal_has_scrollback() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    // Size the grid before feeding output so lines land at the
    // rendered dimensions.
    render_to_string(&mut t, 60, 10, true);
    let mut bytes = Vec::new();
    for i in 0..100 {
        bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes,
        first_seq: 1,
        seq: 1,
    });
    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        out.contains('█'),
        "terminal with scrollback shows a scrollbar thumb; got:\n{out}"
    );
}

#[test]
fn render_hides_scrollbar_when_terminal_fits() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    render_to_string(&mut t, 60, 10, true);
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"just one line\r\n".to_vec(),
        first_seq: 1,
        seq: 1,
    });
    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        !out.contains('█'),
        "scrollbar auto-hides when content fits; got:\n{out}"
    );
}

/// Keyboard scroll fallback over PRIMARY-screen scrollback (the
/// native-scrollback tmux mode keeps relayed output there):
/// Shift-PageUp moves the viewport up locally, Shift-End snaps back to
/// the live tail, and neither writes a byte to the PTY.
#[test]
fn shift_pageup_scrolls_local_scrollback_without_pty_writes() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    render_to_string(&mut t, 60, 10, true);
    let mut bytes = Vec::new();
    for i in 0..100 {
        bytes.extend_from_slice(format!("line {i}\r\n").as_bytes());
    }
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes,
        first_seq: 1,
        seq: 1,
    });
    let at_bottom = t.scrollbar_summary().expect("scrollbar state");
    assert!(
        at_bottom.contains("screen=Some(Primary)"),
        "plain output stays on the primary screen: {at_bottom}"
    );

    let mut cmds = Vec::new();
    let _ = t.handle_key(
        KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT),
        &mut cmds,
    );
    // The first upward scroll may ship a deep-scrollback fetch (#393),
    // but never a PTY write — that's what would leak scroll keys into
    // the inner program.
    assert!(
        !cmds.iter().any(|c| matches!(c, Command::Write { .. })),
        "scroll keys never reach the PTY: {cmds:?}"
    );
    let scrolled = t.scrollbar_summary().expect("scrollbar state");
    assert_ne!(
        scrolled, at_bottom,
        "Shift-PageUp must move the viewport into scrollback"
    );

    cmds.clear();
    let _ = t.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT), &mut cmds);
    assert!(cmds.is_empty(), "scroll keys never reach the PTY: {cmds:?}");
    assert_eq!(
        t.scrollbar_summary().expect("scrollbar state"),
        at_bottom,
        "Shift-End returns to the live tail"
    );
}

#[test]
fn render_shows_no_perms_badge_for_autonomous_session() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&Event::TerminalSpawned {
        model_label: None,
        terminal_id: TerminalId(1),
        session_key: sk("o/r#1"),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: true,
        on_main: false,
    });
    t.set_active_session(Some(sk("o/r#1")));
    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        out.contains("no-perms"),
        "autonomous session must show the no-permission badge; got:\n{out}"
    );
}

#[test]
fn render_omits_no_perms_badge_for_interactive_session() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));
    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        !out.contains("no-perms"),
        "interactive session must not show the no-permission badge; got:\n{out}"
    );
}

#[test]
fn render_tab_bar_updates_after_cycle() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"AGENT_OUTPUT".to_vec(),
        first_seq: 1,
        seq: 1,
    });
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(2),
        bytes: b"SHELL_OUTPUT".to_vec(),
        first_seq: 1,
        seq: 1,
    });

    let out_before = render_to_string(&mut t, 60, 10, true);
    assert!(out_before.contains("AGENT_OUTPUT"));
    assert!(!out_before.contains("SHELL_OUTPUT"));

    t.cycle_tab_forward();
    let out_after = render_to_string(&mut t, 60, 10, true);
    assert!(out_after.contains("SHELL_OUTPUT"));
    assert!(!out_after.contains("AGENT_OUTPUT"));
}

// ── Singleton lookup + focus ─────────────────────────────────────────
//
// The "one Claude per session" invariant lives at the App layer (it
// intercepts duplicate spawns and routes them to focus_terminal).
// These tests cover the primitives the App leans on.

#[test]
fn find_runner_returns_existing_singleton_in_same_session() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    let found = t.find_runner(&sk("o/r#1"), &TerminalKind::Agent("claude".into()));
    assert_eq!(found, Some(TerminalId(1)));
}

#[test]
fn find_runner_distinguishes_agents_by_id() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let claude = t.find_runner(&sk("o/r#1"), &TerminalKind::Agent("claude".into()));
    let codex = t.find_runner(&sk("o/r#1"), &TerminalKind::Agent("codex".into()));
    assert_eq!(claude, Some(TerminalId(1)));
    assert_eq!(codex, None, "codex isn't claude");
}

#[test]
fn find_runner_returns_none_for_shell() {
    // Shells are explicitly multi: every `s` press spawns a fresh
    // one, no singleton lookup ever returns an existing slot.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    assert!(t.find_runner(&sk("o/r#1"), &TerminalKind::Shell).is_none());
}

#[test]
fn find_runner_scopes_to_session() {
    // Claude in session A is invisible to a lookup in session B —
    // sessions are independent worktrees, so the singleton constraint
    // doesn't cross sessions.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    assert_eq!(
        t.find_runner(&sk("o/r#2"), &TerminalKind::Agent("claude".into())),
        None,
        "claude in #1 doesn't satisfy a #2 lookup"
    );
}

#[test]
fn focus_terminal_activates_target_tab_and_expands() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Agent("codex".into())));
    t.set_active_session(Some(sk("o/r#1")));
    // Now collapse and focus the second tab.
    t.set_collapsed(true);
    assert!(t.is_collapsed());

    let switched = t.focus_terminal(TerminalId(2));
    assert!(switched);
    assert_eq!(t.active_terminal_id(), Some(TerminalId(2)));
    assert!(!t.is_collapsed(), "focusing a tab expands the section");
}

#[test]
fn focus_terminal_returns_false_for_invisible_target() {
    // Target belongs to a different session → not in `visible_terminals`
    // → focus_terminal can't switch to it.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#2", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    assert!(!t.focus_terminal(TerminalId(2)));
    assert_eq!(t.active_terminal_id(), Some(TerminalId(1)));
}

// ── Tile-manager wiring ───────────────────────────────────────────────
//
// The renderer + the `]]` leader's tile commands drive the
// SessionLayout state (the Model resolves `]]|` / `]]<arrow>` / `]]x`
// and calls these entry points, #286). These tests cover the
// state-machine path: splitting, focus moves, close. Render-shape
// tests live alongside (visual checks require a TestBackend which we
// already use elsewhere).

use lazybox_core::{SessionLayout, TileTree};
use lazybox_tui::components::terminal_stack::PendingSplit;

fn ws_key(s: &str) -> SessionKey {
    s.into()
}

#[test]
fn split_tile_emits_shell_spawn() {
    // `]]|` should arm a pending vertical split and emit a Shell
    // spawn. The new terminal's id arrives later via TerminalSpawned
    // and triggers `commit_pending_split`.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));

    let mut cmds = Vec::new();
    t.split_tile(PendingSplit::Vertical, &mut cmds);

    assert!(
        cmds.iter().any(|c| matches!(
            c,
            Command::Spawn {
                kind: TerminalKind::Shell,
                ..
            }
        )),
        "split commits a Shell spawn so the new tile has a runner"
    );
}

#[test]
fn terminal_spawned_after_split_promotes_to_splits_layout() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_layout(SessionLayout::Tabs { active: 0 });

    let mut cmds = Vec::new();
    t.split_tile(PendingSplit::Vertical, &mut cmds);
    // Stage 2: daemon would respond with TerminalSpawned for the new shell.
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));

    match t.layout() {
        SessionLayout::Splits { tree, focused } => {
            // Tree is HSplit(Leaf(1), Leaf(2)) — old leaf on the
            // left, new shell on the right.
            assert_eq!(tree.leaves(), vec![1, 2]);
            // Focus lands on the new leaf so the user types into the
            // freshly-spawned shell.
            assert_eq!(focused, &vec![1u8]);
        }
        SessionLayout::Tabs { .. } => panic!("expected Splits after split, got Tabs"),
    }
}

#[test]
fn move_tile_focus_right_moves_focus_right() {
    // Pre-build a 2-leaf HSplit, focus on the left. Tile focus
    // moves use arrow keys after the `]]` leader (used to be
    // `h/j/k/l` vim-style — replaced for consistency with the
    // no-vim-mode rule the rest of the app follows).
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_layout(SessionLayout::Splits {
        tree: TileTree::HSplit {
            left: Box::new(TileTree::Leaf { terminal_id: 1 }),
            right: Box::new(TileTree::Leaf { terminal_id: 2 }),
            ratio: 50,
        },
        focused: vec![0],
    });
    let mut cmds = Vec::new();
    t.move_tile_focus(lazybox_core::TileDirection::Right, &mut cmds);
    if let SessionLayout::Splits { focused, .. } = t.layout() {
        assert_eq!(focused, &vec![1u8], "`]]→` moved focus to the right tile");
    }
    // Persist via SetSessionLayout.
    assert!(
        cmds.iter()
            .any(|c| matches!(c, Command::SetSessionLayout { .. })),
        "focus moves persist"
    );
}

#[test]
fn close_focused_tile_closes_and_collapses() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_layout(SessionLayout::Splits {
        tree: TileTree::HSplit {
            left: Box::new(TileTree::Leaf { terminal_id: 1 }),
            right: Box::new(TileTree::Leaf { terminal_id: 2 }),
            ratio: 50,
        },
        focused: vec![1],
    });
    let mut cmds = Vec::new();
    t.close_focused_tile(&mut cmds);
    // Layout collapsed back to Tabs since only one leaf remained.
    assert!(
        matches!(t.layout(), SessionLayout::Tabs { .. }),
        "single-leaf collapse downgrades to Tabs"
    );
    // Daemon-side close emitted for the killed tile.
    assert!(
        cmds.iter().any(|c| matches!(c, Command::Close { .. })),
        "close kills the runner's PTY"
    );
}

#[test]
fn pane_divider_is_accent_only_while_the_pane_has_focus() {
    // #286: the rule under the tab strip doubles as the pane's focus
    // ring — accent while the terminal pane has focus, chrome when
    // another pane does.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    let theme = lazybox_tui::theme::current();

    for (focused, want) in [(true, theme.accent), (false, theme.chrome)] {
        let backend = TestBackend::new(60, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| t.render(Rect::new(0, 0, 60, 12), f, focused))
            .unwrap();
        // The divider sits on row 1, inset one column.
        let cell = &term.backend().buffer()[(5u16, 1u16)];
        assert_eq!(cell.symbol(), "─", "divider row must hold the rule");
        assert_eq!(
            cell.style().fg,
            Some(want),
            "divider color must track focus (focused={focused})"
        );
    }
}

#[test]
fn split_tiles_paint_focus_contrast_bars() {
    // #286: in a split, EVERY tile gets a top rule — accent on the
    // focused tile, chrome on the rest — so "where does my typing
    // land" is legible at a glance.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    t.set_layout(SessionLayout::Splits {
        tree: TileTree::HSplit {
            left: Box::new(TileTree::Leaf { terminal_id: 1 }),
            right: Box::new(TileTree::Leaf { terminal_id: 2 }),
            ratio: 50,
        },
        focused: vec![1],
    });

    let (w, h) = (60u16, 12u16);
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| t.render(Rect::new(0, 0, w, h), f, true))
        .unwrap();
    let buf = term.backend().buffer();
    let theme = lazybox_tui::theme::current();

    // Tile bodies start at row 3 (title / divider / blank). The body
    // spans x 1..59; a 50% HSplit puts the left tile around x=2 and
    // the right tile past the divider column.
    let left = &buf[(2u16, 3u16)];
    let right = &buf[(w - 4, 3u16)];
    assert_eq!(left.symbol(), "─", "unfocused tile still gets a rule");
    assert_eq!(
        left.style().fg,
        Some(theme.chrome),
        "unfocused tile's rule is chrome"
    );
    assert_eq!(
        right.style().fg,
        Some(theme.accent),
        "focused tile's rule is accent"
    );
}

#[test]
fn tile_rule_is_carved_above_the_recap_not_over_it() {
    // #286 follow-up: the tile rule must not overdraw content — an
    // agent tile in a split keeps its pinned "you ▸ …" recap visible
    // on the row BELOW the rule.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    // Submit the prompt while the agent is the only (focused)
    // terminal, THEN add the shell — spawning first would steal focus
    // and the recap would record against the wrong terminal.
    type_str(&mut t, "fix the bug");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Shell));
    // Focus the SHELL tile so the agent tile is the unfocused one —
    // the case the old overdraw hid entirely.
    t.set_layout(SessionLayout::Splits {
        tree: TileTree::VSplit {
            top: Box::new(TileTree::Leaf { terminal_id: 1 }),
            bottom: Box::new(TileTree::Leaf { terminal_id: 2 }),
            ratio: 50,
        },
        focused: vec![1],
    });

    let out = render_to_string(&mut t, 60, 20, true);
    assert!(
        out.contains("you ▸ fix the bug"),
        "recap must stay visible under the tile rule; got:\n{out}"
    );
}

#[test]
fn close_focused_tile_in_tabs_closes_the_active_terminal() {
    // #286 follow-up: `]]x` must not be a silent no-op in Tabs mode —
    // it closes the active tab's terminal.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_layout(SessionLayout::Tabs { active: 0 });

    let mut cmds = Vec::new();
    t.close_focused_tile(&mut cmds);
    assert!(
        cmds.iter().any(
            |c| matches!(c, Command::Close { terminal_id, .. } if *terminal_id == TerminalId(1))
        ),
        "Tabs-mode close targets the active terminal, got {cmds:?}"
    );
}

#[test]
fn tile_command_keys_without_the_leader_reach_the_pty() {
    // With no armed leader, the erstwhile tile keys (`x`, `|`, Ctrl-w)
    // are ordinary input and must route to the PTY.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.set_active_session(Some(ws_key("o/r#1")));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));

    let mut cmds = Vec::new();
    t.handle_key(ch('x'), &mut cmds);
    t.handle_key(ch('|'), &mut cmds);
    t.handle_key(ctrl('w'), &mut cmds);
    let writes = cmds
        .iter()
        .filter(|c| matches!(c, Command::Write { .. }))
        .count();
    assert_eq!(writes, 3, "untouched keys go to the active terminal");
}

// ── Pinned "you ▸ …" recap ─────────────────────────────────────────────
//
// The recap header shows the latest message the user submitted to an
// agent, pinned above the agent's terminal grid. Tracking lives at the
// byte level — we mirror the exact bytes written to the PTY into our
// own buffer, commit on CR, and render that as a one-line summary
// above the agent. Parsing the bytes (rather than the KeyEvent) keeps
// the recap in lock-step with what the agent receives, including
// one-shot writes like snippet expansion.

fn type_str(t: &mut TerminalStack, s: &str) {
    let mut cmds = Vec::new();
    for c in s.chars() {
        t.handle_key(ch(c), &mut cmds);
    }
}

#[test]
fn enter_commits_typed_text_as_last_user_message() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "fix the bug");
    // Not yet committed — Enter is what marks the message as sent.
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);

    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("fix the bug"));
    // Composing buffer is wiped so the next message starts fresh.
    assert_eq!(t.composing_of(TerminalId(1)), Some(""));
}

#[test]
fn enter_emits_record_user_message_command() {
    // The recap lives only in the client slot, so on commit the client
    // must ship the message to the daemon for persistence — otherwise it
    // can't be restored after a restart (issue #105). Plain editing keys
    // emit only a Write; the submit additionally emits RecordUserMessage.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    for c in "ship it".chars() {
        t.handle_key(ch(c), &mut cmds);
    }
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::RecordUserMessage { .. })),
        "no persistence before submit; got: {cmds:?}"
    );

    cmds.clear();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    let recorded = cmds.iter().find_map(|c| match c {
        Command::RecordUserMessage {
            terminal_id,
            prompt,
        } => Some((*terminal_id, prompt.text.clone(), prompt.source.clone())),
        _ => None,
    });
    assert_eq!(
        recorded,
        Some((TerminalId(1), "ship it".to_string(), PromptSource::Typed)),
    );
}

#[test]
fn shell_submit_does_not_emit_record_user_message() {
    // Shells have no single "user prompt"; the recap is Agent-only, so a
    // commit on a shell must not persist anything.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    for c in "ls -la".chars() {
        t.handle_key(ch(c), &mut cmds);
    }
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert!(
        !cmds
            .iter()
            .any(|c| matches!(c, Command::RecordUserMessage { .. })),
        "shells never persist a recap; got: {cmds:?}"
    );
}

#[test]
fn snapshot_restores_recap_for_agent_terminal() {
    // Issue #105: the recap is client-side-only state, so on restart it
    // must be seeded from the daemon-persisted value carried on the
    // Snapshot — the replay ring only holds PTY output, never the input
    // the recap is composed from.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&Event::Snapshot {
        workspaces: vec![],
        terminals: vec![TerminalSnapshot {
            model_label: None,
            terminal_id: TerminalId(7),
            session_key: sk("o/r#1"),
            kind: TerminalKind::Agent("claude".into()),
            replay: b"reconnected\n".to_vec(),
            last_seq: 3,
            replay_available: true,
            no_permission: false,
            on_main: false,
            prompt_history: hist(Some("rebase onto main")),
            composing_buffer: None,
            agent_state: None,
            authenticating: false,
        }],
        projects: vec![],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
    });
    t.set_active_session(Some(sk("o/r#1")));

    assert_eq!(
        t.last_user_message_of(TerminalId(7)),
        Some("rebase onto main"),
    );

    // And it renders the pinned recap immediately, before any new input.
    let out = render_to_string(&mut t, 60, 12, true);
    assert!(
        out.contains("you ▸ rebase onto main"),
        "restored recap should render; got:\n{out}"
    );
}

#[test]
fn second_enter_replaces_the_recap() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    type_str(&mut t, "first");
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    type_str(&mut t, "second");
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("second"));
}

#[test]
fn empty_enter_does_not_overwrite_previous_recap() {
    // Pressing Enter on an empty buffer (just a CR with nothing
    // typed) is meaningless as "the latest message" — the previous
    // recap should stay. Avoids the pin going blank every time the
    // user mashes Enter to dismiss a Claude approval prompt.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    type_str(&mut t, "look at this");
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("look at this"));
}

#[test]
fn shift_enter_appends_newline_and_does_not_commit() {
    // Claude binds Shift-Enter to "newline in the prompt without
    // submit". The composing buffer mirrors that: the line keeps
    // building until a plain Enter arrives.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    type_str(&mut t, "line 1");
    t.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &mut cmds,
    );
    type_str(&mut t, "line 2");
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);
    assert_eq!(t.composing_of(TerminalId(1)), Some("line 1\nline 2"));

    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(
        t.last_user_message_of(TerminalId(1)),
        Some("line 1\nline 2")
    );
}

#[test]
fn backspace_pops_from_composing() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "hello");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Backspace), &mut cmds);
    t.handle_key(code(KeyCode::Backspace), &mut cmds);
    assert_eq!(t.composing_of(TerminalId(1)), Some("hel"));
}

#[test]
fn ctrl_c_clears_composing_without_commit() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "draft");
    let mut cmds = Vec::new();
    t.handle_key(ctrl('c'), &mut cmds);
    assert_eq!(t.composing_of(TerminalId(1)), Some(""));
    // No prior submit → still None, not a phantom committed "draft".
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);
}

#[test]
fn esc_clears_composing_without_commit() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "abandon");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Esc), &mut cmds);
    assert_eq!(t.composing_of(TerminalId(1)), Some(""));
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);
}

#[test]
fn shell_terminals_are_not_tracked() {
    // Shells don't have a single semantic "user prompt" the way an
    // agent does — every `cd`, `ls`, `grep` would otherwise commit
    // as the latest recap. Skip them entirely.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "ls");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);
    assert_eq!(t.composing_of(TerminalId(1)), Some(""));
}

#[test]
fn recap_is_per_agent_terminal() {
    // Two agents in the same session each track their own message.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.on_event(&spawned(2, "o/r#1", TerminalKind::Agent("codex".into())));
    t.set_active_session(Some(sk("o/r#1")));

    // Submit a message into the Claude tab (active by default — idx 0).
    let mut cmds = Vec::new();
    type_str(&mut t, "ask claude");
    t.handle_key(code(KeyCode::Enter), &mut cmds);

    // Switch to codex and submit a different message.
    t.cycle_tab_forward();
    type_str(&mut t, "ask codex");
    t.handle_key(code(KeyCode::Enter), &mut cmds);

    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("ask claude"));
    assert_eq!(t.last_user_message_of(TerminalId(2)), Some("ask codex"));
}

#[test]
fn record_paste_appends_to_focused_agent_composing() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "review ");
    t.record_paste("this snippet");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(
        t.last_user_message_of(TerminalId(1)),
        Some("review this snippet")
    );
}

#[test]
fn record_paste_truncates_at_composing_cap() {
    // Pathological paste — way larger than the cap. The buffer
    // should clamp to COMPOSING_CAP rather than grow unbounded.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let blob = "a".repeat(COMPOSING_CAP * 2);
    t.record_paste(&blob);
    assert_eq!(
        t.composing_of(TerminalId(1)).map(|s| s.len()),
        Some(COMPOSING_CAP),
        "paste clamped to cap"
    );
}

#[test]
fn record_paste_keeps_utf8_char_boundaries_on_clamp() {
    // The clamp must land on a UTF-8 char boundary — splitting a
    // multi-byte codepoint would produce an invalid String. Build a
    // buffer that already sits one byte short of the cap, then paste
    // a 4-byte emoji that would straddle the boundary.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let near_full = "x".repeat(COMPOSING_CAP - 1);
    t.record_paste(&near_full);
    t.record_paste("🚀");
    let composing = t.composing_of(TerminalId(1)).expect("agent slot");
    assert!(
        composing.is_char_boundary(composing.len()),
        "result is a valid UTF-8 string"
    );
    assert!(composing.len() <= COMPOSING_CAP);
}

#[test]
fn record_paste_is_noop_on_shell() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    t.record_paste("should be ignored");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);
}

#[test]
fn record_pty_write_updates_recap_for_one_shot_commands() {
    // Snippet expansion (and other programmatic sends) write a full
    // command + a trailing CR straight to the PTY, bypassing the
    // key-by-key path. The recap must still refresh to that command —
    // this is the desync the issue (#68) is about.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "typed message");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("typed message"));

    // A snippet fires straight at the PTY: body + `\r`.
    t.record_pty_write(TerminalId(1), b"run the tests\r", PromptSource::Typed);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("run the tests"));
    assert_eq!(t.composing_of(TerminalId(1)), Some(""));
}

#[test]
fn prompt_history_accumulates_all_submits_newest_first_with_source() {
    // Issue #523: every submit is retained (not just the latest), and a
    // snippet-sourced send is tagged with the snippet key so the `]]h`
    // history can mark it.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    type_str(&mut t, "first");
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    // A snippet-sourced one-shot send carries a Snippet source.
    let returned = t.record_pty_write(
        TerminalId(1),
        b"review the diff\r",
        PromptSource::Snippet {
            key: "rev".into(),
            category: "Review".into(),
        },
    );
    assert_eq!(
        returned.as_ref().map(|p| p.text.as_str()),
        Some("review the diff")
    );

    // Recap still shows the latest.
    assert_eq!(
        t.last_user_message_of(TerminalId(1)),
        Some("review the diff")
    );

    // The full history is browsable, newest-first, with the snippet tag.
    let (_, history) = t.focused_prompt_history().expect("agent has history");
    let rows: Vec<(&str, &PromptSource)> = history
        .iter()
        .map(|p| (p.text.as_str(), &p.source))
        .collect();
    assert_eq!(rows[0].0, "review the diff");
    assert!(matches!(
        rows[0].1,
        PromptSource::Snippet { key, category } if key == "rev" && category == "Review"
    ));
    assert_eq!(rows[1], ("first", &PromptSource::Typed));
}

#[test]
fn record_pty_write_treats_embedded_newlines_as_soft() {
    // A multi-line snippet body arrives as `line1\nline2\r`; only the
    // trailing CR submits, so the whole body commits once rather than
    // committing the first line and dropping the rest.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    t.record_pty_write(TerminalId(1), b"line1\nline2\r", PromptSource::Typed);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("line1\nline2"));
}

#[test]
fn record_pty_write_skips_escape_sequences() {
    // Arrow keys / mouse reports reach the PTY as CSI sequences. They
    // must be skipped, not appended as literal `[D` garbage into the
    // composing buffer.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    // "ab", left-arrow (ESC [ D), "c", then submit.
    t.record_pty_write(TerminalId(1), b"ab\x1b[Dc\r", PromptSource::Typed);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("abc"));
}

#[test]
fn record_pty_write_unknown_meta_escape_does_not_wipe_buffer() {
    // A stray `ESC`-prefixed sequence that isn't CSI/SS3 (e.g. an
    // Alt-combo) must drop only the ESC and keep parsing — it must
    // never silently clear an in-flight prompt. Only a *lone* ESC
    // (the real Esc key) resets the line.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    // "a", ESC+x (meta), "b", submit → the ESC is dropped, the rest
    // composes normally.
    t.record_pty_write(TerminalId(1), b"a\x1bxb\r", PromptSource::Typed);
    assert_eq!(t.last_user_message_of(TerminalId(1)), Some("axb"));
}

#[test]
fn ctrl_u_clears_composing_without_commit() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "scratch");
    let mut cmds = Vec::new();
    t.handle_key(ctrl('u'), &mut cmds);
    assert_eq!(t.composing_of(TerminalId(1)), Some(""));
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);
}

#[test]
fn record_pty_write_is_noop_on_shell() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    t.record_pty_write(TerminalId(1), b"ls -la\r", PromptSource::Typed);
    assert_eq!(t.last_user_message_of(TerminalId(1)), None);
}

#[test]
fn render_pins_recap_above_agent_grid() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    type_str(&mut t, "fix the bug");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);

    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        out.contains("you ▸ fix the bug"),
        "recap visible; got:\n{out}"
    );
}

#[test]
fn render_does_not_pin_recap_when_no_message_yet() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        !out.contains("you ▸"),
        "no recap before first submit; got:\n{out}"
    );
}

#[test]
fn render_does_not_pin_recap_on_shell() {
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Shell));
    t.set_active_session(Some(sk("o/r#1")));

    // Even if we (hypothetically) had a message buffered, shells
    // should never render the recap — the field is `None` for
    // shells by construction.
    type_str(&mut t, "ls");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    let out = render_to_string(&mut t, 60, 10, true);
    assert!(!out.contains("you ▸"), "no recap on shell; got:\n{out}");
}

#[test]
fn render_updates_recap_to_latest_message() {
    // Acceptance criterion: "Pin updates on every new user message sent."
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    type_str(&mut t, "first ask");
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    let out_a = render_to_string(&mut t, 60, 10, true);
    assert!(out_a.contains("you ▸ first ask"), "got:\n{out_a}");

    type_str(&mut t, "second ask");
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    let out_b = render_to_string(&mut t, 60, 10, true);
    assert!(out_b.contains("you ▸ second ask"), "got:\n{out_b}");
    assert!(!out_b.contains("first ask"), "stale recap; got:\n{out_b}");
}

#[test]
fn render_truncates_long_recap_with_ellipsis() {
    // Acceptance criterion: "Long messages are summarized / truncated
    // cleanly." Render into a narrow pane and check we get a `…`.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    type_str(
        &mut t,
        "this is a very long prompt that will not fit inside a narrow pane",
    );
    t.handle_key(code(KeyCode::Enter), &mut cmds);
    let out = render_to_string(&mut t, 30, 10, true);
    assert!(
        out.contains("…"),
        "ellipsis present on overflow; got:\n{out}"
    );
}

#[test]
fn render_summary_collapses_internal_newlines() {
    // Shift-Enter inserts a literal `\n` in the composing buffer.
    // The pinned line is single-row, so the renderer should collapse
    // those newlines (and runs of whitespace) to single spaces.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    let mut cmds = Vec::new();
    type_str(&mut t, "line one");
    t.handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        &mut cmds,
    );
    type_str(&mut t, "line two");
    t.handle_key(code(KeyCode::Enter), &mut cmds);

    let out = render_to_string(&mut t, 60, 10, true);
    assert!(
        out.contains("you ▸ line one line two"),
        "newline collapsed to space; got:\n{out}"
    );
}

#[test]
fn render_inserts_blank_spacer_between_recap_and_agent_grid() {
    // Issue #68: the recap used to sit on the row directly above the
    // agent grid, so output ran straight into it. There must be a
    // blank spacer row between the "you ▸ …" line and the first row
    // of agent output.
    let mut t = TerminalStack::new(PaneId::new(1));
    t.on_event(&spawned(1, "o/r#1", TerminalKind::Agent("claude".into())));
    t.set_active_session(Some(sk("o/r#1")));

    // Put output on the agent's grid so the body's first row is
    // distinguishable from the blank spacer.
    t.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"AGENTLINE".to_vec(),
        first_seq: 1,
        seq: 1,
    });
    type_str(&mut t, "do the thing");
    let mut cmds = Vec::new();
    t.handle_key(code(KeyCode::Enter), &mut cmds);

    let out = render_to_string(&mut t, 60, 10, true);
    let lines: Vec<&str> = out.lines().collect();
    let recap_idx = lines
        .iter()
        .position(|l| l.contains("you ▸ do the thing"))
        .unwrap_or_else(|| panic!("recap row present; got:\n{out}"));
    assert!(
        lines[recap_idx + 1].trim().is_empty(),
        "row below recap must be a blank spacer; got:\n{out}"
    );
    let agent_idx = lines
        .iter()
        .position(|l| l.contains("AGENTLINE"))
        .unwrap_or_else(|| panic!("agent output present; got:\n{out}"));
    assert!(
        agent_idx >= recap_idx + 2,
        "agent output must start below the spacer; got:\n{out}"
    );
}

// ── Footer hint bar (from #25) ─────────────────────────────────────────

#[test]
fn footer_drops_all_keys_to_pty_noise() {
    let bindings = TerminalStack::contextual_bindings(']');
    let labels: Vec<String> = bindings.iter().map(|b| b.label.to_string()).collect();
    assert!(
        !labels.iter().any(|l| l.contains("→ PTY")),
        "footer must not advertise `→ PTY` mode, got {labels:?}",
    );
    let keys: Vec<String> = bindings.iter().map(|b| b.keys.to_string()).collect();
    assert!(
        !keys.iter().any(|k| k == "all keys"),
        "footer must not advertise the `all keys` pseudo-binding, got {keys:?}",
    );
    assert_eq!(labels, ["menu"]);
    assert_eq!(keys, ["]]"]);
}

#[test]
fn footer_collapses_terminal_commands_into_the_leader_menu() {
    let bindings = TerminalStack::contextual_bindings(']');
    let labels: Vec<String> = bindings.iter().map(|b| b.label.to_string()).collect();
    let keys: Vec<String> = bindings.iter().map(|b| b.keys.to_string()).collect();
    assert_eq!(labels, ["menu"]);
    assert_eq!(keys, ["]]"]);
}

#[test]
fn footer_leader_hints_honor_the_configured_escape_char() {
    let bindings = TerminalStack::contextual_bindings('}');
    let keys: Vec<String> = bindings.iter().map(|b| b.keys.to_string()).collect();
    assert_eq!(keys, ["}}"]);
}

#[test]
fn every_footer_hint_is_catalog_backed_or_allowlisted() {
    use lazybox_tui_core::action::ActionDef;
    let overrides = std::collections::BTreeMap::new();
    let bindings = TerminalStack::contextual_bindings(']');
    let catalog = ActionDef::catalog(&[], &overrides);
    let mut catalog_pairs: Vec<(String, String)> = catalog
        .iter()
        .map(|e| (e.keys_display.to_string(), e.label.to_string()))
        .collect();
    catalog_pairs.push(("]]".to_string(), "menu".to_string()));
    for b in &bindings {
        let keys = b.keys.to_string();
        let label = b.label.to_string();
        let backed = catalog_pairs.iter().any(|(k, l)| *k == keys && *l == label);
        assert!(
            backed,
            "footer hint `{keys}` / `{label}` is neither catalog-backed nor allowlisted",
        );
    }
}

#[test]
fn footer_menu_hint_tracks_the_escape_char() {
    let bindings = TerminalStack::contextual_bindings(']');
    let menu = bindings.iter().find(|b| b.label == "menu");
    assert_eq!(menu.expect("menu binding").keys, "]]");

    let bindings = TerminalStack::contextual_bindings('}');
    let menu = bindings.iter().find(|b| b.label == "menu");
    assert_eq!(menu.expect("menu binding").keys, "}}");
}
