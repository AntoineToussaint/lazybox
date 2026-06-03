//! Native desktop notifications carried by terminal OSC escape
//! sequences.
//!
//! Ghostty, iTerm2, Kitty and WezTerm raise a real Notification
//! Center banner when lazybox writes an OSC notification sequence to
//! its controlling terminal. Unlike the subprocess fallbacks in
//! [`crate::platform::notify_user`] (`terminal-notifier` /
//! `osascript` / `notify-send`), the banner is produced by the
//! terminal emulator itself — so it surfaces on the *local* machine
//! even when lazybox runs over SSH, and needs no helper binary.
//!
//! Two sequences cover the field:
//!   - **OSC 777** `ESC ] 777 ; notify ; TITLE ; BODY BEL` — full
//!     title + body. Ghostty, Kitty, WezTerm.
//!   - **OSC 9** `ESC ] 9 ; BODY BEL` — body only, no title field.
//!     iTerm2; the title is folded into the body.
//!
//! Inside tmux the sequence is wrapped in a passthrough envelope so
//! it reaches the outer terminal — tmux must have `allow-passthrough`
//! enabled (the default since tmux 3.3a).

use std::sync::atomic::{AtomicU8, Ordering};

/// Which OSC notification dialect the controlling terminal speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscNotifier {
    /// `ESC ] 777 ; notify ; TITLE ; BODY BEL` — title + body.
    Osc777,
    /// `ESC ] 9 ; BODY BEL` — body only.
    Osc9,
}

// Tri-valued focus state. A terminal that never reports focus
// (Terminal.app, plain SSH) stays `UNKNOWN` forever, and `UNKNOWN`
// must not suppress — otherwise those users would never see a
// notification. Only an explicit, reported `FOCUSED` suppresses.
const FOCUS_UNKNOWN: u8 = 0;
const FOCUS_FOCUSED: u8 = 1;
const FOCUS_UNFOCUSED: u8 = 2;
static FOCUS: AtomicU8 = AtomicU8::new(FOCUS_UNKNOWN);

/// Record a terminal focus change (DEC mode 1004 focus reporting).
/// The TUI calls this from its crossterm `FocusGained` / `FocusLost`
/// handlers. Terminal focus is genuinely process-global — there is
/// one controlling terminal — so a global is the right model.
pub fn set_terminal_focus(focused: bool) {
    FOCUS.store(
        if focused {
            FOCUS_FOCUSED
        } else {
            FOCUS_UNFOCUSED
        },
        Ordering::Relaxed,
    );
}

/// True only when the terminal has *reported* itself focused. Unknown
/// focus (no report ever seen) is deliberately treated as not focused
/// so notifications still fire on terminals without focus reporting.
pub fn terminal_is_focused() -> bool {
    FOCUS.load(Ordering::Relaxed) == FOCUS_FOCUSED
}

/// Classify the controlling terminal from `$TERM_PROGRAM` (with a
/// Kitty fallback, which doesn't set `TERM_PROGRAM`). Pure so it's
/// unit-testable without touching the environment.
pub fn classify_terminal(term_program: &str, is_kitty: bool) -> Option<OscNotifier> {
    match term_program {
        "ghostty" | "WezTerm" => Some(OscNotifier::Osc777),
        "iTerm.app" => Some(OscNotifier::Osc9),
        _ if is_kitty => Some(OscNotifier::Osc777),
        _ => None,
    }
}

/// Detect the OSC dialect for this process's terminal, or `None` when
/// no OSC-notification-capable terminal is recognized.
pub fn detect_osc_notifier() -> Option<OscNotifier> {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let is_kitty = std::env::var_os("KITTY_WINDOW_ID").is_some();
    classify_terminal(&term_program, is_kitty)
}

/// Build the escape sequence for `notifier`, tmux-wrapped when
/// `in_tmux`. Pure; production callers pass
/// `std::env::var_os("TMUX").is_some()`.
pub fn osc_sequence(notifier: OscNotifier, title: &str, body: &str, in_tmux: bool) -> String {
    let seq = match notifier {
        OscNotifier::Osc777 => format!(
            "\x1b]777;notify;{};{}\x07",
            // The title is `;`-delimited from the body, so a `;`
            // inside it would shift the body into the wrong field.
            sanitize(title, true),
            sanitize(body, false),
        ),
        OscNotifier::Osc9 => {
            // No title field — fold it in so the banner isn't bodiless.
            let combined = match (title.is_empty(), body.is_empty()) {
                (false, false) => format!("{title} — {body}"),
                (false, true) => title.to_string(),
                (true, _) => body.to_string(),
            };
            format!("\x1b]9;{}\x07", sanitize(&combined, false))
        }
    };
    if in_tmux { wrap_tmux(&seq) } else { seq }
}

/// Emit a notification to stdout for the given dialect. Best-effort —
/// a closed or redirected stdout simply drops it.
pub fn emit_osc_notification(notifier: OscNotifier, title: &str, body: &str) {
    let seq = osc_sequence(notifier, title, body, std::env::var_os("TMUX").is_some());
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Strip control bytes that would terminate or corrupt the OSC string
/// (BEL, ESC, other C0). When `is_field` — an OSC 777 title, which is
/// `;`-delimited from the body — also neutralize `;`.
fn sanitize(s: &str, is_field: bool) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() {
                ' '
            } else if is_field && c == ';' {
                ','
            } else {
                c
            }
        })
        .collect()
}

/// Wrap a sequence in tmux's passthrough envelope: `ESC P tmux ;
/// <payload, every ESC doubled> ESC \`. Requires `allow-passthrough`
/// on the outer session (tmux 3.3a default-on).
fn wrap_tmux(seq: &str) -> String {
    let escaped = seq.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_terminals() {
        assert_eq!(
            classify_terminal("ghostty", false),
            Some(OscNotifier::Osc777)
        );
        assert_eq!(
            classify_terminal("WezTerm", false),
            Some(OscNotifier::Osc777)
        );
        assert_eq!(
            classify_terminal("iTerm.app", false),
            Some(OscNotifier::Osc9)
        );
        // Kitty doesn't set TERM_PROGRAM — recognized via KITTY_WINDOW_ID.
        assert_eq!(classify_terminal("", true), Some(OscNotifier::Osc777));
        // Terminal.app / unknown → no OSC support, falls back to subprocess.
        assert_eq!(classify_terminal("Apple_Terminal", false), None);
        assert_eq!(classify_terminal("", false), None);
    }

    #[test]
    fn osc777_carries_title_and_body() {
        let seq = osc_sequence(OscNotifier::Osc777, "Title", "Body", false);
        assert_eq!(seq, "\x1b]777;notify;Title;Body\x07");
    }

    #[test]
    fn osc9_folds_title_into_body() {
        let seq = osc_sequence(OscNotifier::Osc9, "Title", "Body", false);
        assert_eq!(seq, "\x1b]9;Title — Body\x07");
        // Body-only and title-only collapse cleanly.
        assert_eq!(
            osc_sequence(OscNotifier::Osc9, "", "Body", false),
            "\x1b]9;Body\x07"
        );
        assert_eq!(
            osc_sequence(OscNotifier::Osc9, "Title", "", false),
            "\x1b]9;Title\x07"
        );
    }

    #[test]
    fn sanitizes_control_bytes_and_field_separators() {
        // An ESC in the body must not be able to terminate the OSC
        // string early, and a `;` in an OSC 777 title must not shift
        // the body into the wrong field.
        let seq = osc_sequence(OscNotifier::Osc777, "a;b\x1bc", "x\x07y", false);
        assert_eq!(seq, "\x1b]777;notify;a,b c;x y\x07");
    }

    #[test]
    fn tmux_wrapping_doubles_inner_escapes() {
        let seq = osc_sequence(OscNotifier::Osc777, "T", "B", true);
        assert_eq!(seq, "\x1bPtmux;\x1b\x1b]777;notify;T;B\x07\x1b\\");
    }

    #[test]
    fn focus_state_is_tri_valued() {
        // Default (no report seen) must not be "focused", or
        // notifications would be silenced on terminals that never
        // report focus.
        assert!(!terminal_is_focused());
        set_terminal_focus(true);
        assert!(terminal_is_focused());
        set_terminal_focus(false);
        assert!(!terminal_is_focused());
    }
}
