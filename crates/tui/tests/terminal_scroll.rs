//! Terminal scroll regression harness (#371).
//!
//! Terminal scrolling has been fixed and regressed repeatedly (#306,
//! #321, #360, #42, #362). This file is the permanent net: it exercises
//! every scroll surface through the real entry points so a change that
//! breaks any of them turns a test red instead of shipping.
//!
//! The per-tile #362 routing itself is covered at the model level in
//! `realm/model/tests.rs`; this harness owns the surfaces that live one
//! layer down and weren't otherwise pinned:
//!   - Fresh-spawned agent (the case that kept breaking) — wheel,
//!     Shift-PageUp/PageDown/Home/End.
//!   - Reattached session (daemon `Snapshot` replay).
//!   - Fresh spawn and reattach reach IDENTICAL scroll state (they must
//!     go through the same init).
//!   - Split tiles — scrolling a non-focused tile leaves the focused one
//!     put (#362), read at the `TerminalStack` seam.
//!   - Alternate-screen program (no local scrollback) vs. normal screen.
//!   - A no-op is never silent: whenever scrollback exists, a scroll
//!     returns `Moved`; when it genuinely can't, a typed reason.
//!   - The scroll owner is the ONLY caller of `scroll_viewport`
//!     (source-level guard, so a new raw offset poke fails the build).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_core::{SessionKey, SessionLayout, TileTree};
use lazybox_ipc::{Event, TerminalId, TerminalKind, TerminalSnapshot};
use lazybox_tui::PaneId;
use lazybox_tui::components::TerminalStack;
use lazybox_tui::components::terminal_stack::{ScrollOutcome, WheelRoute};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

const W: u16 = 100;
const H: u16 = 40;

fn sk(s: &str) -> SessionKey {
    s.into()
}

/// Render the pane the way the model does, so `ensure_size` runs, any
/// buffered output flushes into the VT, and the tile hit-rects get
/// recorded — the exact path a real frame takes before the user scrolls.
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

    // The wheel path scrolls the tile it resolves; over a single agent
    // that's this terminal. `scroll_active` is the same viewport move.
    let out = stack.scroll_active(-3);
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

    let out = stack.scroll_active(-3);
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
/// filled with scrollback.
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
fn scrolling_a_non_focused_tile_leaves_the_focused_one_put() {
    // The #362 promise at the TerminalStack seam: resolve the tile under
    // a right-half point, scroll THAT terminal, and the focused (left)
    // tile must not move. (The full wheel routing is covered end-to-end
    // in realm/model/tests.rs.)
    let mut stack = split_stack();

    // `offset()` reads the focused (left, id 1) terminal throughout.
    let left_before = offset(&stack);
    assert!(left_before > 0, "focused tile has scrollback");

    // A point in the right half resolves to the right tile.
    let right_id = stack
        .terminal_at((W * 3) / 4, H / 2)
        .expect("a right-half point lands in a tile");
    assert_eq!(
        right_id,
        TerminalId(2),
        "the right-half point resolves to the right tile, not the focused one",
    );

    let out = stack.scroll_terminal(right_id, -5);
    assert!(
        matches!(out, ScrollOutcome::Moved { .. }),
        "scrolling the right tile moves it: {out:?}",
    );
    assert_eq!(
        offset(&stack),
        left_before,
        "scrolling the RIGHT tile must not move the focused LEFT tile",
    );
}

#[test]
fn keyboard_scroll_targets_the_focused_tile() {
    // The keyboard path has no cursor, so it acts on the focused tile —
    // the left one here. Complements the cursor-directed wheel.
    let mut stack = split_stack();
    let left_before = offset(&stack);
    let mut cmds = Vec::new();
    stack.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT), &mut cmds);
    assert!(
        offset(&stack) < left_before,
        "Shift-Home moved the focused (left) tile toward the top",
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
    // Across every surface, once scrollback exists a scroll must report
    // `Moved`. A silent nothing — the recurring symptom — is impossible
    // because the outcome is always typed and, here, asserted to move.
    for mut stack in [fresh_agent(), reattached_agent()] {
        assert!(
            matches!(stack.scroll_active(-5), ScrollOutcome::Moved { .. }),
            "wheel/keyboard scroll silently failed to move",
        );
        assert!(stack.scroll_to_top().is_some(), "Shift-Home reports state");
        assert_eq!(offset(&stack), 0, "Shift-Home reached the top");
        assert!(
            stack.scroll_to_bottom().is_some(),
            "Shift-End reports state"
        );
    }
}

#[test]
fn empty_terminal_scroll_is_a_typed_reason_not_a_move() {
    // A terminal with no scrollback yet (nothing streamed) must report a
    // reason, never claim `Moved`. This keeps "there was nothing to
    // scroll" distinguishable from "scroll broke."
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
    //
    // Robust to reformatting: rather than a fixed byte window, we brace-
    // match the owner function's body and assert EVERY `scroll_viewport`
    // call in the file lands inside it. A new verb the owner adds is fine;
    // a call added anywhere else fails.
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/components/terminal_stack.rs"
    ))
    .expect("read terminal_stack.rs");

    let owner = "fn scroll(&mut self, request: ScrollRequest)";
    let owner_at = src.find(owner).expect("the scroll owner still exists");
    // Body spans from the owner's opening brace to its matching close.
    let body_open = owner_at + src[owner_at..].find('{').expect("owner has a body");
    let mut depth = 0usize;
    let mut body_close = body_open;
    for (off, ch) in src[body_open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    body_close = body_open + off;
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(body_close > body_open, "owner body brace-match failed");

    let calls: Vec<usize> = src
        .match_indices(".scroll_viewport(")
        .map(|(i, _)| i)
        .collect();
    assert!(
        !calls.is_empty(),
        "no scroll_viewport calls found — did the owner or its call syntax change?",
    );
    for idx in calls {
        assert!(
            idx > body_open && idx < body_close,
            "a `scroll_viewport` call escaped the scroll owner (byte {idx}, owner \
             body {body_open}..{body_close}) — route it through `TerminalVt::scroll`",
        );
    }
}
