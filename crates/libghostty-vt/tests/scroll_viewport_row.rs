//! `ScrollViewport::Row` — the absolute scroll verb (#1909).
//!
//! libghostty's C ABI has had four scroll verbs; the Rust binding exposed
//! three. The missing one is the only *absolute* position write, and its
//! own ABI doc states the property this file pins: the row space is the
//! same one `GHOSTTY_TERMINAL_DATA_SCROLLBAR` reports, so a position read
//! off the scrollbar round-trips cleanly.
//!
//! Why that matters above the binding: with only TOP/BOTTOM/DELTA a caller
//! can *read* where the viewport is and cannot *write* it, so every
//! "put the viewport back where the user was" has to be re-derived as a
//! delta from wherever the viewport currently sits — correct only while
//! nothing moved underneath it, which across a grid rebuild is exactly
//! what did.

use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::{Terminal, TerminalOptions};

/// A terminal holding `lines` rows of history above a 10-row screen.
fn scrolled(lines: usize) -> Terminal<'static, 'static> {
    let mut t = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 10,
        max_scrollback_lines: lines * 4,
        max_scrollback_bytes: Some(lines * 4 * 4096),
    })
    .expect("terminal");
    let mut payload = String::new();
    for i in 0..lines {
        payload.push_str(&format!("line-{i}\r\n"));
    }
    t.vt_write(payload.as_bytes());
    t
}

/// Every reachable offset round-trips. Each case starts from the bottom so
/// a move cannot accidentally be a no-op that happens to read back right.
#[test]
fn row_round_trips_with_the_scrollbar_offset() {
    let mut t = scrolled(200);
    let bar = t.scrollbar().expect("scrollbar");
    let max_offset = bar.total.saturating_sub(bar.len);
    assert!(max_offset > 0, "precondition: real scrollback: {bar:?}");

    for want in [0, 1, max_offset / 2, max_offset - 1, max_offset] {
        t.scroll_viewport(ScrollViewport::Bottom);
        t.scroll_viewport(ScrollViewport::Row(want as usize));
        assert_eq!(
            t.scrollbar().expect("scrollbar").offset,
            want,
            "Row({want}) must land on offset {want}"
        );
    }
}

/// A row past the live bottom clamps to the bottom instead of scrolling off
/// the end, so a restore computed against a grid that has since shrunk
/// degrades to "follow the tail" rather than to nonsense.
#[test]
fn row_past_the_bottom_clamps_to_the_active_area() {
    let mut t = scrolled(200);
    let bar = t.scrollbar().expect("scrollbar");
    let max_offset = bar.total.saturating_sub(bar.len);

    t.scroll_viewport(ScrollViewport::Top);
    t.scroll_viewport(ScrollViewport::Row(usize::MAX));
    assert_eq!(
        t.scrollbar().expect("scrollbar").offset,
        max_offset,
        "an out-of-range row clamps to the live bottom"
    );
}

/// With no scrollback there is nowhere to go: the viewport stays on the
/// active area rather than reporting a move it did not make.
#[test]
fn row_is_inert_without_scrollback() {
    let mut t = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 10,
        max_scrollback_lines: 0,
        max_scrollback_bytes: Some(0),
    })
    .expect("terminal");
    t.vt_write(b"one screen only");
    let before = t.scrollbar().expect("scrollbar").offset;
    t.scroll_viewport(ScrollViewport::Row(5));
    assert_eq!(t.scrollbar().expect("scrollbar").offset, before);
}

/// The pairing the client depends on, in the order the client performs it:
/// observe the parked position, let output stream in, **re-observe** (an
/// append moves the bottom, so the distance to it grows while libghostty
/// holds the pin on the same content), then — after something else moves
/// the viewport, which is the shape a grid rebuild produces — write the
/// absolute row back and land on exactly the content we started on.
///
/// Re-observing is the load-bearing step. Restoring the distance measured
/// *before* the write would land 15 rows low here: that is the difference
/// between re-deriving an anchor and re-imposing a stale one.
#[test]
fn a_position_read_before_a_write_can_be_restored_after_one() {
    let mut t = scrolled(200);
    t.scroll_viewport(ScrollViewport::Delta(-20));
    let parked_offset = t.scrollbar().expect("scrollbar").offset;

    let mut payload = String::new();
    for i in 0..15 {
        payload.push_str(&format!("new-{i}\r\n"));
    }
    t.vt_write(payload.as_bytes());

    let after_write = t.scrollbar().expect("scrollbar");
    assert_eq!(
        after_write.offset, parked_offset,
        "libghostty holds the pin on the same content across an append"
    );
    let rows_above_bottom = after_write.total - after_write.offset - after_write.len;

    // Something else moves the viewport — a fresh parser from a grid
    // rebuild sits at the live bottom.
    t.scroll_viewport(ScrollViewport::Bottom);

    let bar = t.scrollbar().expect("scrollbar");
    let target = bar.total.saturating_sub(bar.len) - rows_above_bottom;
    t.scroll_viewport(ScrollViewport::Row(target as usize));

    assert_eq!(
        t.scrollbar().expect("scrollbar").offset,
        parked_offset,
        "the absolute row restores the exact position the scrollbar reported"
    );
}
