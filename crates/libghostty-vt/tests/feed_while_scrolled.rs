//! A scrolled-up viewport must stay anchored to the content the user is
//! reading while new output streams in below (#393).
//!
//! This is the invariant that makes deep scrollback *usable* on a live
//! agent: the whole point of the capture-pane fetch is to let the user
//! read history while the agent keeps producing output. If a write
//! snapped the viewport back to the live bottom, every streamed chunk
//! would yank the user out of scrollback and "scrolling is broken"
//! would be back regardless of how deep the history is.

use libghostty_vt::terminal::ScrollViewport;
use libghostty_vt::{Terminal, TerminalOptions};

#[test]
fn viewport_stays_anchored_while_output_streams_in() {
    let mut t = Terminal::new(TerminalOptions {
        cols: 80,
        rows: 10,
        max_scrollback: 1000,
    })
    .expect("terminal");
    for i in 0..100 {
        t.vt_write(format!("line-{i}\r\n").as_bytes());
    }

    t.scroll_viewport(ScrollViewport::Delta(-20));
    let scrolled = t.scrollbar().unwrap();
    assert!(scrolled.offset < scrolled.total - scrolled.len);

    for i in 100..120 {
        t.vt_write(format!("line-{i}\r\n").as_bytes());
    }
    let after = t.scrollbar().unwrap();

    assert_eq!(
        after.offset, scrolled.offset,
        "a write while scrolled up must not move the viewport — offset \
         following the bottom means snap-back on every live chunk"
    );
    assert_eq!(
        after.total,
        scrolled.total + 20,
        "the new lines still landed in history below the viewport"
    );
}
