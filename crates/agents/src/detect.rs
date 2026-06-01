//! Pure, testable PTY-output detection over `&[u8]`.
//!
//! pilot wraps every agent in tmux and infers the agent's state by
//! screen-scraping the PTY byte stream. tmux paints by absolute cursor
//! position, so the bytes pilot sees arrive ANSI-laden and temporally
//! reordered — a single visual line can land as
//! `<cursor-move>❯<cursor-move> <cursor-move>1.<…>Yes`. Detection is
//! therefore inherently heuristic; the only way to keep it honest is to
//! make every step a pure function with no IO so it can be exercised
//! against captured real bytes (see `tests/detect_fixtures.rs`) as well
//! as synthetic strings.
//!
//! Everything here takes raw `&[u8]` (or its `strip_ansi_lossy`'d
//! `&str`) and returns a value — no `self`, no mutation, no clock. The
//! `Agent` impls in [`crate::agent`] are thin wrappers that call into
//! these functions.

use pilot_ipc::AgentState;

/// Standard bare yes/no prompt markers. Used by every CLI that doesn't
/// have a custom approval UI (Codex, Cursor, most GenericCli configs).
/// Order doesn't matter — substring search.
pub const YN_PROMPT_PATTERNS: &[&str] = &["[y/n]", "(y/n)", "[Y/n]", "[y/N]"];

/// Standalone phrases that are unambiguously Claude blocking on user
/// input — no chat context realistically produces them, so matching one
/// is enough confidence to flag `InputNeeded`. Lowercase; matched
/// against the lowercased buffer so phrasing variants ("Do you want to
/// Proceed?" vs "Do you want to proceed?") both fire without expanding
/// the table.
///
/// Each entry maps to one prompt shape. Keep in sync with the
/// fixture-driven test suite so every documented shape has explicit
/// coverage. Shapes covered, by tool:
///   - Write tool          → "do you want to create"
///   - Edit / MultiEdit    → "do you want to make this edit"
///   - file overwrites     → "do you want to overwrite"
///   - bash / file deletes → "do you want to delete"
///   - Bash approval       → "do you want to allow"
///   - plan-mode exit      → "do you want to proceed?"
///   - continue chat       → "do you want to continue?"
///   - apply diff/patch    → "do you want to apply"
///   - tool consent        → "do you want to enable"
///   - retry on failure    → "do you want to retry"
///   - settings edit       → "do you want to edit its own settings"
pub const CLAUDE_STANDALONE_PROMPT_PHRASES: &[&str] = &[
    "do you want to proceed?",
    "do you want to continue?",
    "do you want to apply",
    "do you want to allow",
    "do you want to enable",
    "do you want to retry",
    "do you want to create",
    "do you want to make this edit",
    "do you want to overwrite",
    "do you want to delete",
    "do you want to edit its own settings",
];

/// Substring "any-of" match. Plain text in; bytes should be passed
/// through [`strip_ansi_lossy`] first so escape sequences don't split
/// the markers.
pub fn contains_any(text: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|p| text.contains(p))
}

/// Two-stage check: at least one `choice` marker AND at least one
/// `question` phrase. Pairing raises confidence — neither alone is
/// enough to distinguish "agent is asking" from "the agent's chat
/// output mentions the same phrase."
pub fn contains_paired(text: &str, choices: &[&str], questions: &[&str]) -> bool {
    contains_any(text, choices) && contains_any(text, questions)
}

/// Claude Code's three observable states, in one detector. Ordered
/// most-specific first; the priority is deliberate — a live permission
/// chooser outranks the working status line, which outranks the quiet
/// input box.
///
/// - **`InputNeeded`** — ONLY structural prompt markers: the chooser
///   arrow + numbered options, the `Esc to cancel` permission footer
///   (without the input box's `Tab to amend`), the standalone
///   `do you want to …` consent phrases, or a bare yes/no pairing.
///   Freeform conversational asks ("Want me to …?", a line that merely
///   ends in `?`) are NOT flagged — they were the dominant
///   false-positive source and fired spurious desktop notifications.
/// - **`Working`** — Claude paints a live status line ONLY while busy:
///   `✦ Gusting… (2m 2s · ↓ 7.2k tokens · …)`. Its presence is the
///   reliable "working" pulser — far more robust than the old
///   `esc to interrupt`-only match, which the newer phase suffix
///   sometimes replaces.
/// - **`Idle`** — the input box is drawn with nothing pending, or the
///   output is plain non-interactive text.
///
/// Returning `Some(_)` on every path (rather than `None`) lets the
/// daemon notice every transition between the three states; without it
/// the cached state would stick at its last value.
pub fn claude_state(recent_output: &[u8]) -> Option<AgentState> {
    // Always `Some`. The inner `&str` form lets `claude_ready_for_prompt`
    // reuse the classification without stripping ANSI a second time.
    Some(claude_state_of(&strip_ansi_lossy(recent_output)))
}

/// [`claude_state`] over an already-ANSI-stripped buffer. Returns
/// `AgentState` directly (never the outer `Option`): every path
/// classifies, so callers that hold the stripped string skip the
/// redundant strip + decode.
fn claude_state_of(s: &str) -> AgentState {
    // tmux paints the screen by absolute cursor position, so the bytes
    // pilot sees arrive in TEMPORAL order: a single visual line
    // `❯ 1. Yes` can land in the buffer as
    // `<cursor-move>❯<cursor-move> <cursor-move>1.<…>Yes` and
    // strip_ansi removes the CSI runs but NOT the absolute positioning
    // gap that may be filled with other content. So strict substring
    // matching for `❯ 1.` fails even when the prompt is visually on
    // screen.
    //
    // Generalized matcher: the arrow `❯` ANYWHERE in the buffer paired
    // with a numbered-choice shape (any `1.` / `1)` followed by a text
    // option AND any `2.` / `2)`) catches every chooser claude renders
    // today: permission prompts, tool-approval, multi-choice
    // continuations, custom yes/no — anything with at least two
    // numbered options. The `2.` / `2)` second requirement filters out
    // chat lists that happen to start with `1.` (the second option is
    // the giveaway: a chooser is always followed immediately by `2.`).
    //
    // ASCII arrow accepted on ANY option (`> 1.`, `> 2.`, `> 3.`, …) —
    // the user moves the cursor with j/k, and claude re-renders the
    // arrow at the new option.
    let has_arrow = s.contains('❯') || has_ascii_chooser_arrow(s);
    let has_chooser = has_numbered_chooser_options(s);
    if has_arrow && has_chooser {
        return AgentState::InputNeeded;
    }

    // Permission-chooser footer: `Esc to cancel` rendered alongside a
    // numbered-options shape. The permission dialog uses a SHORTER
    // footer than the input box ("Esc to cancel" alone, without "Tab to
    // amend"), so the input-box paired check below doesn't fire.
    //
    // CRITICAL: gate on the ABSENCE of `Tab to amend`. The idle input
    // box's footer is `Esc to cancel · Tab to amend · ctrl+e to
    // explain` — it ALSO contains `Esc to cancel`. A real permission
    // dialog REPLACES the input box, so its footer is the SHORT
    // `Esc to cancel` WITHOUT `Tab to amend`; requiring `Tab to amend`
    // to be absent is exactly the distinction the branch's design
    // always intended.
    if s.contains("Esc to cancel") && !s.contains("Tab to amend") && has_chooser {
        return AgentState::InputNeeded;
    }

    // Standalone high-confidence prompts. These exact phrases appear
    // only when claude is blocking on user input. Lowercase comparison
    // so phrasing variants both match without expanding the table.
    let lower = s.to_lowercase();
    if contains_any(&lower, CLAUDE_STANDALONE_PROMPT_PHRASES) {
        return AgentState::InputNeeded;
    }

    // Paired fallback for bare yes/no prompts (no arrow UI) + older
    // prompt shapes. Question phrases ANY-ed with choice markers — both
    // must be present in the buffer.
    if contains_paired(
        s,
        &["1. Yes", "1) Yes", "(y/n)", "[y/n]"],
        &[
            "Do you want",
            "Allow Claude",
            "Allow Bash",
            "Approve",
            "Continue?",
            "Proceed?",
        ],
    ) {
        return AgentState::InputNeeded;
    }

    // Working: the model is streaming or a tool is running. Claude
    // paints a live status line ONLY while busy. Recency guard: the
    // detection buffer is the append-only PTY byte stream, so a finished
    // agent's last status line still sits in it — but Claude redraws the
    // idle input box AFTER it. Treat the agent as working only when the
    // status line is the MORE RECENT of the two bottom-line markers.
    if let Some(work_pos) = working_status_pos(&lower)
        && idle_box_pos(&lower).is_none_or(|ip| work_pos > ip)
    {
        return AgentState::Working;
    }

    // Nothing pending and not streaming: the input box is drawn and
    // quiet, or the output is plain non-interactive text.
    AgentState::Idle
}

/// Claude is ready to receive a pasted prompt when its input box (the
/// composer) is drawn AND no permission chooser / Y-N gate / folder-
/// trust dialog is up.
///
/// This reuses the [`claude_state`] model rather than re-deriving the
/// prompt markers: readiness is "the composer is visible and the state
/// isn't `InputNeeded`." Deriving it from the shared state model is what
/// fixes the spawn-time injection delay — the old check required the
/// FULL idle footer (`Esc to cancel` AND `Tab to amend` paired), which
/// silently never matched the newer composer footer (`? for shortcuts`
/// alone) or a banner mid-render, so the ready signal never fired and
/// the injector rode its hard deadline. Keying off "composer drawn + not
/// asking" fires the moment Claude is genuinely idle at the prompt.
///
/// Returning true means "paste arrives in the input buffer, not as a
/// Y/N answer." Returning false means "wait — the banner isn't done OR a
/// gate is up."
pub fn claude_ready_for_prompt(recent_output: &[u8]) -> bool {
    // Strip once and share the result with the state classifier — this
    // runs on every output chunk while the spawn-time injector polls for
    // readiness, so a second strip of the (up to 16 KiB) buffer is wasted
    // work on a hot path.
    let s = strip_ansi_lossy(recent_output);
    // A live chooser / permission / standalone-consent prompt means a
    // paste would be eaten by the gate. The state model already
    // recognises every one of those shapes.
    if claude_state_of(&s) == AgentState::InputNeeded {
        return false;
    }
    let lower = s.to_lowercase();
    // Folder-trust prompts don't always render as a numbered chooser
    // (older builds, alt phrasings), so `claude_state` can read them as
    // Idle. Veto explicitly — pasting the work prompt into the trust
    // dialog is the original "y eats my prompt" race.
    if lower.contains("trust this folder") || lower.contains("do you trust the files") {
        return false;
    }
    // The composer must actually be on screen. The boot banner is also
    // `Idle`, so requiring a composer footer marker excludes it — we
    // don't want to paste into the banner before the input box exists.
    input_box_visible(&lower)
}

/// Whether Claude's input box (composer) footer is on screen. Claude
/// draws one of these footers ONLY once it's done streaming and waiting
/// at the prompt: the long form `Esc to cancel · Tab to amend · …` or
/// the short newer form `? for shortcuts`. Either is proof the composer
/// is drawn.
fn input_box_visible(lower: &str) -> bool {
    lower.contains("tab to amend") || lower.contains("? for shortcuts")
}

/// Detect Claude's ASCII selection-arrow at ANY numbered option:
/// `> 0.`–`> 9.` or `> 0)`–`> 9)`.
///
/// `windows(4)` walks the byte stream once with no allocations.
/// ASCII-only sentinels (`>`, ` `, digit, `.`, `)`) are safe to scan
/// against raw bytes — these values never appear inside a multi-byte
/// UTF-8 sequence.
fn has_ascii_chooser_arrow(s: &str) -> bool {
    s.as_bytes().windows(4).any(|w| {
        w[0] == b'>' && w[1] == b' ' && w[2].is_ascii_digit() && (w[3] == b'.' || w[3] == b')')
    })
}

/// Numbered-chooser shape: at least one `1.` / `1)` AND at least one
/// `2.` / `2)` in the buffer. The chooser is always followed by `2.` —
/// that's what distinguishes it from a chat list that happens to start
/// with `1.`.
fn has_numbered_chooser_options(s: &str) -> bool {
    (s.contains("1.") || s.contains("1)")) && (s.contains("2.") || s.contains("2)"))
}

/// Byte offset of the most recent live "working" status-line marker in
/// `lower` (the ANSI-stripped, lowercased buffer), or `None` when
/// nothing in the buffer says Claude is busy.
///
/// Claude renders a status line ONLY while streaming / running a tool:
/// `✦ Gusting… (2m 2s · ↓ 7.2k tokens · thinking some more)` or
/// `✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)`. We anchor on
/// its two stable shapes — the `esc to interrupt` interrupt hint, and
/// the `(<elapsed> · … tokens …)` live counter (a single line carrying
/// both the `·` separator and the word `tokens`). Requiring `·` and
/// `tokens` on the SAME line avoids a cross-line false match.
fn working_status_pos(lower: &str) -> Option<usize> {
    let interrupt = lower.rfind("esc to interrupt");
    let counter = last_line_pos(lower, |l| l.contains('·') && l.contains("tokens"));
    [interrupt, counter].into_iter().flatten().max()
}

/// Byte offset of the most recent idle input-box footer in `lower`.
/// Claude draws this footer only once it's done and waiting at the
/// prompt, so it's the recency anchor that beats a stale status line
/// still sitting in the append-only buffer.
fn idle_box_pos(lower: &str) -> Option<usize> {
    [lower.rfind("tab to amend"), lower.rfind("? for shortcuts")]
        .into_iter()
        .flatten()
        .max()
}

/// Start offset of the last line satisfying `pred`. Walks the buffer
/// once, keeping line boundaries (`split_inclusive`) so the returned
/// offset is comparable against `rfind` positions in the same string.
fn last_line_pos(s: &str, pred: impl Fn(&str) -> bool) -> Option<usize> {
    let mut best = None;
    let mut offset = 0;
    for line in s.split_inclusive('\n') {
        if pred(line) {
            best = Some(offset);
        }
        offset += line.len();
    }
    best
}

/// Filter out ANSI escape sequences, then UTF-8-decode the remainder.
///
/// Earlier this function pushed `bytes[i] as char` — that mangled
/// multi-byte UTF-8 glyphs like Claude's choice arrow `❯` (U+276F, 3
/// bytes) into three separate Latin-1 chars, so any pattern containing
/// the glyph silently failed to match. We keep the raw glyph bytes
/// untouched and decode once at the end so the patterns can search for
/// it literally.
///
/// Every `ESC`-introduced run is dropped wholesale via `skip_escape`;
/// the parser handles the four families tmux/Claude emit (CSI, OSC, the
/// DCS/SOS/PM/APC string family, and charset designators) so none of
/// their payload bytes leak into the searched text. Malformed or
/// truncated sequences are consumed safely to the end of the buffer
/// rather than panicking — a half-arrived chunk is the common case on a
/// live PTY.
pub fn strip_ansi_lossy(bytes: &[u8]) -> String {
    let mut filtered: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = skip_escape(bytes, i);
        } else {
            filtered.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&filtered).into_owned()
}

/// Advance past one escape sequence whose `ESC` (0x1b) is at `start`.
/// Returns the index of the first byte *after* the sequence — i.e. the
/// next byte the caller should treat as content. Recognises the four
/// families that actually reach pilot through tmux:
///
///   - **CSI** `ESC [ … <final 0x40–0x7e>` — SGR colour, cursor moves.
///   - **OSC** `ESC ] … <BEL | ST>` — window title, hyperlinks.
///   - **string** `ESC (P|X|^|_) … <BEL | ST>` — DCS / SOS / PM / APC.
///     tmux wraps app passthrough in DCS, so the payload must be dropped,
///     not leaked into the matched text as stray characters.
///   - **charset** `ESC (|)|*|+ <one byte>` — G0–G3 designators.
///
/// Anything else is treated as a two-byte escape (`ESC c`, `ESC =`,
/// `ESC 7`, …): drop `ESC` plus the single introducer. A lone trailing
/// `ESC` consumes just itself.
fn skip_escape(bytes: &[u8], start: usize) -> usize {
    debug_assert_eq!(bytes.get(start), Some(&0x1b));
    let mut i = start + 1;
    let Some(&intro) = bytes.get(i) else {
        return i; // lone trailing ESC
    };
    i += 1;
    match intro {
        b'[' => {
            // CSI: parameter / intermediate bytes until a final byte.
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1; // consume the final byte
            }
            i
        }
        b']' | b'P' | b'X' | b'^' | b'_' => skip_string_terminated(bytes, i),
        b'(' | b')' | b'*' | b'+' => (i + 1).min(bytes.len()), // + one charset byte
        _ => i,                                                // two-byte escape, already consumed
    }
}

/// Advance past the body of a string-terminated escape (OSC / DCS / SOS /
/// PM / APC) starting at `i` (the first byte after the introducer),
/// consuming the terminator. The body ends at BEL (0x07) or ST
/// (`ESC \`); an unterminated body runs to the end of the buffer.
fn skip_string_terminated(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            0x07 => return i + 1,                                     // BEL
            0x1b if bytes.get(i + 1) == Some(&b'\\') => return i + 2, // ST
            _ => i += 1,
        }
    }
    i
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the pure primitives. The agent-level tests in
    //! `tests/agents.rs` and the real-byte corpus in
    //! `tests/detect_fixtures.rs` cover composition and live wire shapes;
    //! these pin the building blocks — above all `strip_ansi_lossy`, the
    //! `pub` function every detector sits on, which until now was only
    //! exercised transitively.
    use super::*;

    #[test]
    fn strip_passes_plain_text_through_unchanged() {
        assert_eq!(strip_ansi_lossy(b""), "");
        assert_eq!(strip_ansi_lossy(b"hello world"), "hello world");
    }

    #[test]
    fn strip_removes_csi_sgr_and_cursor_moves() {
        // SGR colour + erase-line + cursor home, interleaved with text.
        assert_eq!(strip_ansi_lossy(b"\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi_lossy(b"\x1b[2Khello"), "hello");
        assert_eq!(strip_ansi_lossy(b"a\x1b[1;2Hb"), "ab");
    }

    #[test]
    fn strip_preserves_multibyte_glyphs() {
        // The regression this function was rewritten for: the chooser
        // arrow `❯` (U+276F, 3 bytes) must survive intact so patterns can
        // match it literally. Wrap it in CSI runs to mimic a real repaint.
        let raw = b"\x1b[38;5;2m\xe2\x9d\xaf\x1b[0m 1. Yes";
        assert_eq!(strip_ansi_lossy(raw), "❯ 1. Yes");
    }

    #[test]
    fn strip_removes_osc_terminated_by_bel_or_st() {
        // OSC 0 (window title) — both terminators occur in the wild.
        assert_eq!(strip_ansi_lossy(b"\x1b]0;title\x07keep"), "keep");
        assert_eq!(strip_ansi_lossy(b"\x1b]0;title\x1b\\keep"), "keep");
    }

    #[test]
    fn strip_drops_dcs_string_family_payload() {
        // tmux wraps app passthrough in DCS (`ESC P … ST`). Pre-hardening
        // the introducer was dropped but the payload leaked as text,
        // which could inject stray `tokens`/digits into the matched
        // buffer. The whole run — including the body — must vanish.
        assert_eq!(strip_ansi_lossy(b"\x1bPtmux;data\x1b\\visible"), "visible");
        // SOS / PM / APC share the string terminator.
        assert_eq!(strip_ansi_lossy(b"\x1b^priv\x07ok"), "ok");
        assert_eq!(strip_ansi_lossy(b"\x1b_app\x1b\\ok"), "ok");
    }

    #[test]
    fn strip_drops_charset_designator_second_byte() {
        // `ESC ( B` selects the ASCII charset for G0. Pre-hardening the
        // `B` leaked; now the full two-byte designator is dropped.
        assert_eq!(strip_ansi_lossy(b"\x1b(Bplain"), "plain");
        assert_eq!(strip_ansi_lossy(b"\x1b)0graphics"), "graphics");
    }

    #[test]
    fn strip_drops_two_byte_escapes() {
        // `ESC c` (full reset) and friends carry no payload.
        assert_eq!(strip_ansi_lossy(b"\x1bcfresh"), "fresh");
    }

    #[test]
    fn strip_handles_truncated_sequences_without_panicking() {
        // Half-arrived chunks are the common case on a live PTY. None of
        // these may panic; the dangling escape is consumed to the end.
        assert_eq!(strip_ansi_lossy(b"text\x1b"), "text"); // lone trailing ESC
        assert_eq!(strip_ansi_lossy(b"text\x1b[31"), "text"); // CSI, no final byte
        assert_eq!(strip_ansi_lossy(b"text\x1b]0;unterminated"), "text"); // OSC, no terminator
        assert_eq!(strip_ansi_lossy(b"text\x1bPdcs-no-st"), "text"); // DCS, no terminator
    }

    #[test]
    fn ascii_chooser_arrow_detects_any_option_and_ignores_short_buffers() {
        assert!(has_ascii_chooser_arrow("> 1. Yes"));
        assert!(has_ascii_chooser_arrow("foo\n> 3) No"));
        assert!(!has_ascii_chooser_arrow(">1.")); // no space — not the arrow
        assert!(!has_ascii_chooser_arrow("> a.")); // not a digit
        assert!(!has_ascii_chooser_arrow(">")); // shorter than the 4-byte window
    }

    #[test]
    fn numbered_chooser_requires_both_first_and_second_option() {
        assert!(has_numbered_chooser_options("1. Yes\n2. No"));
        assert!(has_numbered_chooser_options("1) a\n2) b"));
        assert!(!has_numbered_chooser_options("1. only one option")); // no `2.`/`2)`
    }

    #[test]
    fn last_line_pos_returns_start_offset_of_final_match() {
        // Two matching lines — the offset of the LATER one wins, and it's
        // a byte offset comparable against `rfind` in the same string.
        let s = "x marks\nfirst hit\nfiller\nsecond hit\n";
        let pos = last_line_pos(s, |l| l.contains("hit")).unwrap();
        assert_eq!(&s[pos..pos + "second hit".len()], "second hit");
        assert_eq!(last_line_pos(s, |l| l.contains("absent")), None);
    }

    #[test]
    fn working_status_position_reflects_recency_order() {
        // The recency guard in `claude_state_of` compares these two
        // offsets. Inputs are pre-lowercased, as the callers pass them.
        // Status line AFTER the idle footer → working is the more recent.
        let work_recent = "esc to cancel · tab to amend\n✻ (8s · 412 tokens · esc to interrupt)";
        assert!(working_status_pos(work_recent) > idle_box_pos(work_recent));
        // Idle footer AFTER a now-stale status line → footer is the more
        // recent, so the agent reads as done rather than forever-busy.
        let idle_recent = "✻ (esc to interrupt)\ndone\n? for shortcuts";
        assert!(working_status_pos(idle_recent) < idle_box_pos(idle_recent));
    }
}
