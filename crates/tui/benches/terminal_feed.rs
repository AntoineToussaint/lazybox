//! Per-drain VT-parse cost as the number of agent terminals grows.
//!
//! The TUI runs one libghostty-vt parser per terminal on the single UI
//! thread. Before the hidden-terminal fix, every `TerminalOutput` chunk
//! was fed to its parser regardless of whether that terminal was on
//! screen, so M chatty background agents multiplied the parse cost by M
//! every drain. The fix feeds only the displayed terminal and buffers
//! raw bytes for the rest (a cheap `Vec::extend_from_slice`), replaying
//! them lazily on the next render.
//!
//! This bench pits the two strategies against a recorded-style "chatty
//! Claude" corpus over K terminals. `feed_all` parses every chunk in
//! every terminal (old behavior); `feed_visible` parses one terminal and
//! buffers raw bytes for the other K-1 (current behavior for hidden
//! terminals). The divergence as K grows is the cost the fix removes.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use libghostty_vt as vt;

const COLS: u16 = 80;
const ROWS: u16 = 50;

/// A deterministic stand-in for a chatty agent's output: colored status
/// lines, cursor moves, and a spinner redraw — the escape-heavy traffic
/// Claude Code streams while working. Split into chunks the way the PTY
/// reader hands them to the UI.
fn chatty_corpus() -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    for i in 0..256 {
        let mut chunk = Vec::new();
        // Move cursor, set a color, print a line, reset.
        chunk.extend_from_slice(b"\x1b[2K\r");
        chunk.extend_from_slice(format!("\x1b[38;5;{}m", 16 + (i % 200)).as_bytes());
        chunk.extend_from_slice(format!("· working ({i}) ").as_bytes());
        chunk.extend_from_slice("⠋⠙⠹⠸⠼".as_bytes());
        chunk.extend_from_slice(b"\x1b[0m");
        chunk
            .extend_from_slice(format!(" tool call #{i}: read file foo/bar/baz.rs\r\n").as_bytes());
        chunks.push(chunk);
    }
    chunks
}

fn new_parser() -> vt::Terminal<'static, 'static> {
    vt::Terminal::new(vt::TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback_lines: 10_000,
        max_scrollback_bytes: None,
    })
    .expect("libghostty-vt init")
}

fn bench_feed(c: &mut Criterion) {
    let corpus = chatty_corpus();

    let mut group = c.benchmark_group("terminal_feed");
    for &k in &[1usize, 4, 16] {
        // Old behavior: K parsers, each fed every chunk.
        group.bench_with_input(BenchmarkId::new("feed_all", k), &k, |b, &k| {
            b.iter_batched(
                || (0..k).map(|_| new_parser()).collect::<Vec<_>>(),
                |mut parsers| {
                    for chunk in &corpus {
                        for p in parsers.iter_mut() {
                            p.vt_write(black_box(chunk));
                        }
                    }
                    parsers
                },
                criterion::BatchSize::SmallInput,
            );
        });

        // Current behavior: only the visible parser parses; the K-1
        // hidden terminals stash raw bytes for a deferred replay.
        group.bench_with_input(BenchmarkId::new("feed_visible", k), &k, |b, &k| {
            b.iter_batched(
                || (new_parser(), vec![Vec::<u8>::new(); k.saturating_sub(1)]),
                |(mut visible, mut hidden)| {
                    for chunk in &corpus {
                        visible.vt_write(black_box(chunk));
                        for buf in hidden.iter_mut() {
                            buf.extend_from_slice(black_box(chunk));
                        }
                    }
                    (visible, hidden)
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_feed);
criterion_main!(benches);
