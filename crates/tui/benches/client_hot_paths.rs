//! Wall-clock cost of the client's four hot paths (#1919).
//!
//! The regression gate lives in `tests/terminal_hot_paths.rs` and asserts
//! **counted work**, because a time threshold flakes on a shared box and
//! on a CI runner. This file is the other half: numbers for humans, run
//! deliberately on a quiet machine with `make bench`, and quoted in the PR
//! that changes any of these paths.
//!
//! Driven through `TerminalStack`'s real public surface — the daemon event
//! path for output, `render` for a frame, `handle_key` for a keystroke —
//! so what is measured is the path the UI loop actually takes, chrome,
//! tile hits, composing-buffer bookkeeping and all. The per-chunk VT parse
//! in isolation is `terminal_feed.rs`'s job.
//!
//! Groups:
//!   - `client/feed_following_tail` — output arriving while the viewport
//!     follows the live tail. The typing path, and the one #1918 was
//!     about.
//!   - `client/feed_parked` — the same output while the user is scrolled
//!     up reading history, where the anchor is still re-derived per chunk
//!     because the distance genuinely changed.
//!   - `client/render_changed` — a frame that walks the grid.
//!   - `client/render_unchanged` — a repaint of an unmutated grid, which
//!     must come from the cached-frame blit.
//!   - `client/keystroke` — one key through `handle_key` to the
//!     `Command::Write` that carries it.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_ipc::{Command, Event, TerminalId, TerminalKind};
use lazybox_tui::PaneId;
use lazybox_tui::components::TerminalStack;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

const W: u16 = 120;
const H: u16 = 50;

/// Point `LAZYBOX_HOME` at a throwaway dir before `main`, the same floor
/// `tests/common/mod.rs` installs: a bench binary links the production
/// library, and anything it runs that resolves config must not read or
/// rewrite the developer's real `~/.lazybox/config.yaml` (#1539, #1751).
#[ctor::ctor]
unsafe fn redirect_config_home() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "lazybox-tui-bench-sandbox-{}-{}",
        std::process::id(),
        nanos
    ));
    let _ = std::fs::create_dir_all(&dir);
    // SAFETY: a `#[ctor]` runs before `main`, single-threaded, and no
    // other initializer in this binary spawns a thread.
    unsafe { std::env::set_var("LAZYBOX_HOME", &dir) };
    lazybox_config::Config::invalidate_cache();
}

fn render(stack: &mut TerminalStack) {
    let backend = TestBackend::new(W, H);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| stack.render(Rect::new(0, 0, W, H), f, true))
        .unwrap();
}

/// A deterministic stand-in for a chatty agent's output — the same shape
/// as `terminal_feed.rs`'s corpus: colored status lines, cursor moves, a
/// spinner redraw, split into PTY-reader-sized chunks.
fn chatty_corpus(chunks: usize) -> Vec<Vec<u8>> {
    (0..chunks)
        .map(|i| {
            let mut chunk = Vec::new();
            chunk.extend_from_slice(b"\x1b[2K\r");
            chunk.extend_from_slice(format!("\x1b[38;5;{}m", 16 + (i % 200)).as_bytes());
            chunk.extend_from_slice(format!("· working ({i}) ").as_bytes());
            chunk.extend_from_slice("⠋⠙⠹⠸⠼".as_bytes());
            chunk.extend_from_slice(b"\x1b[0m");
            chunk.extend_from_slice(
                format!(" tool call #{i}: read file foo/bar/baz.rs\r\n").as_bytes(),
            );
            chunk
        })
        .collect()
}

/// A focused agent terminal with real scrollback, built through the daemon
/// event path. Returns the stack, the acked PTY size its chunks must be
/// stamped with, and the next free sequence number.
fn agent(history_lines: usize) -> (TerminalStack, u16, u16, u64) {
    let mut stack = TerminalStack::new(PaneId::new(0));
    stack.on_event(&Event::TerminalSpawned {
        terminal_id: TerminalId(1),
        session_key: "bench".into(),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
        model_label: None,
        agent_state: None,
    });
    stack.set_active_session(Some("bench".into()));
    render(&mut stack);
    let (_, cols, rows) = stack
        .drain_pending_resizes()
        .into_iter()
        .find(|(id, _, _)| *id == TerminalId(1))
        .expect("the first render asks for a resize");
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: Vec::new().into(),
        first_seq: 1,
        seq: 1,
        cols,
        rows,
    });
    let mut history = String::new();
    for i in 0..history_lines {
        history.push_str(&format!("history line {i}\r\n"));
    }
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: history.into_bytes().into(),
        first_seq: 2,
        seq: 2,
        cols,
        rows,
    });
    render(&mut stack);
    assert!(stack.focus_terminal(TerminalId(1)));
    (stack, cols, rows, 3)
}

fn deliver(stack: &mut TerminalStack, seq: u64, cols: u16, rows: u16, bytes: &[u8]) {
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: bytes.to_vec().into(),
        first_seq: seq,
        seq,
        cols,
        rows,
    });
}

fn bench_feed(c: &mut Criterion) {
    let corpus = chatty_corpus(128);
    let mut group = c.benchmark_group("client");
    group.throughput(criterion::Throughput::Elements(corpus.len() as u64));

    group.bench_function("feed_following_tail", |b| {
        b.iter_batched(
            || agent(2_000),
            |(mut stack, cols, rows, mut seq)| {
                for chunk in &corpus {
                    deliver(&mut stack, seq, cols, rows, black_box(chunk));
                    seq += 1;
                }
                stack
            },
            criterion::BatchSize::LargeInput,
        );
    });

    group.bench_function("feed_parked", |b| {
        b.iter_batched(
            || {
                let (mut stack, cols, rows, seq) = agent(2_000);
                let _ = stack.scroll_terminal(TerminalId(1), -500);
                (stack, cols, rows, seq)
            },
            |(mut stack, cols, rows, mut seq)| {
                for chunk in &corpus {
                    deliver(&mut stack, seq, cols, rows, black_box(chunk));
                    seq += 1;
                }
                stack
            },
            criterion::BatchSize::LargeInput,
        );
    });

    group.finish();
}

fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("client");

    // A frame that must walk the grid: the VT changed since the last paint.
    group.bench_function("render_changed", |b| {
        let (mut stack, cols, rows, mut seq) = agent(2_000);
        b.iter(|| {
            deliver(&mut stack, seq, cols, rows, b"a fresh line of output\r\n");
            seq += 1;
            render(&mut stack);
        });
    });

    // A repaint with nothing changed — the cached-frame blit, which is
    // what an idle agent pane does all day.
    group.bench_function("render_unchanged", |b| {
        let (mut stack, cols, rows, seq) = agent(2_000);
        deliver(&mut stack, seq, cols, rows, b"settled\r\n");
        render(&mut stack);
        b.iter(|| render(&mut stack));
    });

    group.finish();
}

fn bench_keystroke(c: &mut Criterion) {
    let mut group = c.benchmark_group("client");

    group.bench_function("keystroke", |b| {
        let (mut stack, _, _, _) = agent(2_000);
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        b.iter(|| {
            let mut cmds: Vec<Command> = Vec::with_capacity(4);
            stack.handle_key(black_box(key), &mut cmds);
            black_box(cmds)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_feed, bench_render, bench_keystroke);
criterion_main!(benches);
