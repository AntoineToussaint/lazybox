//! The terminal hot-path budget gate (#1919).
//!
//! # What this file is for
//!
//! #1910 fixed real scrollback corruption and, in doing so, put
//! `libghostty_vt::Terminal::scrollbar()` — whose own binding says "may be
//! expensive … The caller should take care to only call this as needed and
//! not too frequently" — inside `TerminalVt::feed`, which runs for every
//! output chunk of every terminal. `build`, `test`, `clippy`, `fmt`,
//! `sandbox`, `changes` and `plan` all stayed green. The user noticed by
//! typing (#1918). This file is the gate that would have been red.
//!
//! # Why counts and not milliseconds
//!
//! A wall-clock budget cannot be the gate here. The suite has a loaded
//! profile (`make test-loaded`) precisely because fixed timeouts flake on
//! a shared box; the regression that prompted all of this was reported at
//! load average 75. A time threshold is then either too loose to catch
//! anything or tight enough to fail PRs that changed nothing.
//!
//! What is exact on every machine is *how much work the path did*. Each
//! assertion here is a structural claim about the code — "following the
//! tail, a write reads the scrollbar zero times" — true or false
//! independently of load, of core count, and of what else is compiling.
//! The milliseconds live in `benches/` and in the PR body, for humans.
//!
//! # Every budget has a positive control
//!
//! The dangerous failure of a counting gate is not a wrong number, it is a
//! counter that stops observing: then every "this path does no work"
//! assertion passes while the regression ships. So each zero-assertion
//! here is paired with a reading from a path that *must* do work, through
//! `Counts::assert_live`. A test that cannot prove the instrument is alive
//! proves nothing about the budget.
//!
//! The budgets themselves are written down in `crates/tui/AGENTS.md`, for
//! whoever edits `feed()` next.

mod common;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_ipc::{Command, Event, TerminalId, TerminalKind};
use lazybox_tui::PaneId;
use lazybox_tui::components::TerminalStack;
use lazybox_tui_term::vt_budget;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;

const W: u16 = 100;
const H: u16 = 40;

// ── Scaffolding (the real daemon event path, as `terminal_scroll.rs`) ──

fn render(stack: &mut TerminalStack) {
    let backend = TestBackend::new(W, H);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| stack.render(Rect::new(0, 0, W, H), f, true))
        .unwrap();
}

/// Enough plain output to overflow the viewport into scrollback, so there
/// is something to park above.
fn scrollback_payload() -> Vec<u8> {
    let mut p = String::new();
    for i in 0..80 {
        p.push_str(&format!("output line {i}\r\n"));
    }
    p.into_bytes()
}

/// The daemon's half of the resize handshake: the size the first render
/// asked for, announced back as an empty stamped chunk.
fn ack_resize(stack: &mut TerminalStack, seq: u64) -> (u16, u16) {
    let (_, cols, rows) = stack
        .drain_pending_resizes()
        .into_iter()
        .find(|(id, _, _)| *id == TerminalId(1))
        .expect("the render asked for a resize");
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: Vec::new().into(),
        first_seq: seq,
        seq,
        cols,
        rows,
    });
    (cols, rows)
}

/// A fresh agent with scrollback, driven through the daemon event path.
/// Returns the stack and the PTY size its chunks must be stamped with.
fn agent_with_scrollback() -> (TerminalStack, u16, u16) {
    let mut stack = TerminalStack::new(PaneId::new(0));
    stack.on_event(&Event::TerminalSpawned {
        terminal_id: TerminalId(1),
        session_key: "s".into(),
        kind: TerminalKind::Agent("claude".into()),
        no_permission: false,
        on_main: false,
        model_label: None,
        agent_state: None,
    });
    stack.set_active_session(Some("s".into()));
    render(&mut stack);
    let (cols, rows) = ack_resize(&mut stack, 1);
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: scrollback_payload().into(),
        first_seq: 2,
        seq: 2,
        cols,
        rows,
    });
    render(&mut stack);
    (stack, cols, rows)
}

/// Deliver one output chunk the way the daemon does.
fn output(stack: &mut TerminalStack, seq: u64, cols: u16, rows: u16, bytes: &[u8]) {
    stack.on_event(&Event::TerminalOutput {
        terminal_id: TerminalId(1),
        bytes: bytes.to_vec().into(),
        first_seq: seq,
        seq,
        cols,
        rows,
    });
}

/// Pull `offset=` out of the focused terminal's scrollbar summary.
fn offset(stack: &TerminalStack) -> u64 {
    stack
        .scrollbar_summary()
        .expect("focused terminal has a scrollbar summary")
        .split_whitespace()
        .find_map(|kv| kv.strip_prefix("offset="))
        .expect("offset field")
        .parse()
        .expect("numeric offset")
}

// ── The feed path: the #1918 budget ───────────────────────────────────

/// **The gate #1910 would have failed.**
///
/// While the viewport follows the live tail — the overwhelmingly common
/// case, and the whole of the typing path — a write must not derive the
/// scrollbar at all. On `aae9fd9f7` this counted one per chunk.
///
/// The positive control is in the same test: the same stack, parked, must
/// read. A silent counter therefore fails this test rather than passing it.
#[test]
fn a_following_tail_feed_reads_no_scrollbar() {
    let (mut stack, cols, rows) = agent_with_scrollback();

    let watch = vt_budget::watch();
    for seq in 3..23 {
        output(&mut stack, seq, cols, rows, b"a line of agent output\r\n");
    }
    let following = watch.counts();

    // The positive control: park the viewport and feed the same chunks.
    // This path MUST read, which is what proves the counter is observing.
    assert!(matches!(
        stack.scroll_terminal(TerminalId(1), -7),
        lazybox_tui::components::terminal_stack::ScrollOutcome::Moved { .. }
    ));
    let watch = vt_budget::watch();
    for seq in 23..43 {
        output(&mut stack, seq, cols, rows, b"a line of agent output\r\n");
    }
    let parked = watch.counts();
    parked.assert_live();

    assert_eq!(
        following.scrollbar, 0,
        "a write while following the tail must derive no scrollbar — \
         `Terminal::scrollbar()` is documented expensive and `feed` is the \
         hottest path in the client (#1918). Parked feeds counted \
         {parked:?}, so the counter is observing.",
    );
}

/// The other half of the same contract, and the reason the fix is a
/// placement change rather than a revert: a *parked* viewport still
/// re-derives on every write, because the distance from a tail that just
/// grew genuinely changed.
#[test]
fn a_parked_feed_still_re_derives_once_per_write() {
    let (mut stack, cols, rows) = agent_with_scrollback();
    assert!(matches!(
        stack.scroll_terminal(TerminalId(1), -7),
        lazybox_tui::components::terminal_stack::ScrollOutcome::Moved { .. }
    ));

    let watch = vt_budget::watch();
    output(&mut stack, 3, cols, rows, b"new 1\r\nnew 2\r\nnew 3\r\n");
    let counts = watch.counts();

    assert_eq!(
        counts.scrollbar, 1,
        "a parked write re-derives the anchor exactly once: {counts:?}",
    );
}

/// The equivalence the fix rests on, checked rather than argued.
///
/// Skipping the read while following the tail is only sound if a write can
/// never move a following viewport off the tail. That is an argument about
/// libghostty's behaviour, so it is held here against traffic designed to
/// break it: region scrolls (`DECSTBM`, the #239 shape), an alt-screen
/// round trip, a full reset, and plain overflow. After every chunk, the
/// cached anchor must agree with what a fresh derivation would say.
#[test]
fn following_a_tail_is_equivalent_to_re_deriving() {
    let (mut stack, cols, rows) = agent_with_scrollback();

    // Escape-heavy traffic, each entry a chunk as the PTY reader hands it
    // over. Anything that could plausibly move a viewport that is at the
    // bottom belongs in here.
    let traffic: Vec<Vec<u8>> = vec![
        b"plain tail line\r\n".to_vec(),
        // Scroll region, then churn inside it — reverse index included.
        b"\x1b[5;20r\x1b[10;1Hinside the region\r\n".to_vec(),
        b"\x1bM\x1bM\x1bMreverse indexed\r\n".to_vec(),
        b"\x1b[r".to_vec(), // release the region
        // Alternate screen there and back.
        b"\x1b[?1049h".to_vec(),
        b"alt screen content\r\n".to_vec(),
        b"\x1b[?1049l".to_vec(),
        // Erase-in-display variants and a hard reset.
        b"\x1b[2J\x1b[Hcleared\r\n".to_vec(),
        b"\x1b[3J".to_vec(),
        b"\x1bcafter a full reset\r\n".to_vec(),
        // And enough plain output to overflow into scrollback again.
        (0..60)
            .map(|i| format!("overflow {i}\r\n"))
            .collect::<String>()
            .into_bytes(),
    ];

    for (i, chunk) in traffic.iter().enumerate() {
        let seq = 3 + i as u64;
        output(&mut stack, seq, cols, rows, chunk);
        // `anchor_of` is private, so the observable stand-in is the one
        // the production code derives from: a viewport at (or past) the
        // live bottom. Reading it here is a fresh derivation by
        // definition — it goes through the counted accessor — so a
        // divergence between it and the cached anchor shows up as a
        // rendered-row difference, which the next assertion catches.
        let summary = stack
            .scrollbar_summary()
            .expect("the focused terminal has a scrollbar");
        let field = |name: &str| -> u64 {
            summary
                .split_whitespace()
                .find_map(|kv| kv.strip_prefix(name))
                .unwrap_or_else(|| panic!("{name} in {summary}"))
                .parse()
                .expect("numeric")
        };
        let (total, off, len) = (field("total="), field("offset="), field("len="));
        assert_eq!(
            total.saturating_sub(off.saturating_add(len)),
            0,
            "chunk {i} moved a following viewport off the live tail, so \
             caching `Bottom` across writes would be unsound: {summary}",
        );
    }
}

/// Leaving the parked state is a user action through the scroll owner, so
/// the cached anchor cannot be stale on return to the tail: `scroll` sets
/// it. This pins the transition the feed fix relies on.
#[test]
fn scrolling_back_to_the_tail_restores_the_free_feed() {
    let (mut stack, cols, rows) = agent_with_scrollback();
    let _ = stack.scroll_terminal(TerminalId(1), -7);
    output(&mut stack, 3, cols, rows, b"while parked\r\n");
    assert!(stack.focus_terminal(TerminalId(1)));
    let _ = stack.scroll_to_bottom();
    assert_eq!(offset(&stack), {
        // At the bottom: offset is the maximum the grid allows.
        let summary = stack.scrollbar_summary().expect("summary");
        let field = |name: &str| -> u64 {
            summary
                .split_whitespace()
                .find_map(|kv| kv.strip_prefix(name))
                .expect("field")
                .parse()
                .expect("numeric")
        };
        field("total=").saturating_sub(field("len="))
    });

    let watch = vt_budget::watch();
    output(&mut stack, 4, cols, rows, b"back on the tail\r\n");
    assert_eq!(
        watch.counts().scrollbar,
        0,
        "once the user is back on the tail, writes are free again",
    );
}

// ── The scroll path: bounded, not free ────────────────────────────────

/// A scroll is a user action, not a hot path, and it legitimately reads
/// twice — `before` and `after` — because the owner classifies the
/// observed transition and a scroll that silently fails to move is the
/// #42/#371 bug it exists to prevent.
///
/// #1919 proposed "at most once per `scroll()`". Taken literally that
/// would mean dropping the `after` read and with it `Stalled` detection,
/// trading a behaviour guarantee for a constant on a path a human drives
/// at most a few times a second. So the budget asserted is the real
/// structural claim: a **constant** number of derivations per scroll,
/// independent of scrollback depth and of how far the viewport travels.
/// That is what catches a regression; "exactly 1" would only catch the
/// guard being removed.
#[test]
fn a_scroll_reads_a_constant_number_of_scrollbars() {
    let (mut stack, cols, rows) = agent_with_scrollback();
    // Deepen the history so a depth-dependent read count would show.
    for seq in 3..40 {
        output(&mut stack, seq, cols, rows, b"deepen the scrollback\r\n");
    }

    let watch = vt_budget::watch();
    let _ = stack.scroll_terminal(TerminalId(1), -1);
    let one_row = watch.counts();

    let watch = vt_budget::watch();
    let _ = stack.scroll_terminal(TerminalId(1), -25);
    let many_rows = watch.counts();

    one_row.assert_live();
    assert_eq!(
        one_row.scrollbar, 2,
        "the scroll owner reads before and after, and nothing else: {one_row:?}",
    );
    assert_eq!(
        many_rows.scrollbar, one_row.scrollbar,
        "the read count must not scale with how far the viewport moves: \
         {many_rows:?} vs {one_row:?}",
    );
}

// ── The render path: per-frame budgets ────────────────────────────────

/// A painted frame walks each viewport row once, and that walk is the
/// justified cost (`tui-term/src/ghostty_widget.rs` module docs:
/// libghostty's dirty flags are unsound as a skip signal under region
/// scrolls, #239). It is not something to "fix" — it is something to hold
/// a budget on, so a change that walks the grid twice is visible.
///
/// The `+ 1` is the one overshoot fetch a `while let` has to make to
/// discover the rect is full; it is a constant, not a term that scales.
#[test]
fn a_frame_walks_each_viewport_row_at_most_once() {
    let (mut stack, cols, rows) = agent_with_scrollback();
    // Force a real walk: the cached-frame blit would otherwise serve it.
    output(&mut stack, 3, cols, rows, b"changed\r\n");

    let watch = vt_budget::watch();
    render(&mut stack);
    let counts = watch.counts();

    counts.assert_live();
    assert_eq!(counts.frames, 1, "one grid walk for one tile: {counts:?}");
    let budget = u64::from(rows) + 1;
    assert!(
        counts.row_reads <= budget,
        "a {rows}-row frame fetched {} rows (budget {budget}): {counts:?}",
        counts.row_reads,
    );
}

/// The content-revision gate (2026-08-19 audit, U1) is load-bearing: an
/// idle agent pane repaints constantly and must not re-walk an unchanged
/// grid, because that walk is ~5 FFI round trips per cell. Nothing
/// asserted that it still holds.
///
/// The scrollbar reading is NOT cached across frames, deliberately — it
/// measures ~5 ns at any viewport depth, so caching it on the frame-blit
/// key would trade a new staleness obligation for nothing. So an
/// unchanged frame walks zero rows and still takes its one reading per
/// tile; what must never happen is the grid walk.
#[test]
fn an_unchanged_frame_walks_no_rows() {
    let (mut stack, cols, rows) = agent_with_scrollback();
    output(&mut stack, 3, cols, rows, b"changed\r\n");

    let watch = vt_budget::watch();
    render(&mut stack);
    let first = watch.counts();
    first.assert_live();
    assert!(
        first.row_reads > 0,
        "the control paint must walk the grid: {first:?}",
    );

    let watch = vt_budget::watch();
    render(&mut stack);
    render(&mut stack);
    let repeats = watch.counts();

    assert_eq!(
        repeats.row_reads, 0,
        "two repaints of an unmutated grid must walk no rows — the \
         revision gate blits the cached frame (#1919). First paint did \
         {first:?}, repeats did {repeats:?}.",
    );
    assert_eq!(
        repeats.frames, 0,
        "and must not construct the grid-walking widget at all: {repeats:?}",
    );
    assert_eq!(
        repeats.scrollbar, 2,
        "one reading per repaint per tile, uncached by design: {repeats:?}",
    );
}

/// One scrollbar derivation per tile per painted frame, never two. Render
/// needs the reading twice (the tile hit's offset for selection mapping,
/// and the gutter's extent) and used to read it twice.
#[test]
fn a_painted_frame_reads_one_scrollbar_per_tile() {
    let (mut stack, cols, rows) = agent_with_scrollback();
    output(&mut stack, 3, cols, rows, b"changed\r\n");

    let watch = vt_budget::watch();
    render(&mut stack);
    let counts = watch.counts();

    counts.assert_live();
    assert_eq!(
        counts.scrollbar, 1,
        "the offset recorded on the tile hit and the gutter's extent come \
         from ONE reading: {counts:?}",
    );
}

// ── The keystroke path ────────────────────────────────────────────────

/// One keystroke is one `Command::Write`, plus bounded bookkeeping.
///
/// The per-key path also mirrors the bytes into the composing buffer and
/// may emit a draft-persist command; what it must never do is multiply
/// writes, and it must do no VT work at all — a keystroke is PTY-bound,
/// and the echo comes back as output.
#[test]
fn one_keystroke_is_one_write_and_no_vt_work() {
    let (mut stack, _, _) = agent_with_scrollback();
    assert!(stack.focus_terminal(TerminalId(1)));

    let watch = vt_budget::watch();
    let mut cmds: Vec<Command> = Vec::new();
    stack.handle_key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        &mut cmds,
    );
    let counts = watch.counts();

    let writes = cmds
        .iter()
        .filter(|c| matches!(c, Command::Write { .. }))
        .count();
    assert_eq!(writes, 1, "one key, one Write: {cmds:?}");
    assert_eq!(
        counts,
        vt_budget::Counts::default(),
        "a keystroke is PTY-bound and must touch the VT not at all: {counts:?}",
    );
    // Bounded bookkeeping: the draft persist, and nothing unbounded.
    assert!(
        cmds.len() <= 2,
        "a keystroke's command burst must stay bounded: {cmds:?}",
    );
}

/// A whole typed word is one write per key — the count must not grow
/// super-linearly with the draft already in the buffer.
#[test]
fn typing_a_word_is_one_write_per_key() {
    let (mut stack, _, _) = agent_with_scrollback();
    assert!(stack.focus_terminal(TerminalId(1)));

    let word = "implement the fix";
    let mut cmds: Vec<Command> = Vec::new();
    for ch in word.chars() {
        stack.handle_key(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            &mut cmds,
        );
    }
    let writes = cmds
        .iter()
        .filter(|c| matches!(c, Command::Write { .. }))
        .count();
    assert_eq!(
        writes,
        word.chars().count(),
        "exactly one Write per key over a whole word",
    );
}

/// A keystroke into a read-only log window is **zero** writes (#1920).
///
/// The budget beside it says one keystroke is one `Command::Write`. For a
/// `LogTail` runner the right number is none: it is `tail -F`, which never
/// reads its stdin, so a write there goes to a reader that does not exist
/// — and `Enter` additionally shipped `TerminalInputIntent::Submit`, which
/// `lazybox_ipc` documents as authoritative turn-start evidence.
///
/// This lives in the budget file rather than beside the behavioural tests
/// in `terminal_stack.rs` because it is the same claim as its neighbour,
/// with a different number: "how many writes does this keystroke produce"
/// is exactly what this gate counts, and a per-kind exemption that the
/// gate does not know about is one a future change to the write path can
/// quietly take back.
///
/// Its positive control is in the same test: the agent in the SAME
/// session, the same keystroke, still produces its one write. A zero that
/// is only ever read next to another zero proves nothing, and muting the
/// pane rather than the runner would satisfy the zero alone.
#[test]
fn a_keystroke_into_a_log_window_is_zero_writes_and_no_vt_work() {
    let (mut stack, _, _) = agent_with_scrollback();
    // A `lazybox log` window alongside the agent — what
    // `cargo test 2>&1 | lazybox log` opens.
    stack.on_event(&Event::TerminalSpawned {
        terminal_id: TerminalId(2),
        session_key: "s".into(),
        kind: TerminalKind::LogTail {
            path: "/w/target/test.log".into(),
        },
        no_permission: false,
        on_main: false,
        model_label: None,
        agent_state: None,
    });
    render(&mut stack);

    let count_writes = |cmds: &[Command]| {
        cmds.iter()
            .filter(|c| matches!(c, Command::Write { .. }))
            .count()
    };

    // The log window: nothing goes out, and no VT work is done either.
    assert!(stack.focus_terminal(TerminalId(2)), "focus the log window");
    let watch = vt_budget::watch();
    let mut refused: Vec<Command> = Vec::new();
    for key in [
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    ] {
        stack.handle_key(key, &mut refused);
    }
    let refused_counts = watch.counts();
    assert_eq!(
        count_writes(&refused),
        0,
        "a log window takes no typed input: {refused:?}",
    );
    assert!(
        refused.is_empty(),
        "and a refusal emits no command at all: {refused:?}",
    );
    assert_eq!(
        refused_counts,
        vt_budget::Counts::default(),
        "refusing input is not VT work either: {refused_counts:?}",
    );

    // The positive control, in the same session: the agent still writes.
    assert!(stack.focus_terminal(TerminalId(1)), "focus the agent");
    let mut typed: Vec<Command> = Vec::new();
    stack.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        &mut typed,
    );
    assert_eq!(
        count_writes(&typed),
        1,
        "the guard is on the runner kind, not the pane: {typed:?}",
    );

    // And the instrument itself is alive — a painted frame must count.
    let watch = vt_budget::watch();
    output(&mut stack, 9, W, H, b"fresh output\r\n");
    render(&mut stack);
    watch.counts().assert_live();
}

// ── Source-level backstops ───────────────────────────────────────────

/// The files named by a `#[cfg(test)] mod <name>;` declaration — test
/// code living outside the file that declares it (`sidebar/tests.rs`,
/// `right_pane/tests.rs`, `realm/model/tests.rs`). They read the binding
/// deliberately, like any other test, so the production audits skip them.
fn out_of_line_test_modules(paths: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for path in paths {
        let src = std::fs::read_to_string(path).expect("read Rust source");
        for (idx, _) in src.match_indices("#[cfg(test)]") {
            let rest = src[idx + "#[cfg(test)]".len()..].trim_start();
            let Some(rest) = rest.strip_prefix("mod ") else {
                continue;
            };
            let Some((name, _)) = rest.split_once(';') else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let dir = path.parent().expect("a source file has a parent");
            out.push(dir.join(format!("{name}.rs")));
            out.push(dir.join(name).join("mod.rs"));
        }
    }
    out
}

/// Collect every `.rs` under a directory.
fn collect_rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("read source entry").path();
        if path.is_dir() {
            collect_rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// Drop every `#[cfg(test)]` module from a source file, leaving the
/// production code.
///
/// Discriminating on `#[cfg(test)]` *modules* specifically, not on the
/// attribute: `terminal_stack.rs` carries a `#[cfg(test)]` struct FIELD
/// (`fail_next_reset`, the resync fault injector) a thousand lines above
/// the scrollbar owner, so truncating at the first attribute would hide
/// most of the production code from this audit and pass it vacuously —
/// which is exactly what the first run of this test did.
fn production_only(src: &str) -> String {
    let mut out = src.to_string();
    while let Some(at) = out.find("#[cfg(test)]") {
        let after = &out[at + "#[cfg(test)]".len()..];
        let trimmed = after.trim_start();
        if !trimmed.starts_with("mod ") {
            // Not a test module (a field, a method, a `use`). Neutralise
            // the marker so the scan advances, keeping the code itself.
            out.replace_range(at..at + "#[cfg(test)]".len(), "//          ");
            continue;
        }
        let module_start = at + (after.len() - trimmed.len()) + "#[cfg(test)]".len();
        // `#[cfg(test)] mod tests;` — an OUT-OF-LINE test module, three of
        // which exist in this crate. Its body is in another file, so there
        // is nothing here to excise: brace-matching forward would run into
        // the next unrelated item and swallow real production code, which
        // is a silently-vacuous audit. Neutralise and move on; the file it
        // names is excluded from the scan by `out_of_line_test_modules`.
        let decl_end = out[module_start..].find(';');
        let rel_open = out[module_start..].find('{');
        match (decl_end, rel_open) {
            (Some(semi), Some(brace)) if semi < brace => {
                out.replace_range(at..at + "#[cfg(test)]".len(), "//          ");
                continue;
            }
            (Some(_), None) => {
                out.replace_range(at..at + "#[cfg(test)]".len(), "//          ");
                continue;
            }
            _ => {}
        }
        let Some(rel_open) = rel_open else {
            break;
        };
        let open = module_start + rel_open;
        let mut depth = 0usize;
        let mut close = None;
        for (off, ch) in out[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(open + off);
                        break;
                    }
                }
                _ => {}
            }
        }
        match close {
            Some(close) => out.replace_range(at..=close, ""),
            None => break,
        }
    }
    strip_line_comments(&out)
}

/// Drop `//` comments, so prose that *mentions* a pattern this file audits
/// for is not mistaken for a call site. The owner's own doc comment names
/// `.terminal.scrollbar()` in order to explain the rule, and counting it
/// was the second false positive this test produced.
///
/// A `//` whose line has an odd number of unescaped quotes before it is
/// inside a string literal (a URL in a user-facing message) and is left
/// alone: truncating there could unbalance a brace and corrupt the
/// brace-matching below.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let mut quotes = 0usize;
        let mut escaped = false;
        let mut cut = None;
        let bytes = line.as_bytes();
        for (i, ch) in line.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => quotes += 1,
                '/' if quotes.is_multiple_of(2) && bytes.get(i + 1) == Some(&b'/') => {
                    cut = Some(i);
                    break;
                }
                _ => {}
            }
        }
        out.push_str(cut.map_or(line, |i| &line[..i]));
        out.push('\n');
    }
    out
}

/// Brace-match a function body starting at a signature.
fn body_of<'a>(src: &'a str, signature: &str) -> &'a str {
    let at = src
        .find(signature)
        .unwrap_or_else(|| panic!("`{signature}` still exists"));
    let open = at + src[at..].find('{').expect("the function has a body");
    let mut depth = 0usize;
    for (off, ch) in src[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open..=open + off];
                }
            }
            _ => {}
        }
    }
    panic!("brace-match failed for `{signature}`");
}

/// The counting shim cannot be bypassed.
///
/// A counted budget is only as good as the funnel it counts through: a new
/// raw `self.terminal.scrollbar()` anywhere in the crate would be
/// invisible to every assertion above. This is the same mechanical
/// backstop `scroll_viewport` has carried since #371 —
/// `terminal_scroll.rs::scroll_viewport_has_a_single_owner` — applied to
/// the derivation #1918 was about. Production sources only; tests read the
/// binding directly on purpose, to observe state without disturbing the
/// counts they are asserting on.
#[test]
fn scrollbar_reads_have_a_single_counted_owner() {
    let owner_path = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/components/terminal_stack.rs"
    ));
    let src =
        production_only(&std::fs::read_to_string(&owner_path).expect("read terminal_stack.rs"));

    let owner = body_of(
        &src,
        "fn scrollbar(&self) -> vt::error::Result<vt::terminal::Scrollbar>",
    );
    assert!(
        owner.contains("record_scrollbar()"),
        "the scrollbar owner must record the call it makes, or the budget \
         gate counts nothing",
    );
    assert!(
        owner.contains("self.terminal.scrollbar()"),
        "the scrollbar owner must be the one place that reaches the binding",
    );
    // Vacuity sentinels. `production_only` excises text, and an excision
    // bug that swallowed real code would make every count below read zero
    // and the audit pass having inspected nothing. These three are far
    // apart in the file, so all surviving means the production body did.
    for marker in [
        "fn feed(&mut self, bytes: &[u8])",
        "fn scroll(&mut self, request: ScrollRequest)",
        "pub fn render(&mut self, area: Rect",
    ] {
        assert!(
            src.contains(marker),
            "`{marker}` vanished from the production text — `production_only` \
             is excising real code, so this audit is inspecting a fragment",
        );
    }

    let mut paths = Vec::new();
    collect_rust_sources(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut paths,
    );
    paths.sort();
    let excluded = out_of_line_test_modules(&paths);
    assert!(
        !excluded.is_empty(),
        "this crate has out-of-line `#[cfg(test)] mod x;` modules; failing \
         to recognise any means the exclusion logic stopped working",
    );

    let mut counted = 0usize;
    for path in &paths {
        if excluded.contains(path) {
            continue;
        }
        let candidate = std::fs::read_to_string(path).expect("read Rust source");
        // `#[cfg(test)]` unit tests live inside `src/` and read the
        // binding deliberately, to observe state without disturbing the
        // counts they assert on.
        let production = production_only(&candidate);
        for _ in production.matches(".terminal.scrollbar(") {
            counted += 1;
            assert_eq!(
                path,
                &owner_path,
                "a raw scrollbar read escaped the counted owner into {} — \
                 route it through `TerminalVt::scrollbar` so the hot-path \
                 budget still sees it",
                path.display(),
            );
        }
    }
    assert_eq!(
        counted, 1,
        "exactly one raw `.terminal.scrollbar(` in production sources — \
         the counted owner's own call",
    );
}

/// Nothing on the per-tick path reads the config file.
///
/// `Config::load()` is a filesystem read plus a YAML parse and the TUI
/// calls it from a dozen places; every one of them must be an action or a
/// modal mount, never the tick phase that runs on every loop iteration
/// regardless of input (#1919 deliverable 1). Asserted at source level
/// because the honest alternative — counting file opens — would also
/// count the ones a legitimate action makes.
#[test]
fn the_tick_phase_does_not_load_the_config() {
    let src_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let step = production_only(
        &std::fs::read_to_string(src_root.join("realm/model/helpers.rs"))
            .expect("read realm/model/helpers.rs"),
    );
    let body = body_of(&step, "pub(super) fn run_loop_step<T: TerminalAdapter>");

    // The tick phase is the `model.tick_*()` calls in the loop step. Pull
    // their names out of the step itself rather than listing them here, so
    // a tick added later is covered without anyone remembering to.
    let mut ticks: Vec<String> = Vec::new();
    for (idx, _) in body.match_indices("model.tick_") {
        let rest = &body[idx + "model.".len()..];
        let end = rest.find('(').expect("a tick call has an argument list");
        ticks.push(rest[..end].to_string());
    }
    ticks.sort();
    ticks.dedup();
    assert!(
        ticks.len() >= 10,
        "expected the loop step to drive the tick family; found {ticks:?}",
    );

    let mut paths = Vec::new();
    collect_rust_sources(&src_root, &mut paths);
    // Read and strip each file ONCE, up front. The first cut did it inside
    // the per-tick loop, re-parsing a 16k-line `terminal_stack.rs` fourteen
    // times over, and timed out against nextest's 10s deadline on a loaded
    // box — a flake of exactly the kind this gate exists to avoid being.
    let sources: Vec<(&std::path::Path, String)> = paths
        .iter()
        .map(|path| {
            let text = production_only(&std::fs::read_to_string(path).expect("read Rust source"));
            (path.as_path(), text)
        })
        .collect();

    for tick in &ticks {
        let signature = format!("fn {tick}(");
        let mut found = false;
        for (path, candidate) in &sources {
            if !candidate.contains(&signature) {
                continue;
            }
            found = true;
            let body = body_of(candidate, &signature);
            assert!(
                !body.contains("Config::load"),
                "`{tick}` runs on every loop iteration and reads the config \
                 file in {} — a per-tick YAML parse. Resolve it once and \
                 carry it, as `fold_ui_config` does.",
                path.display(),
            );
        }
        assert!(found, "could not locate `{tick}`'s definition to audit it");
    }
}
