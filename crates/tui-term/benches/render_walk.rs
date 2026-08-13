//! Per-frame terminal *render* cost — the walk `GhosttyTerminal` does to
//! paint one VT snapshot into a ratatui buffer (issue #1090).
//!
//! This is distinct from `terminal_feed` (crate `lazybox-tui`), which
//! measures the VT *parse* cost of feeding bytes. Here nothing is fed
//! inside the timed loop: the tiles are pre-filled once, then repainted
//! every iteration. That is deliberately the #1090 workload — the render
//! phase is hot *even when the content did not change* (a keystroke in
//! another pane, a spinner tick), because the widget walks every viewport
//! cell every frame with ~5 FFI round-trips per cell and has **no
//! content-changed short-circuit** (`ghostty_widget.rs` module docs).
//!
//! Two things the numbers should show:
//!   1. `render_1_tile` — the cost of one full-grid walk. Multiply by the
//!      run loop's frame rate to see the steady-state floor.
//!   2. `render_{4,9}_tiles` — the focus-mode grid cockpit (#1057). Cost
//!      scales linearly with visible tiles, so a grid of streaming agents
//!      stacks straight into the reported 100-420ms frames.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use lazybox_tui_term::GhosttyTerminal;
use libghostty_vt as vt;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

/// Default focus-mode-ish grid the issue's numbers were taken at
/// (~3,840 cells → ~19k FFI calls per full walk).
const COLS: u16 = 120;
const ROWS: u16 = 32;

/// A deterministic stand-in for a chatty agent's rendered screen: colored
/// status lines, a spinner, and tool-call output — the escape-heavy
/// traffic Claude Code leaves on screen. Fed once to fill the viewport;
/// the render walk then repaints whatever landed there.
fn chatty_corpus() -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    for i in 0..256 {
        let mut chunk = Vec::new();
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

/// One embedded terminal's full render pipeline — mirrors what
/// `TermSession` holds per slot (`session.rs`).
struct Tile {
    terminal: vt::Terminal<'static, 'static>,
    render_state: vt::RenderState<'static>,
    row_iter: vt::render::RowIterator<'static>,
    cell_iter: vt::render::CellIterator<'static>,
    shadow: Option<Buffer>,
    last_visible_cursor: Option<vt::render::CursorViewport>,
}

impl Tile {
    fn new(cols: u16, rows: u16) -> Self {
        let terminal = vt::Terminal::new(vt::TerminalOptions {
            cols,
            rows,
            max_scrollback_lines: 10_000,
            max_scrollback_bytes: None,
        })
        .expect("libghostty-vt init");
        Self {
            terminal,
            render_state: vt::RenderState::new().expect("render state"),
            row_iter: vt::render::RowIterator::new().expect("row iter"),
            cell_iter: vt::render::CellIterator::new().expect("cell iter"),
            shadow: None,
            last_visible_cursor: None,
        }
    }

    /// Fill the viewport once, outside the timed loop.
    fn prefill(&mut self, corpus: &[Vec<u8>]) {
        for chunk in corpus {
            self.terminal.vt_write(chunk);
        }
    }

    /// One render pass — exactly what `render_one_terminal` does per frame
    /// (`terminal_stack.rs:4400`): update the snapshot, then walk it into
    /// the buffer through the widget.
    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if let Ok(snapshot) = self.render_state.update(&self.terminal) {
            let widget = GhosttyTerminal::new(
                &snapshot,
                &mut self.row_iter,
                &mut self.cell_iter,
                &mut self.shadow,
                &mut self.last_visible_cursor,
            );
            widget.render(area, buf);
        }
    }
}

fn bench_render(c: &mut Criterion) {
    let corpus = chatty_corpus();
    let area = Rect::new(0, 0, COLS, ROWS);

    let mut group = c.benchmark_group("render_walk");
    // Each tile is painted full-grid: an upper bound per tile (a real
    // grid subdivides the screen), but the *scaling* — linear in visible
    // tiles — is the point. `k` visible streaming agents = k full walks
    // every frame, with no short-circuit to skip the unchanged ones.
    for &k in &[1usize, 4, 9] {
        group.bench_with_input(BenchmarkId::from_parameter(k), &k, |b, &k| {
            let mut tiles: Vec<Tile> = (0..k)
                .map(|_| {
                    let mut t = Tile::new(COLS, ROWS);
                    t.prefill(&corpus);
                    t
                })
                .collect();
            let mut buf = Buffer::empty(area);
            // Steady-state repaint of UNCHANGED tiles — the keystroke /
            // spinner-tick cost the run-loop watchdog charges to `render`.
            b.iter(|| {
                for tile in tiles.iter_mut() {
                    tile.render(black_box(area), &mut buf);
                }
                black_box(&buf);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_render);
criterion_main!(benches);
