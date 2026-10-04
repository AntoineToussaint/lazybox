//! Per-call **CPU time** of the VT calls on the hot paths (#1919).
//!
//! # Why this exists next to the criterion benches
//!
//! `make bench`'s criterion groups measure wall clock, which is the right
//! thing on a quiet machine and useless on this one. Measured at load
//! average 70 on a 15-core box shared with the agent fleet, the
//! `anchor/*` group returned `[643 µs … 1.31 ms]` for the arm that does
//! strictly *less* work and `[671 µs … 1.01 ms]` for the arm that does
//! strictly more — overlapping intervals, with the heavier arm's median
//! lower. The benchmark could not tell apart two paths whose work differs
//! by a known 256 FFI calls.
//!
//! That is the empirical case for gating CI on counted work rather than
//! time, and it is also a measurement problem worth solving rather than
//! disclaiming. Thread CPU time solves it: `CLOCK_THREAD_CPUTIME_ID`
//! advances only while this thread is on a core, so time lost to being
//! descheduled behind fifteen rustc processes is excluded. Contention for
//! cache and memory bandwidth still inflates it, which is why the
//! estimator is the **minimum** across interleaved rounds — the round
//! least interfered with — reported next to the median so the spread is
//! visible.
//!
//! Arms are interleaved round by round rather than run to completion in
//! turn, so a drift in machine state lands on both rather than on one.
//!
//! Run it with `make bench-cpu`. It takes a few seconds and needs no
//! quiet box, which is the point.

use std::hint::black_box;

use lazybox_tui_term::vt_budget;
use libghostty_vt as vt;

const COLS: u16 = 80;
const ROWS: u16 = 50;
/// Rounds per arm. The estimator is the minimum, so more rounds buy a
/// better chance of catching one that ran uninterfered.
const ROUNDS: usize = 24;

/// Thread CPU nanoseconds — advances only while this thread runs.
fn cpu_nanos() -> u128 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, correctly-typed out-parameter for the
    // duration of the call, and the clock id is a documented constant.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &raw mut ts) };
    assert_eq!(rc, 0, "clock_gettime(CLOCK_THREAD_CPUTIME_ID) failed");
    ts.tv_sec as u128 * 1_000_000_000 + ts.tv_nsec as u128
}

fn new_parser() -> vt::Terminal<'static, 'static> {
    // The client's own production caps (`tui-term/src/session.rs`), so a
    // deep-history arm is bounded the way a real pane is.
    vt::Terminal::new(vt::TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback_lines: 10_000,
        max_scrollback_bytes: Some(10_000 * 4096),
    })
    .expect("libghostty-vt init")
}

/// A parser with `lines` rows of history above the screen.
fn parser_with_history(lines: usize) -> vt::Terminal<'static, 'static> {
    let mut t = new_parser();
    let mut payload = String::new();
    for i in 0..lines {
        payload.push_str(&format!("history line {i}\r\n"));
    }
    t.vt_write(payload.as_bytes());
    t
}

/// One chunk of escape-heavy agent output, the same shape the other
/// benches use.
fn chunk(i: usize) -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(b"\x1b[2K\r");
    c.extend_from_slice(format!("\x1b[38;5;{}m", 16 + (i % 200)).as_bytes());
    c.extend_from_slice(format!("· working ({i}) ").as_bytes());
    c.extend_from_slice("⠋⠙⠹⠸⠼".as_bytes());
    c.extend_from_slice(b"\x1b[0m");
    c.extend_from_slice(format!(" tool call #{i}: read file foo/bar/baz.rs\r\n").as_bytes());
    c
}

struct Arm {
    name: &'static str,
    ops: usize,
    /// Per-op CPU nanoseconds, one entry per round.
    per_op: Vec<f64>,
}

impl Arm {
    fn new(name: &'static str, ops: usize) -> Self {
        Self {
            name,
            ops,
            per_op: Vec::with_capacity(ROUNDS),
        }
    }

    /// Time one round of `ops` operations, excluding whatever `setup`
    /// costs.
    fn round<S, T>(&mut self, setup: S, body: impl FnOnce(&mut T))
    where
        S: FnOnce() -> T,
    {
        let mut state = setup();
        let start = cpu_nanos();
        body(&mut state);
        let elapsed = cpu_nanos() - start;
        drop(state);
        self.per_op.push(elapsed as f64 / self.ops as f64);
    }

    fn report(&self) {
        let mut sorted = self.per_op.clone();
        sorted.sort_by(f64::total_cmp);
        let min = sorted.first().copied().unwrap_or(0.0);
        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0.0);
        let max = sorted.last().copied().unwrap_or(0.0);
        println!(
            "  {:<34} min {:>9.1} ns   median {:>9.1} ns   max {:>9.1} ns",
            self.name, min, median, max
        );
    }
}

fn main() {
    const CHUNKS: usize = 256;
    const CALLS: usize = 4_096;
    let corpus: Vec<Vec<u8>> = (0..CHUNKS).map(chunk).collect();

    println!(
        "Per-call CPU time (CLOCK_THREAD_CPUTIME_ID), {ROUNDS} interleaved rounds.\n\
         Minimum is the estimator — the round least interfered with. Load-robust:\n\
         descheduling is excluded, so this is meaningful on a busy shared box.\n"
    );

    // State the grid the "deep" arms actually measure, rather than
    // assuming the history asked for is the history held. The client's
    // line cap has been observed not to bind (#1909's incidental
    // finding), so the depth claim has to be read off the grid.
    {
        let mut deep = parser_with_history(40_000);
        let at_bottom = deep.scrollbar().expect("scrollbar");
        deep.scroll_viewport(vt::terminal::ScrollViewport::Delta(-9_000));
        let parked = deep.scrollbar().expect("scrollbar");
        println!(
            "Deep arm's real grid: total {} rows, viewport {} rows long; parked at \
             offset {} — {} rows above the live bottom.",
            at_bottom.total,
            at_bottom.len,
            parked.offset,
            parked.total.saturating_sub(parked.offset + parked.len),
        );
        println!();
    }

    let mut write_only = Arm::new("vt_write", CHUNKS);
    let mut write_plus_bar_bottom = Arm::new("vt_write + scrollbar (at bottom)", CHUNKS);
    let mut write_plus_bar_parked = Arm::new("vt_write + scrollbar (parked 500)", CHUNKS);
    let mut bar_bottom = Arm::new("scrollbar alone (at bottom)", CALLS);
    let mut bar_parked = Arm::new("scrollbar alone (parked 500)", CALLS);
    // The binding warns that "arbitrary pins are expensive". Give that
    // claim its best shot: a pane at the production scrollback cap, with
    // the viewport parked thousands of rows up rather than hundreds. If
    // the warning bites anywhere, it bites here.
    let mut bar_deep_bottom = Arm::new("scrollbar alone (deep, at bottom)", CALLS);
    let mut bar_deep_parked = Arm::new("scrollbar alone (deep, parked 9000)", CALLS);
    let mut bar_deep_mid = Arm::new("scrollbar alone (deep, parked 5000)", CALLS);
    let mut counter = Arm::new("vt_budget::record_row_read", CALLS);

    for _ in 0..ROUNDS {
        write_only.round(
            || parser_with_history(2_000),
            |t| {
                for c in &corpus {
                    t.vt_write(black_box(c));
                }
            },
        );
        write_plus_bar_bottom.round(
            || parser_with_history(2_000),
            |t| {
                for c in &corpus {
                    t.vt_write(black_box(c));
                    black_box(t.scrollbar().ok());
                }
            },
        );
        write_plus_bar_parked.round(
            || {
                let mut t = parser_with_history(2_000);
                t.scroll_viewport(vt::terminal::ScrollViewport::Delta(-500));
                t
            },
            |t| {
                for c in &corpus {
                    t.vt_write(black_box(c));
                    black_box(t.scrollbar().ok());
                }
            },
        );
        bar_bottom.round(
            || parser_with_history(2_000),
            |t| {
                for _ in 0..CALLS {
                    black_box(t.scrollbar().ok());
                }
            },
        );
        bar_parked.round(
            || {
                let mut t = parser_with_history(2_000);
                t.scroll_viewport(vt::terminal::ScrollViewport::Delta(-500));
                t
            },
            |t| {
                for _ in 0..CALLS {
                    black_box(t.scrollbar().ok());
                }
            },
        );
        bar_deep_bottom.round(
            || parser_with_history(40_000),
            |t| {
                for _ in 0..CALLS {
                    black_box(t.scrollbar().ok());
                }
            },
        );
        bar_deep_parked.round(
            || {
                let mut t = parser_with_history(40_000);
                t.scroll_viewport(vt::terminal::ScrollViewport::Delta(-9_000));
                t
            },
            |t| {
                for _ in 0..CALLS {
                    black_box(t.scrollbar().ok());
                }
            },
        );
        bar_deep_mid.round(
            || {
                let mut t = parser_with_history(40_000);
                t.scroll_viewport(vt::terminal::ScrollViewport::Delta(-5_000));
                t
            },
            |t| {
                for _ in 0..CALLS {
                    black_box(t.scrollbar().ok());
                }
            },
        );
        counter.round(
            || (),
            |()| {
                for _ in 0..CALLS {
                    vt_budget::record_row_read();
                }
            },
        );
    }

    println!("The #1918 regression — what `feed` paid per output chunk:");
    write_only.report();
    write_plus_bar_bottom.report();
    write_plus_bar_parked.report();
    println!("\nThe call itself, and why the binding says \"depending on where the viewport is\":");
    bar_bottom.report();
    bar_parked.report();
    bar_deep_bottom.report();
    bar_deep_mid.report();
    bar_deep_parked.report();
    println!("\nThe gate's own instrument, for the claim that it is below the noise floor:");
    counter.report();
}
