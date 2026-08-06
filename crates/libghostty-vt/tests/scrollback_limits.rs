//! Guards the scrollback limits actually taking effect.
//!
//! Upstream replaced the creation-time `max_scrollback` (which capped page
//! *memory* despite its "number of lines" documentation) with two runtime
//! options — a line limit and a byte limit — applied together, whichever is
//! reached first. A fresh terminal ships with a default byte limit, so a
//! caller that sets only the line limit silently gets neither: measured on
//! ghostty d5c7e54a, a 120-col terminal retained ~613 rows whether it asked
//! for 1_000 lines or 67_000_000.
//!
//! That failure is invisible — no error, no warning, just a fraction of the
//! history the user configured — so it gets a test rather than a comment.

use libghostty_vt::{Terminal, TerminalOptions};

const COLS: u16 = 120;
const ROWS: u16 = 32;

/// Feed `lines` numbered rows through a VT built with the given limits and
/// report how many rows of scrollback survived.
fn retained_rows(max_lines: usize, max_bytes: Option<usize>, lines: usize) -> usize {
    let mut stream = Vec::new();
    for i in 0..lines {
        stream.extend_from_slice(format!("line {i:06} ").as_bytes());
        stream.extend_from_slice(&[b'='; 90]);
        stream.extend_from_slice(b"\r\n");
    }

    let mut term = Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback_lines: max_lines,
        max_scrollback_bytes: max_bytes,
    })
    .expect("vt init");
    term.vt_write(&stream);
    term.scrollback_rows().expect("scrollback_rows")
}

/// A deep line limit paired with a matching byte ceiling retains deep
/// history. This is the configuration lazybox's client VT uses.
#[test]
fn line_limit_with_a_byte_ceiling_retains_deep_history() {
    let retained = retained_rows(5_000, Some(5_000 * 4096), 4_000);
    assert!(
        retained >= 3_000,
        "a 5_000-line limit should retain most of a 4_000-line stream, got {retained}"
    );
}

/// Removing the byte limit entirely (`None`) also works — lines become the
/// only bound.
#[test]
fn line_limit_alone_retains_deep_history_when_the_byte_limit_is_removed() {
    let retained = retained_rows(5_000, None, 4_000);
    assert!(
        retained >= 3_000,
        "with no byte limit, a 5_000-line limit should retain most of a \
         4_000-line stream, got {retained}"
    );
}

/// The regression this file exists for: leaving the byte limit at the
/// terminal's default throttles a deep line request down to a few hundred
/// rows. If a future ghostty drops that default, this test starts failing
/// and the byte-ceiling plumbing can be simplified — which is worth knowing.
#[test]
fn the_default_byte_limit_still_caps_a_deep_line_request() {
    let capped = retained_rows(50_000, Some(64 * 1024), 4_000);
    let uncapped = retained_rows(50_000, None, 4_000);
    assert!(
        capped < uncapped,
        "a small byte ceiling must bind before the line limit \
         (capped={capped}, uncapped={uncapped})"
    );
}

/// The limit is honored in the other direction too: a shallow line limit
/// prunes even when the byte ceiling is generous.
#[test]
fn a_shallow_line_limit_prunes_regardless_of_the_byte_ceiling() {
    let retained = retained_rows(200, Some(64 * 1024 * 1024), 4_000);
    assert!(
        retained < 1_000,
        "a 200-line limit must prune a 4_000-line stream, got {retained}"
    );
}
