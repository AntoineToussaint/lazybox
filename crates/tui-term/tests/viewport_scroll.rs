//! The rendered frame must follow the viewport (#306): a
//! `scroll_viewport(Delta)` on the terminal has to change what
//! `GhosttyTerminal` draws, not just the scrollbar numbers.
//! Complements `libghostty-vt/tests/scroll_repro.rs` (which pins the
//! offset math) by covering the snapshot → widget half of the path.

use libghostty_vt::render::{CellIterator, RowIterator};
use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::{RenderState, Terminal, TerminalOptions};

use lazybox_tui_term::GhosttyTerminal;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

const COLS: u16 = 20;
const ROWS: u16 = 5;

fn row_text(buf: &Buffer, y: u16) -> String {
    (0..COLS)
        .map(|x| buf[(x, y)].symbol().to_string())
        .collect::<String>()
        .trim_end()
        .to_string()
}

#[test]
fn rendered_frame_follows_delta_scroll() {
    let mut terminal = Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback_lines: 10_000,
        max_scrollback_bytes: None,
    })
    .unwrap();
    for i in 0..50 {
        terminal.vt_write(format!("line {i}\r\n").as_bytes());
    }

    let mut render_state = RenderState::new().unwrap();
    let mut row_iter = RowIterator::new().unwrap();
    let mut cell_iter = CellIterator::new().unwrap();
    let mut shadow = None;
    let mut last_visible_cursor = None;
    let area = Rect::new(0, 0, COLS, ROWS);
    // A macro rather than a helper fn: `Terminal<'alloc, 'cb>` is
    // invariant in 'alloc, so a parameterised render helper doesn't
    // borrow-check (same story as the Harness in ghostty_widget tests).
    macro_rules! render {
        () => {{
            let snapshot = render_state.update(&terminal).unwrap();
            let widget = GhosttyTerminal::new(
                &snapshot,
                &mut row_iter,
                &mut cell_iter,
                &mut shadow,
                &mut last_visible_cursor,
            );
            let mut buf = Buffer::empty(area);
            widget.render(area, &mut buf);
            buf
        }};
    }

    let at_bottom = render!();
    // 50 lines on a 5-row grid: the bottom of the scrollback is on
    // screen, the earlier lines are history.
    assert_eq!(row_text(&at_bottom, 0), "line 46");

    terminal.scroll_viewport(ScrollViewport::Delta(-5));
    let scrolled = render!();
    assert_eq!(
        row_text(&scrolled, 0),
        "line 41",
        "Delta(-5) must shift the drawn rows five lines up"
    );

    terminal.scroll_viewport(ScrollViewport::Bottom);
    let back = render!();
    assert_eq!(row_text(&back, 0), "line 46", "Bottom returns to live");
}
