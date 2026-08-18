//! Single owner of the host-terminal control sequences lazybox emits.
//! [`enable_host_terminal`] (called from `Model::new`) and
//! [`restore_host_terminal`] (called from the [`HostTerminalGuard`]'s
//! `Drop`, the panic hook, and the signal handler) both walk the same
//! [`HostMode::ALL`] list, while live mouse-capture requests reuse the
//! same encoder. The enable set and restore set therefore can't drift.
//!
//! Four exits, one mode list:
//!
//! | exit | path |
//! |---|---|
//! | clean quit / error | [`HostTerminalGuard`]'s `Drop` |
//! | Rust panic | the panic hook in `model/mod.rs` |
//! | SIGTERM / SIGHUP / SIGINT | `wait_for_exit_signal` (tui-boot) |
//! | **abort / fault** | [`fatal`] — SIGABRT, SIGSEGV, SIGBUS, … |
//!
//! The last row is the one an abort out of the vendored zig VT parser
//! takes: it neither unwinds nor can be handled asynchronously, so it
//! needs the signal-safe path in [`fatal`] rather than any of the other
//! three. See that module for why.
//! The leak this prevents (#211): the old teardown was
//! hand-rolled in three places that didn't all run, and the panic
//! path omitted `DisableFocusChange`, so an error/signal/panic exit
//! stranded the shell in Kitty keyboard protocol (CSI-u) where every
//! keystroke comes back as an escape sequence.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// One host-terminal mode lazybox enables at startup. The enum is the
/// single source of truth: [`HostMode::enable`] and [`HostMode::disable`]
/// are exhaustive matches, so a new variant that adds an `Enable*`/`Push*`
/// is forced by the compiler to spell out its matching `Disable*`/`Pop*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HostMode {
    RawMode,
    AlternateScreen,
    MouseCapture,
    BracketedPaste,
    FocusChange,
    KeyboardEnhancement,
}

impl HostMode {
    /// Enable order. Raw mode + alt screen come first so everything
    /// else paints into the alternate buffer; [`restore_host_terminal`]
    /// walks this in reverse.
    pub(crate) const ALL: [HostMode; 6] = [
        HostMode::RawMode,
        HostMode::AlternateScreen,
        HostMode::MouseCapture,
        HostMode::BracketedPaste,
        HostMode::FocusChange,
        HostMode::KeyboardEnhancement,
    ];

    fn enable(self, out: &mut impl Write) {
        match self {
            // Raw mode: keys reach lazybox un-cooked (no line buffering,
            // no echo, Ctrl-C as a byte not a signal).
            HostMode::RawMode => {
                let _ = enable_raw_mode();
            }
            HostMode::AlternateScreen => {
                let _ = crossterm::execute!(out, EnterAlternateScreen);
            }
            // Mouse capture drives splitter resize + click-to-focus +
            // lazybox-side terminal text selection. F8 / Alt-s toggles
            // it off for host-native selection.
            HostMode::MouseCapture => {
                let _ = set_mouse_capture(out, true);
            }
            // Bracketed paste: the host wraps pasted text in
            // `ESC[200~ … ESC[201~` so a paste is one `Event::Paste`
            // forwarded to the PTY, not N fast keystrokes.
            HostMode::BracketedPaste => {
                let _ = crossterm::execute!(out, EnableBracketedPaste);
            }
            // Focus reporting (DEC 1004): lazybox suppresses desktop
            // notifications while it's the focused window.
            HostMode::FocusChange => {
                let _ = crossterm::execute!(out, EnableFocusChange);
            }
            // Kitty keyboard protocol: disambiguate modified Enter /
            // Tab / Backspace so lazybox can tell Shift-Enter (newline)
            // from Enter (submit). Terminals without support ignore it.
            HostMode::KeyboardEnhancement => {
                let _ = crossterm::execute!(
                    out,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
                    )
                );
            }
        }
    }

    fn disable(self, out: &mut impl Write) {
        match self {
            HostMode::RawMode => {
                let _ = disable_raw_mode();
            }
            HostMode::AlternateScreen => {
                let _ = crossterm::execute!(out, LeaveAlternateScreen);
            }
            HostMode::MouseCapture => {
                let _ = set_mouse_capture(out, false);
            }
            HostMode::BracketedPaste => {
                let _ = crossterm::execute!(out, DisableBracketedPaste);
            }
            HostMode::FocusChange => {
                let _ = crossterm::execute!(out, DisableFocusChange);
            }
            HostMode::KeyboardEnhancement => {
                let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
            }
        }
    }
}

fn set_mouse_capture(out: &mut impl Write, enabled: bool) -> std::io::Result<()> {
    if enabled {
        crossterm::execute!(out, EnableMouseCapture)
    } else {
        crossterm::execute!(out, DisableMouseCapture)
    }
}

pub(crate) fn request_mouse_capture(enabled: bool) -> std::io::Result<()> {
    // Route through the render writer, not stdout directly: a bare stdout
    // write here would race the writer thread mid-frame and corrupt it (or
    // split the mouse-mode escape). `enqueue_raw` serializes it behind the
    // current frame on the one channel (#1090 finding #1). Encode into a buffer
    // first so the whole escape is enqueued as one atomic unit.
    let mut buf = Vec::new();
    set_mouse_capture(&mut buf, enabled)?;
    super::render_writer::enqueue_raw(&buf);
    Ok(())
}

/// Set the first time [`restore_host_terminal`] runs. The guard's
/// `Drop`, the panic hook, and the signal handler can all fire on the
/// way out, but `PopKeyboardEnhancementFlags` is *not* idempotent — it
/// pops one entry off the terminal's flag stack per call, so a second
/// restore would pop a flag set the host had before lazybox started.
/// The flag collapses every teardown path to exactly one real restore.
static RESTORED: AtomicBool = AtomicBool::new(false);

/// The escape-sequence half of [`restore_host_terminal`]: every
/// [`HostMode`] except [`HostMode::RawMode`], in teardown order.
///
/// Raw mode is excluded because it is a *termios* change, not bytes on
/// the wire — the fatal-signal path restores it with `tcsetattr` from a
/// pre-raw snapshot instead. Excluding it also keeps this function pure:
/// `HostMode::RawMode.disable` would call crossterm's `disable_raw_mode`
/// for real, so precomputing the buffer at startup would knock the
/// terminal out of raw mode the moment we armed the handler.
fn signal_restore_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    for mode in HostMode::ALL.into_iter().rev() {
        if matches!(mode, HostMode::RawMode) {
            continue;
        }
        mode.disable(&mut buf);
    }
    buf
}

/// Terminal restore for the signals that kill the process *without*
/// unwinding — the hole between the panic hook (Rust panics) and
/// `wait_for_exit_signal` in tui-boot (SIGTERM / SIGHUP / SIGINT).
///
/// The gap that motivated this: the vendored zig VT parser aborts on a
/// failed internal invariant (ghostty's `PageList` hyperlink capacity
/// check hits `unreachable` under `-Doptimize=ReleaseSafe`). That raises
/// **SIGABRT**, which no existing path covers:
///
/// - it does not unwind, so neither the panic hook nor the
///   `HostTerminalGuard`'s `Drop` runs;
/// - it cannot be handled asynchronously, because `abort()` re-raises
///   with the default disposition as soon as a handler returns — a tokio
///   task would lose the race and usually never be scheduled at all.
///
/// So this is a synchronous `sigaction` handler, and everything it needs
/// is computed at arm time: the handler only calls `tcsetattr`, `write`,
/// and `raise`. It deliberately does NOT reuse [`restore_host_terminal`],
/// which locks stdout and allocates — a lock the aborting thread may
/// already hold, and an allocator that may be mid-operation.
#[cfg(unix)]
mod fatal {
    use super::{RESTORED, signal_restore_bytes};
    use std::ffi::c_int;
    use std::sync::OnceLock;
    use std::sync::atomic::Ordering;

    /// Signals that terminate without unwinding. SIGABRT is the one
    /// seen in the wild (zig `unreachable` → `abort`); the memory
    /// faults ride along because a fault in the vendored C/zig VT layer
    /// arrives the same way and leaves the terminal just as wrecked.
    pub(super) const FATAL_SIGNALS: [c_int; 5] = [
        libc::SIGABRT,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
    ];

    /// The host's termios from *before* raw mode was enabled.
    static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();
    /// Precomputed [`signal_restore_bytes`]. Read-only by the time any
    /// handler can fire, so the handler never allocates.
    static RESTORE_BYTES: OnceLock<Vec<u8>> = OnceLock::new();

    /// Written to stderr after the screen is back. A fixed byte string:
    /// formatting would allocate, which a signal handler must not do.
    const NOTE: &[u8] =
        b"\r\nlazybox: fatal signal - terminal restored. See /tmp/lazybox.log for the cause.\r\n";

    /// `write(2)` until the buffer is out or the fd stops accepting it.
    /// A short/interrupted write ends the loop rather than spinning:
    /// losing the tail of a reset beats hanging inside a handler.
    fn write_all(fd: c_int, mut buf: &[u8]) {
        while !buf.is_empty() {
            let written = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
            if written <= 0 {
                break;
            }
            buf = &buf[written as usize..];
        }
    }

    extern "C" fn handle(sig: c_int) {
        // Take fd 1 back from the off-thread render writer before writing the
        // reset sequence to it, so the two can't interleave and strand the
        // shell (#1045). This is the one restore path that can't route through
        // the writer's channel — a fatal signal doesn't unwind and can't
        // allocate or block — so it writes fd 1 directly, made safe by this
        // signal-safe handoff (atomics + a bounded spin). No-op until the
        // writer has spawned.
        super::super::render_writer::muzzle_writer();
        // Same one-shot flag the guard/panic paths use: if teardown
        // already ran, re-emitting would pop a keyboard-flags entry the
        // host owned before lazybox started.
        if !RESTORED.swap(true, Ordering::SeqCst) {
            if let Some(termios) = ORIGINAL_TERMIOS.get() {
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, termios) };
            }
            if let Some(bytes) = RESTORE_BYTES.get() {
                write_all(libc::STDOUT_FILENO, bytes);
            }
        }
        write_all(libc::STDERR_FILENO, NOTE);
        // SA_RESETHAND restored the default disposition on entry, so
        // this re-raise terminates (and still dumps core). We borrow the
        // instant before death to fix the screen; we never swallow it.
        unsafe {
            libc::raise(sig);
        }
    }

    /// Snapshot the pre-raw termios, precompute the restore bytes, and
    /// install [`handle`] for every [`FATAL_SIGNALS`] entry. Must run
    /// *before* raw mode is enabled. Idempotent.
    pub(super) fn arm() {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) } == 0 {
            let _ = ORIGINAL_TERMIOS.set(unsafe { termios.assume_init() });
        }
        let _ = RESTORE_BYTES.set(signal_restore_bytes());

        for sig in FATAL_SIGNALS {
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handle as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                action.sa_flags = libc::SA_RESETHAND;
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
    }

    #[cfg(test)]
    pub(super) fn write_all_for_test(fd: c_int, buf: &[u8]) {
        write_all(fd, buf);
    }
}

#[cfg(not(unix))]
mod fatal {
    pub(super) fn arm() {}
}

/// Enable every [`HostMode`] in order. Called once from `Model::new`.
pub(crate) fn enable_host_terminal() {
    // Arm the fatal-signal restore FIRST: it snapshots the termios that
    // raw mode is about to replace, and from this point on an abort out
    // of the vendored VT parser leaves a usable shell behind.
    fatal::arm();
    let mut bytes = Vec::new();
    for mode in HostMode::ALL {
        mode.enable(&mut bytes);
    }
    // `Model::new` constructs the async writer before this guard, so startup
    // escapes share the same ordered fd-1 lane as frames, OSC passthrough,
    // mouse toggles, clipboard writes, and restore. `enqueue_raw` retains a
    // direct fallback for a headless caller with no writer.
    super::render_writer::enqueue_raw(&bytes);
}

/// Restore every `HostMode` (reverse of the enable order), at most
/// once per process. Idempotent across the `HostTerminalGuard`'s
/// `Drop`, the panic hook, and the graceful-signal task so the host shell is
/// always left out of raw mode and Kitty keyboard protocol on exit.
///
/// This is the **normal-context** restore (clean exit, panic hook,
/// graceful-signal task). The fd-1 escape half routes through the off-thread
/// render writer (`render_writer::write_restore`) — the sole fd-1 owner — so it
/// serializes behind any in-flight frame and can never interleave with one and
/// strand the shell (#211 / #1122). Raw mode is a *termios* change on stdin,
/// not an fd-1 write, so it is restored directly. The fatal-signal path in
/// `fatal` cannot allocate or block and takes the signal-safe `muzzle_writer` +
/// raw-`write(2)` path instead.
pub fn restore_host_terminal() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    // The fd-1 escapes (alt screen, mouse, paste, focus, Kitty keyboard), in
    // teardown order, minus raw mode. Routed through the writer, which emits
    // them behind any queued frame and then seals fd 1, and blocks until they
    // are out so they land before the shell prompt / a panic message.
    super::render_writer::write_restore(&signal_restore_bytes());
    // Raw mode last, to match the reverse-of-enable order — a termios change
    // on stdin, so it never contends with the writer's fd-1 writes.
    let _ = disable_raw_mode();
}

/// RAII owner of the host-terminal modes. Constructing it enables every
/// [`HostMode`]; dropping it restores them. Held by `Model`, this makes
/// restore an unwind-safe invariant: a clean `q q`, a `?`-bubbled error
/// out of the run loop, or a panic all funnel through the same teardown
/// when the `Model` (and thus the guard) drops.
pub(crate) struct HostTerminalGuard;

impl HostTerminalGuard {
    pub(crate) fn new() -> Self {
        enable_host_terminal();
        HostTerminalGuard
    }
}

impl Drop for HostTerminalGuard {
    fn drop(&mut self) {
        restore_host_terminal();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Every mode enabled at startup has a matching reset in the single
    /// teardown path — the regression guard for #211. Because both
    /// `enable_host_terminal` and `restore_host_terminal` iterate
    /// `HostMode::ALL`, a future variant with an `Enable*`/`Push*` but
    /// no `Disable*`/`Pop*` can't compile (exhaustive match) and can't
    /// silently skip teardown (this test).
    #[test]
    fn teardown_disables_exactly_what_startup_enables() {
        let enabled: BTreeSet<HostMode> = HostMode::ALL.into_iter().collect();
        let disabled: BTreeSet<HostMode> = HostMode::ALL.into_iter().rev().collect();
        assert_eq!(enabled, disabled);

        // The set must cover the modes the symptom report named: mouse,
        // bracketed paste, focus change, keyboard enhancement, alt
        // screen, raw mode. Spelled out so dropping one from `ALL`
        // fails here rather than silently shrinking teardown.
        let expected: BTreeSet<HostMode> = [
            HostMode::RawMode,
            HostMode::AlternateScreen,
            HostMode::MouseCapture,
            HostMode::BracketedPaste,
            HostMode::FocusChange,
            HostMode::KeyboardEnhancement,
        ]
        .into_iter()
        .collect();
        assert_eq!(enabled, expected);
    }

    /// Restore walks the enable order in reverse — alt screen / raw mode
    /// come down last, after the modes that paint into them.
    #[test]
    fn restore_order_is_enable_reversed() {
        let forward: Vec<HostMode> = HostMode::ALL.into_iter().collect();
        let mut reversed = forward.clone();
        reversed.reverse();
        let restore: Vec<HostMode> = HostMode::ALL.into_iter().rev().collect();
        assert_eq!(restore, reversed);
        assert_ne!(restore, forward);
    }

    /// The precomputed signal buffer is exactly the escape half of
    /// teardown, in teardown order, with raw mode left out (it's a
    /// termios change, restored via `tcsetattr`). A new `HostMode` is
    /// picked up automatically — both sides walk `HostMode::ALL`.
    #[test]
    fn signal_restore_bytes_are_teardown_minus_raw_mode() {
        let bytes = signal_restore_bytes();

        let mut expected = Vec::new();
        for mode in HostMode::ALL.into_iter().rev() {
            if matches!(mode, HostMode::RawMode) {
                continue;
            }
            mode.disable(&mut expected);
        }
        assert_eq!(bytes, expected);

        // Pin the two resets that decide whether a crashed lazybox
        // leaves a usable shell, so this can't pass on an empty or
        // reordered buffer: mouse tracking must go, and the alt screen
        // must come down LAST (everything else paints into it).
        let mouse_off = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
        assert!(
            bytes.windows(mouse_off.len()).any(|w| w == mouse_off),
            "signal restore must disable mouse tracking: {bytes:?}"
        );
        assert!(
            bytes.ends_with(b"\x1b[?1049l"),
            "alt screen must be left last: {bytes:?}"
        );
    }

    /// The regression anchor for the crash this path exists for: a zig
    /// `unreachable` in the vendored VT parser raises SIGABRT, which
    /// unwinds nothing and so reaches neither the panic hook nor the
    /// `HostTerminalGuard`'s `Drop`.
    #[cfg(unix)]
    #[test]
    fn fatal_signals_cover_abort() {
        assert!(
            fatal::FATAL_SIGNALS.contains(&libc::SIGABRT),
            "SIGABRT is the signal a zig `unreachable` raises"
        );
        for sig in [libc::SIGSEGV, libc::SIGBUS] {
            assert!(fatal::FATAL_SIGNALS.contains(&sig));
        }
        // The graceful signals stay with tui-boot's async handler,
        // which can also drain the daemon. Claiming them here would
        // replace that with a bare re-raise.
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            assert!(
                !fatal::FATAL_SIGNALS.contains(&sig),
                "signal {sig} belongs to wait_for_exit_signal"
            );
        }
    }

    /// The handler's writer is a raw `write(2)` loop (no `std::io`, no
    /// locking) — verify it actually delivers the whole buffer.
    #[cfg(unix)]
    #[test]
    fn signal_safe_writer_delivers_the_whole_buffer() {
        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let payload = signal_restore_bytes();
        fatal::write_all_for_test(write_fd, &payload);
        unsafe { libc::close(write_fd) };

        let mut got = vec![0_u8; payload.len() + 16];
        let n = unsafe { libc::read(read_fd, got.as_mut_ptr().cast(), got.len()) };
        unsafe { libc::close(read_fd) };

        assert!(n > 0, "read from pipe");
        got.truncate(n as usize);
        assert_eq!(got, payload);
    }

    /// The child half of [`abort_restores_the_terminal_and_still_dies`].
    /// Inert in a normal test run; the parent re-execs this binary with
    /// the env var set.
    #[cfg(unix)]
    #[test]
    fn fatal_signal_child_helper() {
        if std::env::var_os("LAZYBOX_FATAL_SIGNAL_CHILD").is_none() {
            return;
        }
        // Under a real PTY this is the production order: construct the
        // off-thread fd-1 owner, then arm + enable every host mode. The
        // adapter may fail when the parent test captures stdout with a pipe;
        // the signal restore itself is still exercised in that fallback.
        let _adapter = super::super::render_writer::AsyncCrosstermAdapter::new().ok();
        enable_host_terminal();
        unsafe { libc::abort() };
    }

    /// End-to-end proof, through a real `abort()`: the reset sequence
    /// reaches stdout, and the process still dies of SIGABRT. The second
    /// half matters as much as the first — this path borrows the moment
    /// before death to fix the screen, it must never swallow a crash.
    #[cfg(unix)]
    #[test]
    fn abort_restores_the_terminal_and_still_dies() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::Command;

        let exe = std::env::current_exe().expect("test binary path");
        let out = Command::new(exe)
            .args([
                "--exact",
                "realm::model::host_terminal::tests::fatal_signal_child_helper",
                "--nocapture",
            ])
            .env("LAZYBOX_FATAL_SIGNAL_CHILD", "1")
            .output()
            .expect("run child");

        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "the crash must still terminate the process; status={:?}",
            out.status
        );
        let expected = signal_restore_bytes();
        assert!(
            out.stdout
                .windows(expected.len())
                .any(|w| w == expected.as_slice()),
            "abort must emit the restore sequence; stdout={:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("terminal restored"),
            "abort should say what happened; stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Panic counterpart to the fatal child. A PTY smoke check can invoke
    /// this exact helper and compare `stty -g` before/after; the ordinary test
    /// still pins that the panic hook emits the complete reset before the
    /// process reports failure.
    #[cfg(unix)]
    #[test]
    fn panic_restore_child_helper() {
        if std::env::var_os("LAZYBOX_PANIC_RESTORE_CHILD").is_none() {
            return;
        }
        let _adapter = super::super::render_writer::AsyncCrosstermAdapter::new().ok();
        super::super::install_panic_hook();
        enable_host_terminal();
        panic!("intentional lazybox panic restore probe");
    }

    #[cfg(unix)]
    #[test]
    fn panic_hook_restores_before_the_child_exits() {
        use std::process::Command;

        let exe = std::env::current_exe().expect("test binary path");
        let out = Command::new(exe)
            .args([
                "--exact",
                "realm::model::host_terminal::tests::panic_restore_child_helper",
                "--nocapture",
            ])
            .env("LAZYBOX_PANIC_RESTORE_CHILD", "1")
            .output()
            .expect("run child");

        assert!(
            !out.status.success(),
            "intentional panic must fail the child"
        );
        let expected = signal_restore_bytes();
        assert!(
            out.stdout
                .windows(expected.len())
                .any(|window| window == expected.as_slice()),
            "panic must emit the restore sequence; stdout={:?}",
            String::from_utf8_lossy(&out.stdout),
        );
        assert!(
            String::from_utf8_lossy(&out.stderr)
                .contains("intentional lazybox panic restore probe"),
            "panic remains visible after restore; stderr={:?}",
            String::from_utf8_lossy(&out.stderr),
        );
    }

    #[test]
    fn mouse_capture_request_uses_crossterm_tracking_modes() {
        let mut out = Vec::new();
        set_mouse_capture(&mut out, true).expect("enable mouse capture");
        assert_eq!(
            out,
            b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1015h\x1b[?1006h"
        );

        out.clear();
        set_mouse_capture(&mut out, false).expect("disable mouse capture");
        assert_eq!(
            out,
            b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
        );
    }
}
