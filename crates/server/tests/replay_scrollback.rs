//! Recovery-replay regression test for issue #200.
//!
//! A session's scrollback lives in the client-side libghostty-vt grid,
//! which on recovery is rebuilt purely from the daemon's per-terminal
//! replay ring. A live agent's redraw churn (spinners, progress lines,
//! repaints) inflates the byte stream far past the lines it ultimately
//! leaves in scrollback, so a screen-sized ring carries almost no real
//! history — a recovered session had nothing to scroll back through.
//!
//! This drives the full ring → snapshot → VT-feed path a reattaching
//! client takes, over a churn-heavy stream, and asserts the
//! reconstructed grid recovers a meaningful scrollback depth with the
//! sized ring while the old screen-sized ring does not.

use lazybox_server::pty::{REPLAY_RING_BYTES, ReplayRing};
use libghostty_vt::{Terminal, TerminalOptions};

const COLS: u16 = 120;
const ROWS: u16 = 32;
/// Mirrors `max_scrollback` in the TUI client's `TerminalVt`.
const MAX_SCROLLBACK: usize = 10_000;

/// A churn-heavy PTY stream: `lines` rows of committed output, each
/// preceded by a burst of carriage-return redraws on the *current* line
/// (a spinner / progress bar). The redraws balloon the raw byte count
/// without adding scrollback rows — exactly the shape that makes a
/// screen-sized ring useless for history.
fn churned_stream(lines: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        for _ in 0..30 {
            out.extend_from_slice(b"\r\x1b[33m[");
            out.extend_from_slice(&[b'#'; 30]);
            out.extend_from_slice(b"]\x1b[0m");
        }
        out.extend_from_slice(format!("\rline {i:05} done").as_bytes());
        out.extend_from_slice(&[b'.'; 40]);
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Push `stream` through a ring of `cap` bytes in PTY-sized chunks (as
/// the daemon reader thread does), then reconstruct a VT from the ring
/// snapshot exactly as a recovering client does. Returns the scrollback
/// depth of the recovered grid.
fn recovered_scrollback(cap: usize, stream: &[u8]) -> usize {
    let mut ring = ReplayRing::with_capacity(cap);
    for chunk in stream.chunks(8192) {
        ring.push(chunk);
    }

    let mut term = Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: MAX_SCROLLBACK,
    })
    .expect("vt init");
    term.vt_write(&ring.snapshot());
    term.scrollback_rows().expect("scrollback_rows")
}

#[test]
fn recovered_session_retains_meaningful_scrollback() {
    // ~1200 committed lines, each ~1.2 KiB on the wire after churn:
    // the whole stream fits the sized ring but is ~24x the old one.
    let stream = churned_stream(1200);

    let recovered = recovered_scrollback(REPLAY_RING_BYTES, &stream);
    let screen_sized = recovered_scrollback(64 * 1024, &stream);

    // The old screen-sized ring reconstructs barely more than the
    // visible screen — the redraw churn crowded out the history.
    assert!(
        screen_sized < ROWS as usize * 3,
        "screen-sized ring should recover almost no history, got {screen_sized} rows"
    );

    // The sized ring recovers many screens of real scrollback.
    assert!(
        recovered >= ROWS as usize * 8,
        "sized ring should recover many screens of scrollback, got {recovered} rows"
    );
    assert!(
        recovered > screen_sized * 4,
        "the larger ring must deepen recovered scrollback \
         (screen-sized ring gave {screen_sized}, sized ring gave {recovered})"
    );
}
