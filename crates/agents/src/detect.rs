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
    // Always `Some`. Strip + compact once and hand both forms to the
    // classifier; every path classifies, so the caller can diff against
    // the cached state to notice transitions.
    let s = strip_ansi_lossy(recent_output);
    Some(claude_state_of(&s, &compact_lower(&s)))
}

/// Classify Claude's state from an already-stripped buffer `s` and its
/// space-free form `compact` (see [`compact_lower`]). Returns
/// `AgentState` directly (never the outer `Option`): every path
/// classifies. Both forms are passed in so the public entry points build
/// them once and share them — this runs on every PTY chunk.
///
/// Why two forms:
/// - `compact` (lowercased, spaces removed) is what every footer /
///   status-bar marker is matched against. tmux/Claude paint the bottom
///   status bar by absolute cursor position, so a footer phrase
///   (`? for shortcuts`, `Esc to cancel`, the `esc to interrupt` hint)
///   reaches pilot as `?forshortcuts` / `esctocancel` / `esctointerrupt`
///   — the inter-word gaps are cursor moves, not space bytes — while the
///   SAME phrase in scrollback keeps its spaces. Comparing the space-free
///   form matches either rendering; the spaced literal silently never
///   matched the live footer, which is why readiness once never signalled
///   and the injector rode its 10s deadline. All recency offsets are
///   computed in `compact` so they're directly comparable.
/// - raw `s` is kept only for the arrow scan, whose ASCII form (`> 1.`)
///   depends on the space `compact` strips.
fn claude_state_of(s: &str, compact: &str) -> AgentState {
    let idle_pos = idle_box_pos(compact);

    // A live chooser / permission dialog REPLACES the input box, so its
    // markers are the bottom-most thing on screen. When the idle
    // composer footer is MORE RECENT (further down the buffer) than the
    // chooser markers, the arrow + numbered shape is scrollback — a
    // parked `❯` prompt sitting in the composer above an agent's prose
    // numbered list — NOT a live chooser. That combination was the
    // dominant false positive: a `❯` in the composer plus any `1.`/`2.`
    // list in the conversation fired InputNeeded on an idle screen.
    //
    // Gating on recency (chooser at least as recent as the composer
    // footer) is also strictly better than the old `Tab to amend`
    // absence heuristic for the permission branch: it FIRES correctly
    // when a real dialog renders below a now-stale composer footer still
    // sitting in the detect window.
    let chooser_live = chooser_pos(compact).is_some_and(|cp| idle_pos.is_none_or(|ip| cp >= ip));

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
    //
    // The shape match alone is not enough (issue #164): the idle composer
    // draws its OWN `❯` prompt glyph, so a turn that ends with a numbered
    // list in the agent's summary (`1. … 2. …`) above the input box
    // satisfies arrow+chooser even though nothing is being asked. The
    // `chooser_live` recency gate (computed above) discards that case —
    // when the idle footer is the more-recent bottom marker the chooser
    // shape is stale chat and the turn is over.
    let has_arrow = s.contains('❯') || has_ascii_chooser_arrow(s);
    let has_chooser = has_numbered_chooser_options(s);
    if has_arrow && has_chooser && chooser_live {
        return AgentState::InputNeeded;
    }

    // Permission-chooser footer: `Esc to cancel` rendered alongside a
    // numbered-options shape. The permission dialog uses a SHORTER
    // footer than the input box ("Esc to cancel" alone, without "Tab to
    // amend"), so the input-box paired check below doesn't fire. Matched
    // space-free (`esctocancel`) because the footer arrives spaceless
    // from the status bar. The `chooser_live` recency gate handles the
    // input-box footer's own `Esc to cancel`: when that footer is the
    // most recent marker the numbered shape is scrollback and the branch
    // stays silent. This recency gate replaces the brittle `Tab to amend`
    // absence heuristic and also FIRES correctly when a real dialog
    // renders below a now-stale composer footer still in the detect
    // window.
    if compact.contains("esctocancel") && has_chooser && chooser_live {
        return AgentState::InputNeeded;
    }

    // Standalone high-confidence prompts. These exact phrases appear
    // only when claude is blocking on user input. Matched space-free so
    // phrasing variants and the footer's stripped spacing both fire
    // without expanding the table.
    if contains_any_compact(compact, CLAUDE_STANDALONE_PROMPT_PHRASES) {
        return AgentState::InputNeeded;
    }

    // Paired fallback for bare yes/no prompts (no arrow UI) + older
    // prompt shapes. Question phrases ANY-ed with choice markers — both
    // must be present in the buffer.
    if contains_any_compact(compact, &["1. Yes", "1) Yes", "(y/n)", "[y/n]"])
        && contains_any_compact(
            compact,
            &[
                "Do you want",
                "Allow Claude",
                "Allow Bash",
                "Approve",
                "Continue?",
                "Proceed?",
            ],
        )
    {
        return AgentState::InputNeeded;
    }

    // Working: the model is streaming or a tool is running. Claude
    // paints a live status line ONLY while busy. Recency guard: the
    // detection buffer is the append-only PTY byte stream, so a finished
    // agent's last status line still sits in it — but Claude redraws the
    // idle input box AFTER it. Treat the agent as working only when the
    // status line is the MORE RECENT of the two bottom-line markers.
    // Both positions are computed in `compact` so their relative order
    // (all the recency guard needs) is preserved; `idle_pos` is the
    // composer-footer offset already computed above.
    if let Some(work_pos) = working_status_pos(compact)
        && idle_pos.is_none_or(|ip| work_pos > ip)
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
    // Strip + compact once and share both with the state classifier —
    // this runs on every output chunk while the spawn-time injector polls
    // for readiness, so re-stripping or re-compacting the (up to 16 KiB)
    // buffer is wasted work on a hot path.
    let s = strip_ansi_lossy(recent_output);
    let compact = compact_lower(&s);
    // A live chooser / permission / standalone-consent prompt means a
    // paste would be eaten by the gate. The state model already
    // recognises every one of those shapes.
    if claude_state_of(&s, &compact) == AgentState::InputNeeded {
        return false;
    }
    // Folder-trust prompts don't always render as a numbered chooser
    // (older builds, alt phrasings), so `claude_state` can read them as
    // Idle. Veto explicitly — pasting the work prompt into the trust
    // dialog is the original "y eats my prompt" race. Matched space-free
    // like every other footer phrase.
    if compact.contains("trustthisfolder") || compact.contains("doyoutrustthefiles") {
        return false;
    }
    // The composer must actually be on screen. The boot banner is also
    // `Idle`, so requiring a composer footer marker excludes it — we
    // don't want to paste into the banner before the input box exists.
    input_box_visible(&compact)
}

/// Whether Claude's input box (composer) footer is on screen. Claude
/// draws one of these footers ONLY once it's done streaming and waiting
/// at the prompt: the long form `Esc to cancel · Tab to amend · …` or
/// the short newer form `? for shortcuts`. Either is proof the composer
/// is drawn. `compact` is the space-free buffer (see [`compact_lower`])
/// because the live footer is painted by absolute cursor position and
/// arrives spaceless (`?forshortcuts`).
fn input_box_visible(compact: &str) -> bool {
    compact.contains("tabtoamend") || compact.contains("?forshortcuts")
}

/// Lowercased copy of `s` with ASCII spaces removed. tmux/Claude render
/// the bottom status bar by absolute cursor position, so a footer phrase
/// reaches pilot with its inter-word gaps as cursor moves rather than
/// space bytes (`? for shortcuts` → `?forshortcuts`), while the same
/// phrase printed into scrollback keeps its spaces. Matching against
/// this form catches a marker in either rendering. Newlines are
/// preserved so the per-line working-counter scan still sees line
/// boundaries.
fn compact_lower(s: &str) -> String {
    s.chars()
        .filter(|c| *c != ' ')
        .flat_map(char::to_lowercase)
        .collect()
}

/// [`contains_any`] against the space-free buffer, compacting each
/// pattern the same way (lowercase, spaces removed) so a human-readable
/// constant table written WITH spaces matches the spaceless wire form.
///
/// One scratch buffer is reused across patterns (cleared, not
/// reallocated) so a match over an N-entry table costs roughly one
/// allocation total rather than N — this is on the per-chunk detection
/// hot path.
fn contains_any_compact(compact: &str, patterns: &[&str]) -> bool {
    let mut needle = String::new();
    patterns.iter().any(|p| {
        needle.clear();
        needle.extend(p.chars().filter(|c| *c != ' ').flat_map(char::to_lowercase));
        compact.contains(needle.as_str())
    })
}

/// Detect Claude's ASCII selection-arrow at ANY numbered option:
/// `> 0.`–`> 9.` or `> 0)`–`> 9)`. `windows(4)` walks the byte stream
/// once with no allocations. ASCII-only sentinels (`>`, ` `, digit, `.`,
/// `)`) are safe to scan against raw bytes — these values never appear
/// inside a multi-byte UTF-8 sequence.
///
/// Scanned against the RAW (un-compacted) buffer: the arrow is `>` then a
/// literal space, and [`compact_lower`] strips exactly that space, so the
/// shape only survives pre-compaction. The chooser-recency anchor
/// therefore leans on the numbered options (which survive compaction),
/// not this arrow — see [`chooser_pos`].
fn has_ascii_chooser_arrow(s: &str) -> bool {
    s.as_bytes().windows(4).any(|w| {
        w[0] == b'>' && w[1] == b' ' && w[2].is_ascii_digit() && (w[3] == b'.' || w[3] == b')')
    })
}

/// Byte offset of the most recent chooser marker — the unicode selection
/// arrow `❯` or a numbered option (`1.`/`1)`/`2.`/`2)`) — or `None` when
/// no chooser shape is present. The MAX of the marker offsets is the
/// recency anchor compared against the idle composer footer in
/// [`claude_state_of`]: a live chooser's markers sit below (after) any
/// stale footer, while a parked-prompt arrow above a prose list sits
/// above it.
///
/// Called only with the compacted buffer, so it shares
/// [`last_option_marker_pos`]'s digit guard — a version/decimal `N.`
/// (`v2.1.159`) is NOT a marker, matching the `has_chooser` gate exactly
/// so the two can't disagree. The ASCII arrow `> N.` is intentionally
/// absent here: its space is gone post-compaction, so the numbered
/// options carry the anchor instead.
fn chooser_pos(compact: &str) -> Option<usize> {
    [
        compact.rfind('❯'),
        last_option_marker_pos(compact, b'1'),
        last_option_marker_pos(compact, b'2'),
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Numbered-chooser shape: at least one `1.` / `1)` AND at least one
/// `2.` / `2)` option marker in the buffer (see [`last_option_marker_pos`]
/// for the digit guard that excludes version/decimal numbers). The
/// required `2.` / `2)` is what distinguishes a chooser from a chat list
/// that merely happens to start with `1.`.
fn has_numbered_chooser_options(s: &str) -> bool {
    has_option_marker(s, b'1') && has_option_marker(s, b'2')
}

/// True if `s` contains an option marker for `digit` — see
/// [`last_option_marker_pos`] for the exact shape and the digit guard.
fn has_option_marker(s: &str, digit: u8) -> bool {
    last_option_marker_pos(s, digit).is_some()
}

/// Byte offset of the LAST `<digit>.` / `<digit>)` whose delimiter is not
/// immediately followed by another ASCII digit (which would make it part
/// of a number rather than an option label), or `None`.
///
/// The digit guard is what keeps a decimal or dotted version number from
/// reading as a chooser: Claude's boot banner `Claude Code v2.1.159` has
/// `2.1`/`1.1` that satisfy a bare `1.`+`2.` test, and the composer prompt
/// glyph `❯` satisfies the arrow test — so a freshly-spawned IDLE composer
/// (banner still in the detection window) once misread as a live chooser →
/// `InputNeeded`, masking Working/Idle and blocking spawn-time readiness.
/// A real option (`1. Yes`, `2. No`) is followed by a space or letter,
/// never a digit, so the guard keeps every genuine chooser while rejecting
/// `7.2k`, `4.8`, and `2.1.159`. `rposition` returns the most recent match
/// so the offset is comparable against the other recency anchors.
/// ASCII-only sentinels are safe against raw bytes — none appear inside a
/// multi-byte UTF-8 sequence.
fn last_option_marker_pos(s: &str, digit: u8) -> Option<usize> {
    let b = s.as_bytes();
    b.windows(2).enumerate().rposition(|(i, w)| {
        w[0] == digit
            && (w[1] == b'.' || w[1] == b')')
            && !b.get(i + 2).is_some_and(u8::is_ascii_digit)
    })
}

/// Byte offset of the most recent live "working" status-line marker in
/// `compact` (the lowercased, space-free buffer — see [`compact_lower`]),
/// or `None` when nothing in the buffer says Claude is busy.
///
/// Claude renders a status line ONLY while streaming / running a tool:
/// `✦ Gusting… (2m 2s · ↓ 7.2k tokens · thinking some more)` or
/// `✻ Cogitating… (8s · ↑ 412 tokens · esc to interrupt)`. We anchor on
/// its two stable shapes — the `esc to interrupt` interrupt hint (which
/// arrives spaceless as `esctointerrupt` when Claude paints it in the
/// status bar), and the `(<elapsed> · … tokens …)` live counter (a
/// single line carrying both the `·` separator and the word `tokens`).
/// Requiring `·` and `tokens` on the SAME line avoids a cross-line false
/// match.
fn working_status_pos(compact: &str) -> Option<usize> {
    let interrupt = compact.rfind("esctointerrupt");
    let counter = last_line_pos(compact, |l| l.contains('·') && l.contains("tokens"));
    [interrupt, counter].into_iter().flatten().max()
}

/// Byte offset of the most recent idle input-box footer in `compact`.
/// Claude draws this footer only once it's done and waiting at the
/// prompt, so it's the recency anchor that beats a stale status line
/// still sitting in the append-only buffer.
fn idle_box_pos(compact: &str) -> Option<usize> {
    [compact.rfind("tabtoamend"), compact.rfind("?forshortcuts")]
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
    // Happy path (valid UTF-8, the norm) moves the buffer into the String
    // with no copy; only a malformed chunk pays for the lossy fallback.
    match String::from_utf8(filtered) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
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
    fn strip_replaces_invalid_utf8_without_panicking() {
        // The lossy fallback arm: a stray continuation byte (0x80) is not
        // valid UTF-8 and must become U+FFFD, not panic or truncate.
        assert_eq!(strip_ansi_lossy(b"ab\x80cd"), "ab\u{fffd}cd");
        // Valid multi-byte glyphs still survive the (happy) move path.
        assert_eq!(strip_ansi_lossy("héllo".as_bytes()), "héllo");
    }

    #[test]
    fn ready_vetoes_trust_folder_prompt_matched_space_free() {
        // The trust dialog's footer arrives spaceless from the status bar;
        // the veto must still fire so a paste can't land in it.
        assert!(!claude_ready_for_prompt(
            b"Do you trust the files in this folder?\n? for shortcuts"
        ));
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
    fn numbered_chooser_ignores_version_and_decimal_numbers() {
        // The `N.` of a version string / decimal is followed by another
        // digit, so it must NOT read as an option label. Claude's boot
        // banner carries `Claude Code v2.1.159`, which together with the
        // composer's `❯` prompt glyph used to misfire an idle composer
        // as a live chooser.
        assert!(!has_numbered_chooser_options("Claude Code v2.1.159"));
        assert!(!has_numbered_chooser_options(
            "Opus 4.8 · 1.2k tokens · 7.2k"
        ));
        // A genuine chooser whose options happen to sit beside a version
        // banner still fires — the option `N.` is followed by a space.
        assert!(has_numbered_chooser_options("v2.1.159\n1. Yes\n2. No"));
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
    fn chooser_recency_distinguishes_parked_prompt_from_live_chooser() {
        // Parked `❯` prompt in the composer ABOVE a prose numbered list,
        // with the composer footer the most recent marker → scrollback,
        // not a live chooser → Idle. (The dominant #156 false positive.)
        let parked = "Here's the plan:\n1. refactor\n2. test\n❯ ship it\n? for shortcuts";
        assert_eq!(claude_state(parked.as_bytes()), Some(AgentState::Idle));

        // The SAME markers, but the chooser renders BELOW a now-stale
        // composer footer → it's live → InputNeeded.
        let live = "? for shortcuts\nAllow Bash this command?\n❯ 1. Yes\n2. No\nEsc to cancel";
        assert_eq!(claude_state(live.as_bytes()), Some(AgentState::InputNeeded));

        // No composer footer at all (full-screen dialog) → always live.
        let bare = "Allow Bash?\n❯ 1. Yes\n2. No\nEsc to cancel";
        assert_eq!(claude_state(bare.as_bytes()), Some(AgentState::InputNeeded));
    }

    #[test]
    fn chooser_pos_is_the_most_recent_marker() {
        // `chooser_pos` is only ever called with the compacted (space-free)
        // buffer, so these cases are shaped that way.
        assert_eq!(chooser_pos("nomarkershere"), None);
        // Arrow after the numbered options → arrow offset wins.
        let s = "1.a\n2.b\n❯x";
        assert_eq!(chooser_pos(s), s.rfind('❯'));
        // Version/decimal `N.` is NOT a marker — shares the digit guard
        // with `has_option_marker`, so it can't disagree with `has_chooser`.
        assert_eq!(chooser_pos("opus4.8·v2.1.159"), None);
        // An ASCII-only chooser still anchors on its numbered options once
        // the arrow's space has been compacted away.
        let ascii = "allowbash?\n>1.yes\n>2.no";
        assert_eq!(chooser_pos(ascii), ascii.rfind("2."));
    }

    #[test]
    fn working_status_position_reflects_recency_order() {
        // The recency guard in `claude_state_of` compares these two
        // offsets in the space-free buffer, as the callers pass it.
        // Status line AFTER the idle footer → working is the more recent.
        let work_recent =
            compact_lower("esc to cancel · tab to amend\n✻ (8s · 412 tokens · esc to interrupt)");
        assert!(working_status_pos(&work_recent) > idle_box_pos(&work_recent));
        // Idle footer AFTER a now-stale status line → footer is the more
        // recent, so the agent reads as done rather than forever-busy.
        let idle_recent = compact_lower("✻ (esc to interrupt)\ndone\n? for shortcuts");
        assert!(working_status_pos(&idle_recent) < idle_box_pos(&idle_recent));
    }

    #[test]
    fn compact_lower_strips_spaces_and_lowercases_but_keeps_newlines() {
        assert_eq!(compact_lower("? For Shortcuts"), "?forshortcuts");
        assert_eq!(compact_lower("Esc to cancel"), "esctocancel");
        assert_eq!(compact_lower("a b\nc d"), "ab\ncd");
    }

    #[test]
    fn contains_any_compact_matches_across_stripped_spacing() {
        // The live footer arrives spaceless; the constant table is
        // written with spaces. Both compact to the same form.
        let compact = compact_lower("…doyouwanttoproceed?…");
        assert!(contains_any_compact(&compact, &["Do you want to proceed?"]));
        assert!(!contains_any_compact(&compact, &["Do you want to delete"]));
    }
}
