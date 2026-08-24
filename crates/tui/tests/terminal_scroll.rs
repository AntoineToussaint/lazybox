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
//!   - Empty local history vs. populated history.
//!   - A no-op is never silent: whenever scrollback exists, a scroll
//!     returns `Moved`; when it genuinely can't, a typed reason.
//!   - The scroll owner is the ONLY caller of `scroll_viewport`
//!     (source-level guard, so a new raw offset poke fails the build).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_core::{SessionKey, SessionLayout, TileTree};
use lazybox_ipc::{Event, TerminalId, TerminalKind, TerminalSnapshot};
use lazybox_tui::PaneId;
use lazybox_tui::components::TerminalStack;
use lazybox_tui::components::terminal_stack::{ScrollBoundary, ScrollOutcome};
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

/// Assert that a typed successful outcome describes the exact observed
/// viewport transition. This is intentionally stronger than matching
/// `Moved`: the old implementation returned `Moved` whenever scrollback
/// existed, even when libghostty's offset had not changed.
fn assert_moved_from(stack: &TerminalStack, before: u64, outcome: ScrollOutcome) -> u64 {
    match outcome {
        ScrollOutcome::Moved { from, offset, .. } => {
            assert_eq!(
                from, before,
                "outcome must carry the real pre-scroll offset"
            );
            assert_ne!(offset, from, "Moved must represent an actual transition");
            assert_eq!(
                offset,
                self::offset(stack),
                "outcome must carry the real post-scroll offset",
            );
            offset
        }
        other => panic!("scroll should have moved from {before}, got {other:?}"),
    }
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
        bytes: scrollback_payload().into(),
        first_seq: 1,
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
            replay_available: true,
            no_permission: false,
            on_main: false,
            model_label: None,
            prompt_history: Vec::new(),
            composing_buffer: None,
            agent_state: None,
            authenticating: false,
        }],
        recent_snippets: Vec::new(),
        dismissed_updates: Vec::new(),
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

    // The wheel path scrolls the tile it resolves; over a single agent
    // that's this terminal. `scroll_active` is the same viewport move.
    let out = stack.scroll_active(-3);
    let after = assert_moved_from(&stack, bottom, out);
    assert!(after < bottom, "the wheel scrolled up into history");
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

    // The viewport moves are in-process; the only allowed side effect
    // is the deep-scrollback fetch (#393) — never a PTY write, which
    // would leak the scroll keys into the inner program.
    assert!(
        cmds.iter()
            .all(|c| matches!(c, lazybox_ipc::Command::FetchScrollback { .. })),
        "keyboard scroll must not write to the PTY: {cmds:?}"
    );
}

// ── Reattach ────────────────────────────────────────────────────────

#[test]
fn reattached_agent_wheel_moves_the_viewport() {
    let mut stack = reattached_agent();
    let bottom = offset(&stack);
    assert!(bottom > 0, "replay must reconstruct scrollback");

    let out = stack.scroll_active(-3);
    assert!(assert_moved_from(&stack, bottom, out) < bottom);
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
            bytes: scrollback_payload().into(),
            first_seq: 1,
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
        .scroll_terminal_at((W * 3) / 4, H / 2)
        .expect("a right-half point lands in a tile");
    assert_eq!(
        right_id,
        TerminalId(2),
        "the right-half point resolves to the right tile, not the focused one",
    );

    let out = stack.scroll_terminal(right_id, -5);
    match out {
        ScrollOutcome::Moved { from, offset, .. } => assert!(
            offset < from,
            "scrolling the right tile must reduce its offset: {from} -> {offset}",
        ),
        other => panic!("scrolling the right tile must move it, got {other:?}"),
    }
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

// ── The core invariant: no silent no-op ─────────────────────────────

#[test]
fn scroll_never_silently_noops_when_scrollback_exists() {
    // Across every surface, once scrollback exists a scroll must report
    // the exact before/after transition. Merely returning `Moved` is not
    // sufficient: that false-positive implementation is the regression
    // this test exists to prevent.
    for mut stack in [fresh_agent(), reattached_agent()] {
        let bottom = offset(&stack);
        let first = stack.scroll_active(-5);
        let after_first = assert_moved_from(&stack, bottom, first);
        let to_top = stack.scroll_to_top();
        assert_moved_from(&stack, after_first, to_top);
        assert_eq!(offset(&stack), 0, "Shift-Home reached the top");
        let to_bottom = stack.scroll_to_bottom();
        assert_moved_from(&stack, 0, to_bottom);
        assert_eq!(offset(&stack), bottom, "Shift-End reached the live bottom");
    }
}

#[test]
fn expected_noops_are_typed_and_never_claim_movement() {
    let mut stack = fresh_agent();
    let bottom = offset(&stack);

    assert_eq!(stack.scroll_active(0), ScrollOutcome::Noop);
    assert!(matches!(
        stack.scroll_active(3),
        ScrollOutcome::AtBoundary {
            boundary: ScrollBoundary::Bottom,
            offset,
            ..
        } if offset == bottom
    ));
    assert!(matches!(
        stack.scroll_to_bottom(),
        ScrollOutcome::AtBoundary {
            boundary: ScrollBoundary::Bottom,
            offset,
            ..
        } if offset == bottom
    ));

    let to_top = stack.scroll_to_top();
    assert_moved_from(&stack, bottom, to_top);
    assert!(matches!(
        stack.scroll_active(-3),
        ScrollOutcome::AtBoundary {
            boundary: ScrollBoundary::Top,
            offset: 0,
            ..
        }
    ));
    assert!(matches!(
        stack.scroll_to_top(),
        ScrollOutcome::AtBoundary {
            boundary: ScrollBoundary::Top,
            offset: 0,
            ..
        }
    ));
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
    assert_eq!(stack.scroll_active(-3), ScrollOutcome::NoScrollback);
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
    let owner_path = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/components/terminal_stack.rs"
    ));
    let src = std::fs::read_to_string(&owner_path).expect("read terminal_stack.rs");

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
    let owner_body = &src[body_open..=body_close];
    assert!(
        owner_body.contains("classify_scroll_transition(request, before, after)"),
        "the scroll owner must classify the observed before/after offsets",
    );

    fn collect_rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read source directory") {
            let path = entry.expect("read source entry").path();
            if path.is_dir() {
                collect_rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    // Search the whole TUI source tree, not just the current owner's
    // file. Otherwise a raw mutation added to a different module would
    // bypass the supposed crate-wide single-owner guarantee.
    let mut paths = Vec::new();
    collect_rust_sources(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut paths,
    );
    let mut call_count = 0usize;
    for path in paths {
        let candidate = std::fs::read_to_string(&path).expect("read Rust source");
        for (idx, _) in candidate.match_indices(".scroll_viewport(") {
            call_count += 1;
            assert_eq!(
                path,
                owner_path,
                "a `scroll_viewport` call escaped the owner into {}",
                path.display(),
            );
            assert!(
                idx > body_open && idx < body_close,
                "a `scroll_viewport` call escaped the scroll owner (byte {idx}, owner \
                 body {body_open}..{body_close}) — route it through `TerminalVt::scroll`",
            );
        }
    }
    assert!(
        call_count > 0,
        "no scroll_viewport calls found — did the owner or its call syntax change?",
    );
}
