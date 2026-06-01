#!/usr/bin/env python3
"""Generate the real-byte detector fixtures committed alongside this script.

The synthetic string tests in `tests/agents.rs` feed the detector clean,
hand-typed buffers. Live tmux output looks nothing like that: tmux paints
the screen by absolute cursor position, so a single visual line arrives
ANSI-laden (SGR colour runs, `\x1b[2K` erase-line, `\x1b[<row>;<col>H`
cursor jumps) and temporally REORDERED — the arrow glyph, the option
text, and the status ticker can interleave arbitrarily.

These fixtures reproduce that wire shape: genuine escape sequences,
multi-byte glyphs (`❯` U+276F, `✻`, `·`, box-drawing), and fragmented
chooser lines. They exercise `strip_ansi_lossy` + the Claude state
machine over bytes that resemble what `PILOT_CAPTURE_PTY` dumps from a
real session, not idealised strings.

Regenerate with:  python3 generate.py
Then run:         cargo test -p pilot-agents --test detect_fixtures
"""

import os

OUT = os.path.dirname(os.path.abspath(__file__))

ESC = b"\x1b"
CLEAR_LINE = ESC + b"[2K"
HIDE_CUR = ESC + b"[?25l"
SHOW_CUR = ESC + b"[?25h"
RESET = ESC + b"[0m"


def sgr(*codes):
    return ESC + b"[" + b";".join(str(c).encode() for c in codes) + b"m"


def cup(row, col):
    """Absolute cursor position — the source of tmux temporal reordering."""
    return ESC + b"[%d;%dH" % (row, col)


def write(name, parts):
    data = b"".join(parts)
    with open(os.path.join(OUT, name), "wb") as f:
        f.write(data)
    print(f"{name}: {len(data)} bytes")


# ── idle: composer drawn, quiet, newer `? for shortcuts` footer ──────────
write("idle_composer.bin", [
    HIDE_CUR,
    cup(20, 1), CLEAR_LINE,
    sgr(2), "● Done — all tests pass.".encode("utf-8"), RESET, b"\r\n",
    cup(22, 1), CLEAR_LINE,
    sgr(90), "╭───────────────────────────────────────────╮".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "│".encode("utf-8"), RESET, b" > ",
    cup(23, 46), sgr(90), "│".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "╰───────────────────────────────────────────╯".encode("utf-8"), RESET, b"\r\n",
    cup(25, 3), sgr(2, 90), b"? for shortcuts", RESET,
    SHOW_CUR,
])

# ── working: live status line, spinner glyph, SGR colour, ticker rewrite ──
# The ticker rewrites the same row twice via absolute CUP — the stale first
# render stays in the append-only byte stream, exactly as the pump sees it.
write("working_status_line.bin", [
    cup(24, 1), CLEAR_LINE,
    sgr(35), "✻".encode("utf-8"), RESET, b" ",
    sgr(1), b"Cogitating", RESET,
    "… ".encode("utf-8"),
    sgr(2), "(7s · ↑ 318 tokens · esc to interrupt)".encode("utf-8"), RESET,
    # ticker tick — repaint the row in place
    cup(24, 1), CLEAR_LINE,
    sgr(35), "✦".encode("utf-8"), RESET, b" ",
    sgr(1), b"Gusting", RESET,
    "… ".encode("utf-8"),
    sgr(2), "(2m 2s · ↓ 7.2k tokens · thinking some more)".encode("utf-8"), RESET,
])

# ── permission prompt: chooser arrow + numbered options, FRAGMENTED ──────
# tmux emits the arrow, then a status ticker repaints elsewhere, then the
# numbered options land — the arrow and `1.` are NOT adjacent in the byte
# stream. The short `Esc to cancel` footer (no `Tab to amend`).
write("permission_prompt_fragmented.bin", [
    cup(10, 1), CLEAR_LINE,
    sgr(1), b"Allow Bash this command?", RESET, b"\r\n",
    cup(11, 1), CLEAR_LINE,
    "  ❯ ".encode("utf-8"),                       # arrow lands first…
    cup(24, 1), CLEAR_LINE,                        # …ticker repaints a far row
    sgr(2), "(3s · 41 tokens)".encode("utf-8"), RESET,
    cup(11, 5), sgr(7), b"1. Yes", RESET, b"\r\n",  # then the option text
    cup(12, 5), b"2. Yes, and don't ask again\r\n",
    cup(13, 5), b"3. No, and tell Claude what to do differently\r\n",
    cup(15, 1), sgr(2), b"Esc to cancel", RESET,
])

# ── trust-folder prompt: chooser, first-run, full-screen redraw ──────────
write("trust_folder_prompt.bin", [
    ESC + b"[2J", cup(1, 1),
    sgr(1, 33), b"Do you trust the files in this folder?", RESET, b"\r\n\r\n",
    cup(4, 1), os.fsencode("/Users/me/code/widget") + b"\r\n\r\n",
    cup(6, 3), "❯ 1. Yes, proceed".encode("utf-8"), b"\r\n",
    cup(7, 3), b"  2. No, exit", b"\r\n\r\n",
    cup(9, 3), sgr(2), b"Esc to cancel", RESET,
])

# ── conversational question: freeform `?`, NO structural marker → Idle ───
# The dominant historical false-positive. Composer footer below, no chooser.
write("conversational_question.bin", [
    cup(18, 1), CLEAR_LINE,
    sgr(2), "●".encode("utf-8"), RESET,
    b" I've finished the refactor. Want me to run the full suite now?\r\n",
    cup(20, 1), sgr(90), "╭─────────────────────────────────────────╮".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "│".encode("utf-8"), RESET, b" > ",
    cup(21, 44), sgr(90), "│".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "╰─────────────────────────────────────────╯".encode("utf-8"), RESET, b"\r\n",
    cup(23, 3), sgr(2, 90), b"? for shortcuts", RESET,
])

# ── finished: completion summary + a parked injected prompt in composer ──
# A `❯` sits inside the composer box but there are no numbered options, so
# it must NOT read as a chooser. End-of-turn `Brewed for` marker → Idle.
write("finished_with_parked_prompt.bin", [
    cup(16, 1), CLEAR_LINE,
    sgr(32), "●".encode("utf-8"), RESET,
    b" Pushed the fix. CI: https://github.com/o/r/actions/runs/123\r\n",
    cup(17, 1), sgr(35), "✻".encode("utf-8"), RESET,
    " Brewed for 4m 21s\r\n".encode("utf-8"),
    cup(19, 1), sgr(90), "╭──────────────────────────────────╮".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "│".encode("utf-8"), RESET,
    " ❯ watch CI until it passes        ".encode("utf-8"),
    sgr(90), "│".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "╰──────────────────────────────────╯".encode("utf-8"), RESET, b"\r\n",
    cup(22, 3), sgr(2, 90), b"? for shortcuts", RESET,
])

# ── numbered list above an idle composer: must NOT be read as a chooser ──
# Long idle footer carries BOTH `Esc to cancel` and `Tab to amend`; the
# scrollback has a `1.`/`2.` list. The permission-footer branch must stay
# silent (real dialogs drop `Tab to amend`).
write("idle_with_numbered_list.bin", [
    cup(14, 1), b"Here's the plan:\r\n",
    cup(15, 1), b"  1. Refactor the parser\r\n",
    cup(16, 1), b"  2. Add tests\r\n",
    cup(18, 1), sgr(90), "│".encode("utf-8"), RESET, b" > \r\n",
    cup(20, 1), sgr(2), "Esc to cancel · Tab to amend · ctrl+e to explain".encode("utf-8"), RESET,
])
