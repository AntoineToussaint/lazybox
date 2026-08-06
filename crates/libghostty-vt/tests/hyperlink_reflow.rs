//! Exercises the OSC 8 hyperlink + reflow path that aborted the TUI.
//!
//! A running lazybox died with:
//!
//! ```text
//! error(page_list): link dupe failed with capacity check err=error.OutOfMemory
//! thread 16652384 panic: reached unreachable code
//! ```
//!
//! ghostty's `PageList` capacity-checked a hyperlink copy with a single test
//! allocation of `uri.len + id.len`, but `PageEntry.dupe` performs the URI and
//! the explicit ID as two separate allocations, and the string allocator
//! rounds each up to its own 32-byte chunk — so the pair can need one more
//! chunk than the combined check reserved. The "this shouldn't fail" branch
//! then hit `unreachable`, which under `-Doptimize=ReleaseSafe` aborts the
//! process. Fixed upstream in ghostty d5c7e54a (ghostty-org/ghostty#13522),
//! which this crate's pin now includes.
//!
//! Honest scope: the abort needs a destination page whose string allocator is
//! *nearly* full, which depends on ghostty's internal page geometry — this
//! test drives the shape (many distinct hyperlinks, then reflow) rather than
//! pinning that exact boundary, so it is a smoke test for the path, not a
//! deterministic reproduction. It fails loudly if the path aborts again.

use libghostty_vt::{Terminal, TerminalOptions};

/// An OSC 8 hyperlink with an explicit id — the two-allocation case the
/// upstream capacity check got wrong. Long, unique URIs fill the page's
/// string allocator quickly.
fn hyperlink(i: usize) -> Vec<u8> {
    let uri = format!("https://example.com/a/fairly/long/path/segment/number/{i:08}?q={i:08}");
    let id = format!("lazybox-link-{i:08}");
    let mut out = Vec::new();
    out.extend_from_slice(format!("\x1b]8;id={id};{uri}\x1b\\").as_bytes());
    out.extend_from_slice(format!("link {i:06} text").as_bytes());
    out.extend_from_slice(b"\x1b]8;;\x1b\\\r\n");
    out
}

#[test]
fn hyperlink_heavy_output_survives_repeated_reflow() {
    let mut term = Terminal::new(TerminalOptions {
        cols: 120,
        rows: 32,
        max_scrollback_lines: 10_000,
        max_scrollback_bytes: Some(10_000 * 4096),
    })
    .expect("vt init");

    let mut stream = Vec::new();
    for i in 0..4_000 {
        stream.extend_from_slice(&hyperlink(i));
    }
    term.vt_write(&stream);

    // Reflow repeatedly: narrowing is what forces rows — and the hyperlinks
    // they carry — to be duped into fresh destination pages.
    for width in [80_u16, 40, 200, 61, 120] {
        term.resize(width, 32, 0, 0).expect("resize");
        // Keep writing across widths so pages keep being filled and split.
        for i in 0..200 {
            term.vt_write(&hyperlink(10_000 + i));
        }
    }

    // Survived without aborting; confirm the terminal is still coherent.
    let total = term.total_rows().expect("total_rows");
    assert!(total > 0, "terminal should still hold rows after reflow");
}
