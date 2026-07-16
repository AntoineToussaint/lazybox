//! Terminal scroll regression harness (#371).
//!
//! Terminal scrolling has been fixed and regressed repeatedly (#306,
//! #321, #360, #42, #362). This file is the permanent net: it exercises
//! EVERY scroll surface through the real entry points so a change that
//! breaks any of them turns a test red instead of shipping.
//!
//! Surfaces covered:
//!   - Fresh-spawned agent (the case that kept breaking) — wheel,
//!     Shift-PageUp/PageDown/Home/End.
//!   - Reattached session (daemon `Snapshot` replay).
//!   - Fresh spawn and reattach reach IDENTICAL scroll state (they must
//!     go through the same init).
//!   - Split tiles — a wheel scrolls the tile UNDER THE CURSOR, not the
//!     focused one (#362).
//!   - Alternate-screen program (no local scrollback) vs. normal screen.
//!   - A no-op is never silent: whenever scrollback exists, a scroll
//!     request returns `Moved`; when it genuinely can't, it returns a
//!     typed reason (`NoScrollback` / `NoTerminal`).
//!   - The scroll owner is the ONLY caller of `scroll_viewport`
//!     (source-level guard, so a new raw offset poke fails the build).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_core::{SessionKey, SessionLayout, TileTree};
use lazybox_ipc::{Event, TerminalId, TerminalKind, TerminalSnapshot};
use lazybox_tui::PaneId;
use lazybox_tui::components::TerminalStack;
use lazybox_tui::components::terminal_stack::{ScrollOutcome, ScrollRequest, WheelRoute};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

const W: u16 = 100;
const H: u16 = 40;

fn sk(s: &str) -> SessionKey {
    s.into()
}

/// Render the pane the way the model does, so `ensure_size` runs and any
/// buffered output flushes into the VT — the exact path a real frame
/// takes before the user scrolls.
fn render(stack: &mut TerminalStack) {
    let backend = TestBackend::new(W, H);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| stack.render(Rect::new(0, 0, W, H), f, true))
        .unwrap();
}

/// A run of plain scrolling output guaranteed to overflow the viewport
/// and spill into scrollback.
fn scrollback_payload() -> Vec<u8> {
    let mut p = String::new();
    for i in 0..80 {
        p.push_str(&format!("output line {i}\r\n"));
    }
    p.into_bytes()
}

/// Pull `offset=` out of the focused terminal's scrollbar summary.
fn offset(stack: &TerminalStack) -> u64 {
    stack
        .scrollbar_summary()
        .expect("focused terminal has a scrollbar summary")
        .split_whitespace()
        .find_map(|kv| kv.strip_prefix("offset="))
        .expect("offset field")
        .parse()
        .expect("numeric offset")
}

/// A fresh-spawned agent driven entirely through the daemon event path —
/// `TerminalSpawned` then `TerminalOutput`, never a reattach/replay. This
/// is the case the chronic regression always bit.
fn fresh_agent() -> TerminalStack {
    let mut stack = TerminalStack::new(PaneId::new(0));
    stack.on_event(&Event::TerminalSpawned {
        terminal_id: TerminalId(1),
        session_key: sk("s"),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
        model_label: None,
    });
    stack.set_active_session(Some(sk("s")));
    render(&mut stack);
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: scrollback_payload(),
        seq: 1,
    });
    render(&mut stack);
    stack
}

/// The same terminal, but reconstructed from a daemon `Snapshot` replay
/// — the reattach-after-restart path.
fn reattached_agent() -> TerminalStack {
    let mut stack = TerminalStack::new(PaneId::new(0));
    stack.on_event(&Event::Snapshot {
        workspaces: vec![],
        projects: vec![],
        terminals: vec![TerminalSnapshot {
            terminal_id: TerminalId(1),
            session_key: sk("s"),
            kind: TerminalKind::Agent("claude".into()),
            replay: scrollback_payload(),
            last_seq: 1,
            no_permission: false,
            on_main: false,
            model_label: None,
            last_user_message: None,
        }],
    });
    stack.set_active_session(Some(sk("s")));
    render(&mut stack);
    stack
}

// ── Fresh spawn ─────────────────────────────────────────────────────

#[test]
fn fresh_agent_wheel_moves_the_viewport() {
    let mut stack = fresh_agent();
    let bottom = offset(&stack);
    assert!(
        bottom > 0,
        "80 lines of fresh output must produce scrollback"
    );

    // A primary-screen agent's wheel is always lazybox's scrollback.
    assert_eq!(
        stack.wheel_route(),
        WheelRoute::LocalScrollback,
        "a fresh primary-screen agent scrolls locally on the wheel (#360)",
    );

    let out = stack.scroll_at(Rect::new(0, 0, W, H), W / 2, H / 2, ScrollRequest::By(-3));
    assert!(
        matches!(out, ScrollOutcome::Moved { .. }),
        "fresh-spawn wheel must move the viewport, got {out:?}",
    );
    assert!(
        offset(&stack) < bottom,
        "the wheel scrolled up into history"
    );
}

#[test]
fn fresh_agent_keyboard_bindings_move_the_viewport() {
    let mut stack = fresh_agent();
    let bottom = offset(&stack);
    let mut cmds = Vec::new();
    let mut press = |stack: &mut TerminalStack, code| {
        stack.handle_key(KeyEvent::new(code, KeyModifiers::SHIFT), &mut cmds)
    };

    press(&mut stack, KeyCode::Home);
    assert_eq!(
        offset(&stack),
        0,
        "Shift-Home jumps to the top of scrollback"
    );

    press(&mut stack, KeyCode::PageDown);
    let after_pgdn = offset(&stack);
    assert!(after_pgdn > 0, "Shift-PageDown walks back down");

    press(&mut stack, KeyCode::PageUp);
    assert!(offset(&stack) < after_pgdn, "Shift-PageUp walks back up");

    press(&mut stack, KeyCode::End);
    assert_eq!(
        offset(&stack),
        bottom,
        "Shift-End returns to the live bottom"
    );

    assert!(cmds.is_empty(), "keyboard scroll is a pure in-process move");
}

// ── Reattach ────────────────────────────────────────────────────────

#[test]
fn reattached_agent_wheel_moves_the_viewport() {
    let mut stack = reattached_agent();
    let bottom = offset(&stack);
    assert!(bottom > 0, "replay must reconstruct scrollback");

    let out = stack.scroll_at(Rect::new(0, 0, W, H), W / 2, H / 2, ScrollRequest::By(-3));
    assert!(
        matches!(out, ScrollOutcome::Moved { .. }),
        "reattached wheel must move the viewport, got {out:?}",
    );
    assert!(offset(&stack) < bottom);
}

#[test]
fn fresh_and_reattach_reach_identical_scroll_state() {
    // Same bytes in, one via live `TerminalOutput`, the other via
    // `Snapshot` replay: both must land on the SAME scrollbar state.
    // A divergence here means the two init paths drifted — exactly the
    // class of bug #371 exists to prevent.
    let fresh = fresh_agent();
    let reattached = reattached_agent();
    assert_eq!(
        fresh.scrollbar_summary(),
        reattached.scrollbar_summary(),
        "fresh spawn and reattach must reconstruct identical scroll state",
    );
}

// ── Split tiles (#362) ──────────────────────────────────────────────

/// Two agents side by side in one HSplit, focus on the LEFT leaf, both
/// filled with scrollback. A wheel over the RIGHT tile must scroll the
/// right terminal and leave the focused (left) one put.
fn split_stack() -> TerminalStack {
    let mut stack = TerminalStack::new(PaneId::new(0));
    for id in [1u64, 2] {
        stack.on_event(&Event::TerminalSpawned {
            terminal_id: TerminalId(id),
            session_key: sk("s"),
            kind: TerminalKind::Agent("claude".into()),
            no_permission: false,
            on_main: false,
            model_label: None,
        });
    }
    stack.set_active_session(Some(sk("s")));
    stack.set_layout(SessionLayout::Splits {
        tree: TileTree::HSplit {
            left: Box::new(TileTree::Leaf { terminal_id: 1 }),
            right: Box::new(TileTree::Leaf { terminal_id: 2 }),
            ratio: 50,
        },
        focused: vec![0], // left leaf holds focus
    });
    render(&mut stack);
    for id in [1u64, 2] {
        stack.on_event(&Event::TerminalOutput {
            terminal_id: TerminalId(id),
            bytes: scrollback_payload(),
            seq: 1,
        });
    }
    render(&mut stack);
    stack
}

#[test]
fn wheel_targets_the_tile_under_the_cursor_not_the_focused_tile() {
    let mut stack = split_stack();
    let rect = Rect::new(0, 0, W, H);

    // Focused (left) terminal's live-bottom offset, read via a no-move
    // query (`By(0)` returns the current state without scrolling).
    let left_bottom = match stack.scroll_active(0) {
        ScrollOutcome::Moved { offset, .. } => offset,
        other => panic!("focused (left) tile must have scrollback: {other:?}"),
    };
    assert!(left_bottom > 0);

    // The right tile lives past the mid-pane divider. Scroll over it.
    let right_col = (W * 3) / 4;
    let right_out = stack.scroll_at(rect, right_col, H / 2, ScrollRequest::By(-4));
    let right_offset = match right_out {
        ScrollOutcome::Moved { offset, .. } => offset,
        other => panic!("wheel over the right tile must move it: {other:?}"),
    };

    // The right tile moved up into scrollback…
    assert!(
        right_offset < left_bottom,
        "wheel over the right tile scrolled it (right={right_offset} bottom={left_bottom})",
    );
    // …and the focused (left) tile did NOT move — the #362 bug was that
    // it always scrolled the focused tile regardless of the cursor.
    let left_after = match stack.scroll_active(0) {
        ScrollOutcome::Moved { offset, .. } => offset,
        other => panic!("left tile lost its scrollback: {other:?}"),
    };
    assert_eq!(
        left_after, left_bottom,
        "a wheel over the RIGHT tile must not move the focused LEFT tile",
    );
}

#[test]
fn keyboard_scroll_targets_the_focused_tile() {
    // The keyboard path has no cursor, so it acts on the focused tile —
    // the left one here. This pins the complement of the #362 fix: only
    // the wheel is cursor-directed.
    let mut stack = split_stack();
    let left_bottom = match stack.scroll_active(0) {
        ScrollOutcome::Moved { offset, .. } => offset,
        other => panic!("focused tile must have scrollback: {other:?}"),
    };
    let mut cmds = Vec::new();
    stack.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT), &mut cmds);
    let left_after = match stack.scroll_active(0) {
        ScrollOutcome::Moved { offset, .. } => offset,
        other => panic!("focused tile lost scrollback: {other:?}"),
    };
    assert!(
        left_after < left_bottom,
        "Shift-Home moved the focused (left) tile to the top",
    );
}

// ── Alternate vs normal screen ──────────────────────────────────────

#[test]
fn alternate_screen_reports_no_local_scrollback() {
    // An alt-screen app owns its buffer; libghostty keeps no scrollback
    // for it, so a local scroll is a TYPED no-op (not a silent one) and
    // the wheel routes away from local scrollback.
    let mut stack = TerminalStack::new(PaneId::new(0));
    stack.on_event(&Event::TerminalSpawned {
        terminal_id: TerminalId(1),
        session_key: sk("s"),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
        model_label: None,
    });
    stack.set_active_session(Some(sk("s")));
    render(&mut stack);
    // Enter alt-screen and enable mouse reporting (vim `mouse=a`, htop…).
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: b"\x1b[?1049h\x1b[?1006h\x1b[?1002hhello".to_vec(),
        seq: 1,
    });
    render(&mut stack);

    assert_ne!(
        stack.wheel_route(),
        WheelRoute::LocalScrollback,
        "an alt-screen mouse-tracking app owns the wheel, not lazybox",
    );
    match stack.scroll_active(-3) {
        ScrollOutcome::NoScrollback { alternate: true } => {}
        other => panic!("alt-screen scroll must report NoScrollback{{alternate}}, got {other:?}"),
    }
}

// ── The core invariant: no silent no-op ─────────────────────────────

#[test]
fn scroll_never_silently_noops_when_scrollback_exists() {
    // Across every surface, once scrollback exists a scroll request must
    // report `Moved`. A silent nothing — the recurring symptom — is
    // impossible because the outcome is always typed and, here, asserted
    // to be a real move.
    for mut stack in [fresh_agent(), reattached_agent()] {
        let rect = Rect::new(0, 0, W, H);
        for req in [
            ScrollRequest::Top,
            ScrollRequest::By(-5),
            ScrollRequest::Bottom,
        ] {
            match stack.scroll_at(rect, W / 2, H / 2, req) {
                ScrollOutcome::Moved { .. } => {}
                other => panic!("{req:?} silently failed to move: {other:?}"),
            }
        }
    }
}

#[test]
fn empty_terminal_scroll_is_a_typed_reason_not_a_move() {
    // A terminal with no scrollback yet (nothing streamed) must report a
    // reason, never claim `Moved`. This is the flip side that keeps
    // "there was nothing to scroll" distinguishable from "scroll broke".
    let mut stack = TerminalStack::new(PaneId::new(0));
    stack.on_event(&Event::TerminalSpawned {
        terminal_id: TerminalId(1),
        session_key: sk("s"),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
        model_label: None,
    });
    stack.set_active_session(Some(sk("s")));
    render(&mut stack);
    match stack.scroll_active(-3) {
        ScrollOutcome::NoScrollback { alternate: false } => {}
        other => panic!("an empty primary-screen terminal must report NoScrollback, got {other:?}"),
    }
}

// ── Source-level encapsulation guard ────────────────────────────────

#[test]
fn scroll_viewport_has_a_single_owner() {
    // The whole point of #371: exactly one place mutates the viewport.
    // If a handler reintroduces a raw `scroll_viewport` poke, this fails
    // — the mechanical backstop that makes a regression un-mergeable.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/components/terminal_stack.rs"
    ))
    .expect("read terminal_stack.rs");

    let owner = "fn scroll(&mut self, request: ScrollRequest)";
    let owner_at = src.find(owner).expect("the scroll owner still exists");
    // The three verbs (Delta/Top/Bottom) live in the owner's body; no
    // `scroll_viewport` call may appear anywhere else in the file.
    for (idx, _) in src.match_indices(".scroll_viewport(") {
        assert!(
            idx > owner_at && idx < owner_at + 1200,
            "a `scroll_viewport` call escaped the scroll owner \
             (byte {idx}, owner at {owner_at}) — route it through \
             `TerminalVt::scroll`",
        );
    }
    assert_eq!(
        src.matches(".scroll_viewport(").count(),
        3,
        "the owner calls scroll_viewport exactly three times (Delta/Top/Bottom)",
    );
}
