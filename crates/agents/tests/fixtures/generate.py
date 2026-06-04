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
machine over bytes that resemble what `LAZYBOX_CAPTURE_PTY` dumps from a
real session, not idealised strings.

Regenerate with:  python3 generate.py
Then run:         cargo test -p lazybox-agents --test detect_fixtures
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

# ── REAL idle ready screen (captured from claude 2.1.x) ──────────────────
# The genuine fresh-spawn idle screen, reproduced from a real LAZYBOX_CAPTURE_PTY
# dump. Two properties broke detection until #153:
#   1. The boot banner carries the version `Claude Code v2.1.159`; its
#      `2.1`/`1.1` decimals satisfied the bare `1.`+`2.` chooser test, and
#      the composer prompt glyph `❯` satisfied the arrow test — so an IDLE
#      composer misread as a live chooser (InputNeeded), masking Idle/Working
#      and blocking the readiness signal.
#   2. The composer footer is painted into the status bar by ABSOLUTE cursor
#      position, so `? for shortcuts` arrives with NO space bytes between the
#      tokens (`?forshortcuts`). The spaced literal never matched, readiness
#      never fired, and the injector rode its 10s deadline EVERY spawn.
# This fixture must report Idle AND ready.
def spaceless_footer():
    # Each token placed at an absolute column → strip_ansi yields the tokens
    # concatenated with no spaces, exactly as the live status bar arrives.
    return [
        cup(40, 1), CLEAR_LINE,
        sgr(2, 90), b"?", cup(40, 3), b"for", cup(40, 7), b"shortcuts", RESET,
        cup(40, 17), "·".encode("utf-8"),
        cup(40, 19), "←".encode("utf-8"), cup(40, 21), b"for", cup(40, 25), b"agents",
        cup(40, 90), "●".encode("utf-8"), cup(40, 92), b"high",
        cup(40, 97), "·".encode("utf-8"), cup(40, 99), b"/effort",
    ]


write("claude_real_idle_ready.bin", [
    HIDE_CUR,
    ESC + b"[2J", cup(1, 1),
    sgr(1, 34), "╭─── Claude Code v2.1.159 ─────────────────────────────╮".encode("utf-8"), RESET, b"\r\n",
    sgr(34), "│".encode("utf-8"), RESET, b"  Welcome back!   ",
    sgr(34), "│".encode("utf-8"), RESET, b"\r\n",
    sgr(34), "│".encode("utf-8"), RESET, b"  Opus 4.8 (1M context)  ",
    sgr(34), "│".encode("utf-8"), RESET, b"\r\n",
    sgr(34), "╰──────────────────────────────────────────────────────╯".encode("utf-8"), RESET, b"\r\n",
    cup(20, 1), CLEAR_LINE,
    "────────────────────────────────────────────────────────".encode("utf-8"), b"\r\n",
    cup(21, 1), "❯ ".encode("utf-8"),
    sgr(2), b"Try \"fix lint errors\"", RESET, b"\r\n",
    cup(22, 1), "────────────────────────────────────────────────────────".encode("utf-8"), b"\r\n",
    *spaceless_footer(),
    SHOW_CUR,
])

# ── REAL active-work transcript (captured from claude 2.1.x) ─────────────
# A genuinely-streaming session: bash output scrolling, the live status
# line (`✻ … (Ns · ↓ N tokens)`), the spaceless interrupt hint in the
# status bar, AND — crucially — the same version banner + composer `❯` that
# used to misfire InputNeeded. Must report Working, not InputNeeded, and
# must NOT report ready.
write("claude_real_working_statusbar.bin", [
    cup(1, 1),
    sgr(1, 34), b"Claude Code v2.1.159", RESET, b"\r\n",
    cup(6, 1), "⏺".encode("utf-8"), b" Bash(for i in 1 2 3; do echo step $i; sleep 1; done)\r\n",
    cup(7, 1), b"  step 1\r\n",
    cup(8, 1), b"  step 2\r\n",
    cup(20, 1), CLEAR_LINE,
    sgr(35), "✻".encode("utf-8"), RESET, b" ",
    sgr(1), b"Simmering", RESET,
    "… ".encode("utf-8"),
    sgr(2), "(10s · ↓ 137 tokens)".encode("utf-8"), RESET, b"\r\n",
    cup(22, 1), "────────────────────────────────────────────────────────".encode("utf-8"), b"\r\n",
    cup(23, 1), "❯ ".encode("utf-8"), b"\r\n",
    cup(24, 1), "────────────────────────────────────────────────────────".encode("utf-8"), b"\r\n",
    # interrupt hint painted spaceless into the status bar
    cup(40, 1), CLEAR_LINE,
    sgr(2), b"esc", cup(40, 4), b"to", cup(40, 6), b"interrupt", RESET,
    cup(40, 90), "●".encode("utf-8"), cup(40, 92), b"high",
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

# ── FALSE POSITIVE (#156): prose numbered list + a parked `❯` prompt ──────
# The screen is idle: the agent printed a numbered list in its prose and a
# `❯`-prefixed prompt is parked in the composer awaiting the user. The old
# detector saw `❯` (anywhere) + `1.`/`2.` (anywhere) and fired InputNeeded
# on this quiet screen. The composer footer (`? for shortcuts`) is the most
# recent bottom-of-screen marker, so a live chooser cannot be up — it must
# read Idle.
write("false_positive_prose_list.bin", [
    cup(12, 1), CLEAR_LINE,
    sgr(2), "●".encode("utf-8"), RESET, b" Here's the rollout plan:\r\n",
    cup(13, 1), b"  1. Ship the parser refactor\r\n",
    cup(14, 1), b"  2. Backfill the fixtures\r\n",
    cup(15, 1), b"  3. Flip the flag\r\n",
    cup(18, 1), sgr(90), "╭──────────────────────────────────╮".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "│".encode("utf-8"), RESET,
    " ❯ ship it when CI is green        ".encode("utf-8"),
    sgr(90), "│".encode("utf-8"), RESET, b"\r\n",
    sgr(90), "╰──────────────────────────────────╯".encode("utf-8"), RESET, b"\r\n",
    cup(21, 3), sgr(2, 90), b"? for shortcuts", RESET,
])

# ── FALSE NEGATIVE GUARD (#156): live chooser BELOW a stale composer ──────
# A real permission dialog renders below a now-stale composer footer still
# sitting in the detect window (the user typed, Claude went to work, then
# hit a permission gate — all within one 16 KiB window). The chooser
# markers are MORE RECENT than the stale `? for shortcuts`, so recency must
# still fire InputNeeded. A naive "composer footer present → not asking"
# veto would wrongly miss this; the position-based gate gets it right.
write("permission_below_stale_composer.bin", [
    cup(8, 1), sgr(2), "● Running the build…".encode("utf-8"), RESET, b"\r\n",
    cup(9, 3), sgr(2, 90), b"? for shortcuts", RESET, b"\r\n",   # stale composer footer
    cup(11, 1), CLEAR_LINE,
    sgr(1), b"Allow Bash this command?", RESET, b"\r\n",
    cup(12, 1), CLEAR_LINE,
    "  ❯ ".encode("utf-8"),
    cup(24, 1), CLEAR_LINE,
    sgr(2), "(3s · 41 tokens)".encode("utf-8"), RESET,
    cup(12, 5), sgr(7), b"1. Yes", RESET, b"\r\n",
    cup(13, 5), b"2. Yes, and don't ask again\r\n",
    cup(14, 5), b"3. No, and tell Claude what to do differently\r\n",
    cup(16, 1), sgr(2), b"Esc to cancel", RESET,
])

# ── FALSE NEGATIVE (#2): bash-approval dialog with the LONG footer ────────
# A real Bash-command approval whose footer is the full
# `Esc to cancel · Tab to amend · ctrl+e to explain` form — NOT the short
# `Esc to cancel` the detector once assumed. The footer carries
# `Tab to amend`, which `idle_box_pos` matched, so the recency gate read
# the live `❯ 1. Yes` chooser above it as stale scrollback and returned
# Idle — the `?` never showed. The `Esc to cancel` footer fragments
# spaceless across the status bar exactly like the working/idle footers
# do. A bash approval always asks a consent question, so the STRONG
# standalone-phrase tier (gated on the reliable `? for shortcuts` / bypass
# anchor, absent here) catches it. Must read InputNeeded, never ready.
write("permission_bash_amend_footer.bin", [
    cup(8, 1), CLEAR_LINE,
    sgr(1, 34), b"Bash command", RESET, b"\r\n",
    cup(9, 3), b"for c in git-ops gh-provider; do echo $c; done\r\n",
    cup(11, 1), CLEAR_LINE,
    sgr(1), b"Do you want to proceed?", RESET, b"\r\n",
    cup(12, 3), "❯ ".encode("utf-8"),                       # arrow lands first…
    cup(24, 1), CLEAR_LINE,                                  # …ticker repaints a far row
    sgr(2), "(2s · 18 tokens)".encode("utf-8"), RESET,
    cup(12, 5), sgr(7), b"1. Yes", RESET, b"\r\n",           # then the option text
    cup(13, 5), b"2. No\r\n",
    # the long footer, painted spaceless into the status bar
    cup(15, 1), sgr(2), b"Esc", cup(15, 5), b"to", cup(15, 8), b"cancel",
    cup(15, 16), "·".encode("utf-8"), cup(15, 18), b"Tab", cup(15, 22), b"to", cup(15, 25), b"amend",
    cup(15, 33), "·".encode("utf-8"), cup(15, 35), b"ctrl+e", cup(15, 42), b"to", cup(15, 45), b"explain",
    RESET,
])
