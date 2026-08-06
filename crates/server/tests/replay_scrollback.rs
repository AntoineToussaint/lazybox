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
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::screen::Selection;
use libghostty_vt::terminal::{Point, PointCoordinate};
use libghostty_vt::{Terminal, TerminalOptions};

const COLS: u16 = 120;
const ROWS: u16 = 32;
/// Mirrors `max_scrollback_lines` in the TUI client's `TerminalVt`, which
/// now takes the configured line depth directly.
const MAX_SCROLLBACK: usize = lazybox_config::DEFAULT_SCROLLBACK_LINES as usize;

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
        max_scrollback_lines: MAX_SCROLLBACK,
        max_scrollback_bytes: None,
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

/// Fixed 104-byte line: an 11-byte SGR introducer `\x1b[38;5;CCCm`
/// (always 3 colour digits so the length never varies), an 87-byte
/// body carrying a unique `line NNNNNN` marker, then `\x1b[0m\r\n`.
/// The fixed width lets the fidelity test place the ring's oldest
/// retained byte deterministically — see `truncated_replay_is_grid_faithful`.
const FIDELITY_LINE_BYTES: usize = 104;

/// Byte length of the fixed-width SGR introducer each fidelity line opens
/// with (`\x1b[38;5;CCCm`). The truncation must land *inside* this span for
/// the raw replay to begin mid-escape.
const SGR_INTRODUCER_BYTES: usize = "\x1b[38;5;000m".len();

fn colored_line(i: usize) -> Vec<u8> {
    let color = 16 + (i % 216); // 016..=231, zero-padded to a fixed 3 digits
    let body = format!("line {i:06} ");
    let mut out = Vec::with_capacity(FIDELITY_LINE_BYTES);
    out.extend_from_slice(format!("\x1b[38;5;{color:03}m").as_bytes());
    out.extend_from_slice(body.as_bytes());
    out.resize(FIDELITY_LINE_BYTES - b"\x1b[0m\r\n".len(), b'=');
    out.extend_from_slice(b"\x1b[0m\r\n");
    debug_assert_eq!(out.len(), FIDELITY_LINE_BYTES);
    out
}

/// Feed `stream` into a fresh VT (deep scrollback so the ring, not the
/// grid, bounds recovered depth) and read back every grid row — scrollback
/// included — as plain text via a full-screen selection, exactly the path
/// the client uses for cross-scrollback copy.
fn reconstructed_rows(stream: &[u8]) -> Vec<String> {
    // Deep scrollback budget so the ring (not the grid) bounds recovered
    // depth. The budget is spent on the retained pages, not reserved, so a
    // generous ceiling is cheap. (Was a 64 MiB *byte* ceiling before the
    // limit became line-denominated.)
    let mut term = Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback_lines: 64 * 1024 * 1024,
        max_scrollback_bytes: None,
    })
    .expect("vt init");
    term.vt_write(stream);

    let total = term.total_rows().expect("total_rows");
    if total == 0 {
        return Vec::new();
    }
    let start = term
        .grid_ref(Point::Screen(PointCoordinate { x: 0, y: 0 }))
        .expect("start grid_ref");
    let end = term
        .grid_ref(Point::Screen(PointCoordinate {
            x: COLS - 1,
            y: (total - 1) as u32,
        }))
        .expect("end grid_ref");
    let mut formatter = Formatter::new(
        &term,
        FormatterOptions {
            format: Format::Plain,
            trim: true,
            unwrap: false,
            selection: Some(Selection {
                start,
                end,
                rectangle: false,
            }),
        },
    )
    .expect("formatter");
    let bytes = formatter.format_alloc(None).expect("format");
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect()
}

/// Fidelity regression for #498. A churn-heavy row-count test (above)
/// proves the ring recovers *depth*; it does not prove the recovered
/// scrollback is *correct*. The ring drops its oldest bytes on a pure
/// byte boundary, so once a session exceeds the ring capacity the replay
/// can begin in the middle of an escape sequence — that partial SGR
/// introducer is then parsed as ground-state text and corrupts the first
/// reconstructed rows.
///
/// This streams >2 MiB of uniquely-labelled, SGR-coloured lines through a
/// full ring, positioned so the oldest retained byte lands *inside* an
/// SGR introducer, and asserts:
///   - the raw (unguarded) snapshot really is corrupt at its head — the
///     leaked introducer bytes surface as a bogus leading row, so the test
///     is exercising the divergence path, not a benign boundary;
///   - the boundary-guarded snapshot reconstructs a grid whose rows are a
///     byte-faithful contiguous tail of the true live history.
#[test]
fn truncated_replay_is_grid_faithful() {
    // The ring keeps its last REPLAY_RING_BYTES, so its oldest retained byte
    // sits at stream offset `total - REPLAY_RING_BYTES`; within a fixed-width
    // line that is `(-REPLAY_RING_BYTES) mod FIDELITY_LINE_BYTES`, independent
    // of the line count. This test only bites when that offset lands inside
    // the SGR introducer (a mid-escape start) — assert the precondition up
    // front so a future retune of either constant fails here, loudly and with
    // a fix, rather than silently reconstructing from a benign boundary.
    let landing =
        (FIDELITY_LINE_BYTES - (REPLAY_RING_BYTES % FIDELITY_LINE_BYTES)) % FIDELITY_LINE_BYTES;
    assert!(
        (1..SGR_INTRODUCER_BYTES).contains(&landing),
        "test precondition: REPLAY_RING_BYTES={REPLAY_RING_BYTES} against a \
         {FIDELITY_LINE_BYTES}-byte line truncates at line offset {landing}, \
         which is not inside the {SGR_INTRODUCER_BYTES}-byte SGR introducer — \
         adjust FIDELITY_LINE_BYTES so truncation starts mid-escape"
    );

    // Enough lines to overrun the ring with a comfortable margin, derived
    // from the ring size so this holds if REPLAY_RING_BYTES changes.
    let ring_lines = REPLAY_RING_BYTES / FIDELITY_LINE_BYTES;
    let lines = ring_lines + 5_000;
    let mut stream = Vec::new();
    for i in 0..lines {
        stream.extend_from_slice(&colored_line(i));
    }
    assert!(stream.len() > REPLAY_RING_BYTES);

    let mut ring = ReplayRing::with_capacity(REPLAY_RING_BYTES);
    for chunk in stream.chunks(8192) {
        ring.push(chunk);
    }
    assert!(
        !ring.is_complete(),
        "the stream must exceed the ring so truncation is in play"
    );

    // The true history a live client scrolled through: every line, in order.
    let live = reconstructed_rows(&stream);
    let live_content: Vec<&String> = live.iter().filter(|l| l.starts_with("line ")).collect();

    // Raw snapshot: replay begins mid-introducer, so the leaked bytes
    // (`Cm`…) render as a leading row that is not any real history line.
    let raw = reconstructed_rows(&ring.snapshot());
    assert!(
        !raw.is_empty() && !raw[0].starts_with("line "),
        "the raw snapshot must start mid-sequence and corrupt its first row, \
         else this test is not exercising the truncation path (got {:?})",
        raw.first()
    );

    // Guarded snapshot: replay starts on a clean line boundary, so every
    // reconstructed row is a real history line...
    let guarded = reconstructed_rows(&ring.replay_snapshot());
    assert!(
        guarded.iter().all(|l| l.starts_with("line ")),
        "every guarded row must be a clean history line; found {:?}",
        guarded.iter().find(|l| !l.starts_with("line "))
    );
    // The guard drops at most the single partial leading line, so recovered
    // depth stays within one line of everything the ring retained.
    assert!(
        guarded.len() >= ring_lines - 1,
        "the guard must not gut recovered depth: got {} rows, ring held ~{}",
        guarded.len(),
        ring_lines
    );

    // ...and those rows are a byte-faithful contiguous tail of the true
    // history — same content, same order, no corruption at the head.
    let head = &guarded[0];
    let idx = live_content
        .iter()
        .position(|l| *l == head)
        .expect("the guarded head row must be a real history line");
    let live_tail: Vec<&String> = live_content[idx..].to_vec();
    let guarded_refs: Vec<&String> = guarded.iter().collect();
    assert_eq!(
        live_tail, guarded_refs,
        "reconstructed scrollback must match the true history line-for-line"
    );
}
