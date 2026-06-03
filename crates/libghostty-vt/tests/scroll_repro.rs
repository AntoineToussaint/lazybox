//! Pin down scroll_viewport behavior against libghostty so we can tell
//! whether lazybox's "scrolling doesn't move offset" bug is in libghostty,
//! in lazybox's call sites, or in the rendering path.
//!
//! Asserts the contract the lazybox UI assumes: feed >> rows worth of
//! content, scroll back via Delta(-N), and verify scrollbar().offset
//! moves toward zero. Also exercises Top / Bottom anchors.

use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::{Terminal, TerminalOptions};

fn make_terminal() -> Terminal<'static, 'static> {
    let mut t = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 10_000,
    })
    .expect("Terminal::new");
    for i in 0..200 {
        let line = format!("line {i}\r\n");
        t.vt_write(line.as_bytes());
    }
    t
}

#[test]
fn delta_negative_moves_offset_up() {
    let t = make_terminal();
    let before = t.scrollbar().unwrap();
    let mut t = t;
    t.scroll_viewport(ScrollViewport::Delta(-5));
    let after = t.scrollbar().unwrap();
    assert!(
        after.offset < before.offset,
        "Delta(-5) should reduce offset (before={} after={} total={} len={})",
        before.offset,
        after.offset,
        after.total,
        after.len,
    );
}

#[test]
fn top_then_bottom_round_trips() {
    let mut t = make_terminal();
    t.scroll_viewport(ScrollViewport::Top);
    let top = t.scrollbar().unwrap();
    assert_eq!(top.offset, 0, "Top should pin offset=0");
    t.scroll_viewport(ScrollViewport::Bottom);
    let bottom = t.scrollbar().unwrap();
    assert_eq!(
        bottom.offset,
        bottom.total - bottom.len,
        "Bottom should pin offset to total-len",
    );
}

#[test]
fn delta_with_no_scrollback_is_noop() {
    // Lazybox's terminal panes start nearly empty: a fresh claude UI or
    // shell prompt fits inside the active area, so total == len and
    // there's nothing to scroll into. Confirm libghostty correctly
    // reports a stable offset in that case so we can teach the UI to
    // surface a clear notice instead of silently doing nothing.
    let mut t = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 24,
        max_scrollback: 10_000,
    })
    .unwrap();
    // Only a few lines — well under 24 rows.
    for i in 0..5 {
        let line = format!("line {i}\r\n");
        t.vt_write(line.as_bytes());
    }
    let before = t.scrollbar().unwrap();
    t.scroll_viewport(ScrollViewport::Delta(-5));
    let after = t.scrollbar().unwrap();
    assert_eq!(before.offset, after.offset);
    assert_eq!(before.total, after.total);
    assert_eq!(before.len, after.len);
    // Total == len means "no scrollback to scroll into".
    assert_eq!(after.total, after.len);
}

#[test]
fn repeated_delta_walks_offset_toward_zero() {
    let mut t = make_terminal();
    let start = t.scrollbar().unwrap();
    for _ in 0..10 {
        t.scroll_viewport(ScrollViewport::Delta(-5));
    }
    let after = t.scrollbar().unwrap();
    let expected = start.offset.saturating_sub(50);
    assert_eq!(
        after.offset, expected,
        "10× Delta(-5) should subtract 50 (before={} after={})",
        start.offset, after.offset,
    );
}
