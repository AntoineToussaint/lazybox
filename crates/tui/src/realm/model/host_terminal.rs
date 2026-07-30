//! Single owner of the host-terminal control sequences lazybox emits.
//! [`enable_host_terminal`] (called from `Model::new`) and
//! [`restore_host_terminal`] (called from the [`HostTerminalGuard`]'s
//! `Drop`, the panic hook, and the signal handler) both walk the same
//! [`HostMode::ALL`] list, while live mouse-capture requests reuse the
//! same encoder. The enable set and restore set therefore can't drift.
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
    set_mouse_capture(&mut std::io::stdout(), enabled)
}

/// Set the first time [`restore_host_terminal`] runs. The guard's
/// `Drop`, the panic hook, and the signal handler can all fire on the
/// way out, but `PopKeyboardEnhancementFlags` is *not* idempotent — it
/// pops one entry off the terminal's flag stack per call, so a second
/// restore would pop a flag set the host had before lazybox started.
/// The flag collapses every teardown path to exactly one real restore.
static RESTORED: AtomicBool = AtomicBool::new(false);

/// Enable every [`HostMode`] in order. Called once from `Model::new`.
pub(crate) fn enable_host_terminal() {
    let mut out = std::io::stdout();
    for mode in HostMode::ALL {
        mode.enable(&mut out);
    }
    let _ = out.flush();
}

/// Restore every `HostMode` (reverse of the enable order), at most
/// once per process. Idempotent across the `HostTerminalGuard`'s
/// `Drop`, the panic hook, and the signal handler so the host shell is
/// always left out of raw mode and Kitty keyboard protocol on exit.
pub fn restore_host_terminal() {
    if RESTORED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut out = std::io::stdout();
    for mode in HostMode::ALL.into_iter().rev() {
        mode.disable(&mut out);
    }
    // Flush so the host terminal sees the resets before the shell
    // prompt (or a panic message) takes over the screen.
    let _ = out.flush();
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
