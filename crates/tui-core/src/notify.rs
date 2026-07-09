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
//!   - **OSC 777** `ESC ] 777 ; notify ; TITLE ; BODY ST` — full
//!     title + body. Ghostty, Kitty, WezTerm.
//!   - **OSC 9** `ESC ] 9 ; BODY ST` — body only, no title field.
//!     iTerm2; the title is folded into the body.
//!
//! Both are terminated with **ST** (`ESC \`), not BEL (`0x07`). A BEL
//! terminator rings the host terminal's audible bell on every
//! notification — and notifications fire on every agent state change,
//! so a chatty agent rang it continuously (issue #135). ST is accepted
//! as an OSC terminator by all four target terminals (the OSC 52
//! clipboard path already relies on this), so the banner still lands
//! without the bell.
//!
//! Inside tmux the sequence is wrapped in a passthrough envelope so
//! it reaches the outer terminal — tmux must have `allow-passthrough`
//! enabled (the default since tmux 3.3a).
//!
//! Sequences are never written to stdout at the point of the
//! triggering event. The render loop flushes ratatui frames to the
//! same fd, and an escape sequence landing mid-frame (or a frame
//! flush splitting the escape) reaches the terminal malformed: the
//! payload paints as literal text on screen and the banner is lost
//! (issue #296). Instead [`queue_osc_notification`] parks the built
//! sequence and the TUI run loop writes it *between* frames, on the
//! render thread, via [`flush_pending_osc`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// Which OSC notification dialect the controlling terminal speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscNotifier {
    /// `ESC ] 777 ; notify ; TITLE ; BODY ST` — title + body.
    Osc777,
    /// `ESC ] 9 ; BODY ST` — body only.
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
            "\x1b]777;notify;{};{}\x1b\\",
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
            format!("\x1b]9;{}\x1b\\", sanitize(&combined, false))
        }
    };
    if in_tmux { wrap_tmux(&seq) } else { seq }
}

/// OSC sequences queued by [`queue_osc_notification`], awaiting the
/// run loop's between-frames [`flush_pending_osc`].
static PENDING_OSC: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Queue a notification for the given dialect. The sequence is built
/// here (tmux-wrapped when `$TMUX` is set) but written to stdout only
/// by [`flush_pending_osc`], between frames on the render thread —
/// writing from the point of the triggering event could interleave
/// the escape bytes with a ratatui frame flush.
pub fn queue_osc_notification(notifier: OscNotifier, title: &str, body: &str) {
    let seq = osc_sequence(notifier, title, body, std::env::var_os("TMUX").is_some());
    if let Ok(mut pending) = PENDING_OSC.lock() {
        pending.push(seq);
    }
}

/// Drain the queued sequences in FIFO order.
pub fn take_pending_osc() -> Vec<String> {
    PENDING_OSC
        .lock()
        .map(|mut pending| std::mem::take(&mut *pending))
        .unwrap_or_default()
}

/// Write queued sequences to stdout. Called by the TUI run loop after
/// the render step, so the escape bytes are serialized with frame
/// output on the same thread. Best-effort — a closed or redirected
/// stdout simply drops them.
pub fn flush_pending_osc() {
    let pending = take_pending_osc();
    if pending.is_empty() {
        return;
    }
    use std::io::Write;
    let mut out = std::io::stdout();
    for seq in &pending {
        let _ = out.write_all(seq.as_bytes());
    }
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
        assert_eq!(seq, "\x1b]777;notify;Title;Body\x1b\\");
    }

    #[test]
    fn osc9_folds_title_into_body() {
        let seq = osc_sequence(OscNotifier::Osc9, "Title", "Body", false);
        assert_eq!(seq, "\x1b]9;Title — Body\x1b\\");
        // Body-only and title-only collapse cleanly.
        assert_eq!(
            osc_sequence(OscNotifier::Osc9, "", "Body", false),
            "\x1b]9;Body\x1b\\"
        );
        assert_eq!(
            osc_sequence(OscNotifier::Osc9, "Title", "", false),
            "\x1b]9;Title\x1b\\"
        );
    }

    #[test]
    fn sanitizes_control_bytes_and_field_separators() {
        // An ESC in the body must not be able to terminate the OSC
        // string early, and a `;` in an OSC 777 title must not shift
        // the body into the wrong field.
        let seq = osc_sequence(OscNotifier::Osc777, "a;b\x1bc", "x\x07y", false);
        assert_eq!(seq, "\x1b]777;notify;a,b c;x y\x1b\\");
    }

    #[test]
    fn tmux_wrapping_doubles_inner_escapes() {
        let seq = osc_sequence(OscNotifier::Osc777, "T", "B", true);
        // The inner OSC is now ST-terminated (`ESC \`), so its ESC is
        // doubled by the passthrough envelope alongside the introducer.
        assert_eq!(seq, "\x1bPtmux;\x1b\x1b]777;notify;T;B\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn never_emits_an_audible_bell() {
        // Regression for issue #135: notifications fire on every agent
        // state change, and a BEL (0x07) terminator rang the host's
        // audible bell each time — disruptive, and on some hosts the
        // bell handling swallowed the keystroke being typed. The OSC
        // must terminate with ST and carry no BEL anywhere, including
        // when a caller smuggles one in via the title/body or inside a
        // tmux passthrough envelope.
        for in_tmux in [false, true] {
            for notifier in [OscNotifier::Osc777, OscNotifier::Osc9] {
                let seq = osc_sequence(notifier, "ti\x07tle", "bo\x07dy", in_tmux);
                assert!(
                    !seq.contains('\x07'),
                    "OSC notification must not contain BEL: {seq:?}"
                );
                assert!(seq.ends_with("\x1b\\"), "must be ST-terminated: {seq:?}");
            }
        }
    }

    #[test]
    fn queued_notifications_drain_in_order() {
        // Regression for issue #296: notifications must be parked for
        // the render thread, not written to stdout at the point of the
        // triggering event, or the escape bytes can interleave with a
        // frame flush and paint as literal junk. Content assertions
        // use `contains` because the built sequence is tmux-wrapped
        // when the test itself runs under tmux.
        queue_osc_notification(OscNotifier::Osc777, "First", "one");
        queue_osc_notification(OscNotifier::Osc9, "Second", "two");
        let drained = take_pending_osc();
        assert_eq!(drained.len(), 2);
        assert!(drained[0].contains("777;notify;First;one"));
        assert!(drained[1].contains("9;Second — two"));
        assert!(take_pending_osc().is_empty(), "drain must empty the queue");
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
