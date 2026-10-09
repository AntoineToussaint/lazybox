//! Counted work on the terminal hot paths — the instrument the
//! performance gate asserts against (#1919).
//!
//! # Why counts and not milliseconds
//!
//! #1910 put `Terminal::scrollbar()` — a call whose own binding says
//! "may be expensive … not too frequently"
//! ([`libghostty_vt::Terminal::scrollbar`]) — on `TerminalVt::feed`, the
//! client's hottest path. Nothing in CI could see it; the user felt the
//! keyboard get slower and filed #1918.
//!
//! A wall-clock threshold cannot be that gate. This repo already knows
//! why: the suite has a loaded profile (`make test-loaded`), fixed helper
//! timeouts flake above load ~30, and a shared dev box routinely sits at
//! load 75. A time budget is then either so loose it catches nothing or
//! so tight it fails PRs that changed nothing.
//!
//! What *is* exact on every machine is how many times the hot path
//! reached across the FFI boundary. "`scrollbar()` is called zero times
//! while the viewport follows the tail" is a structural claim about the
//! code, it is true or false independent of load, and it would have
//! failed on #1910's diff. So the counts are the gate and the
//! milliseconds are a benchmark for humans (`make bench`).
//!
//! # What this costs in production
//!
//! The counters are compiled in unconditionally and always record. That
//! is a deliberate trade and it is affordable because of the
//! *granularity*: one thread-local [`Cell`] increment per scrollbar read
//! and per **row** (not per cell) of a rendered frame. A full-window tile
//! is ~50 rows, so a frame pays ~50 non-atomic increments against a walk
//! that makes thousands of FFI calls — below the noise floor of the frame
//! benchmark.
//!
//! The alternative — a cargo feature enabled only through
//! `[dev-dependencies]` — was rejected: when the feature is off every
//! counter reads zero, so every "this path does no work" assertion passes
//! *vacuously*. A gate that silently stops gating is worse than no gate,
//! and that failure mode is exactly the one #1919 exists to remove. The
//! counters being always-on is what makes [`Counts::assert_live`]
//! meaningful.
//!
//! Counting per *cell* would not be affordable (~12k increments a frame)
//! and is not needed: the budget #1919 states is row reads per row.
//!
//! # Thread-locals, not atomics
//!
//! A `libghostty-vt` parser is `!Send` and lives on the UI thread, so
//! there is nothing to share. Per-thread state also isolates tests from
//! each other under plain `cargo test` (threads), which `cargo nextest`
//! would give us for free (process per test) and which must not be
//! relied on.
//!
//! # Use
//!
//! ```
//! use lazybox_tui_term::vt_budget;
//!
//! let watch = vt_budget::watch();
//! // … drive the path under test …
//! assert_eq!(watch.counts().scrollbar, 0);
//! ```

use std::cell::Cell;

thread_local! {
    static SCROLLBAR: Cell<u64> = const { Cell::new(0) };
    static ROW_READS: Cell<u64> = const { Cell::new(0) };
    static FRAMES: Cell<u64> = const { Cell::new(0) };
}

/// Record one call to the expensive scrollbar derivation.
///
/// Called from the single counted accessor that owns every
/// `Terminal::scrollbar()` read in the client — see
/// `lazybox_tui::components::terminal_stack`'s `TerminalVt::scrollbar`,
/// whose sole-ownership is enforced at source level by
/// `tests/terminal_hot_paths.rs`.
#[inline]
pub fn record_scrollbar() {
    SCROLLBAR.with(|c| c.set(c.get().wrapping_add(1)));
}

/// Record one row fetched across the FFI boundary while painting a frame.
#[inline]
pub fn record_row_read() {
    ROW_READS.with(|c| c.set(c.get().wrapping_add(1)));
}

/// Record one frame that actually walked the grid — as opposed to one
/// served from the caller's cached-frame blit.
#[inline]
pub fn record_frame() {
    FRAMES.with(|c| c.set(c.get().wrapping_add(1)));
}

/// A reading of the counters, as a difference from when the
/// [`Watch`] was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Calls to libghostty's scrollbar derivation — the expensive one.
    pub scrollbar: u64,
    /// Rows fetched across the FFI boundary by a frame walk.
    pub row_reads: u64,
    /// Frames that walked the grid rather than blitting a cached one.
    pub frames: u64,
}

impl Counts {
    /// Fail loudly when nothing was counted at all.
    ///
    /// Every "this path does N units of work" assertion in the gate has a
    /// companion positive control, because the dangerous failure is not a
    /// wrong number — it is a counter that stopped observing, which turns
    /// every zero-assertion green while the regression ships. Call this
    /// with a control reading taken from a path that *must* do work.
    #[track_caller]
    pub fn assert_live(&self) {
        assert!(
            self.scrollbar > 0 || self.row_reads > 0 || self.frames > 0,
            "the vt_budget counters recorded nothing on a path that must do \
             work — the instrument is broken, so every zero-assertion in \
             this gate is passing vacuously",
        );
    }
}

/// A zero point for the counters. Take one, drive the path, then read
/// [`Watch::counts`] for the work that path did.
///
/// Differences rather than absolutes, so a watch composes with whatever
/// the test did to set the scene (spawning a terminal, feeding a corpus,
/// painting a first frame) without having to account for it.
#[derive(Debug, Clone, Copy)]
pub struct Watch {
    scrollbar: u64,
    row_reads: u64,
    frames: u64,
}

/// Start counting from now.
#[must_use]
pub fn watch() -> Watch {
    Watch {
        scrollbar: SCROLLBAR.with(Cell::get),
        row_reads: ROW_READS.with(Cell::get),
        frames: FRAMES.with(Cell::get),
    }
}

impl Watch {
    /// Work recorded on this thread since the watch was taken.
    #[must_use]
    pub fn counts(&self) -> Counts {
        Counts {
            scrollbar: SCROLLBAR.with(Cell::get).wrapping_sub(self.scrollbar),
            row_reads: ROW_READS.with(Cell::get).wrapping_sub(self.row_reads),
            frames: FRAMES.with(Cell::get).wrapping_sub(self.frames),
        }
    }

    /// Re-zero this watch to the counters as they stand now, so one watch
    /// can measure several phases of a test in turn.
    pub fn restart(&mut self) {
        *self = watch();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watch_measures_the_difference_not_the_absolute() {
        record_scrollbar();
        let watch = watch();
        record_scrollbar();
        record_scrollbar();
        assert_eq!(
            watch.counts().scrollbar,
            2,
            "the call before the watch must not be counted",
        );
    }

    #[test]
    fn restart_rezeroes() {
        let mut watch = watch();
        record_row_read();
        assert_eq!(watch.counts().row_reads, 1);
        watch.restart();
        assert_eq!(watch.counts().row_reads, 0);
        record_row_read();
        record_row_read();
        assert_eq!(watch.counts().row_reads, 2);
    }

    #[test]
    fn assert_live_rejects_a_silent_instrument() {
        let watch = watch();
        let silent = watch.counts();
        assert_eq!(silent, Counts::default());
        record_frame();
        watch.counts().assert_live();
    }

    #[test]
    #[should_panic(expected = "the instrument is broken")]
    fn assert_live_panics_on_nothing_counted() {
        Counts::default().assert_live();
    }
}
