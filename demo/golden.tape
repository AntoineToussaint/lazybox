# Deterministic golden snapshot for CI's stale-demo check.
#
# Why a SEPARATE tape from lazybox.tape: the pretty demo spawns a shell, whose
# prompt carries volatile content (a random tempdir suffix, the wall clock, the
# OS username) — so its text capture changes on every render and can't be a
# golden. This tape never spawns a shell. It boots `lazybox --test` and opens the
# `?` help overlay, whose keybinding table is static lazybox-owned chrome (built
# from `ActionDef::all()`, no live data), so its content only changes when the
# UI or a keybinding actually changes.
#
# What the demo workflow diffs: NOT this whole `.ascii` file. VHS's `.ascii`
# output is the terminal's scrollback at end-of-tape — a vertical concatenation
# of however many full-screen repaints happened, which varies with render
# timing and across machines (Linux CI vs. a dev's macOS). The byte-stable part
# is the keybinding table itself, so the workflow extracts just that table from
# the render and diffs it against the committed `golden.ascii` (which holds only
# the table). See `.github/workflows/demo.yml`.
#
# Regenerate alongside lazybox.tape whenever the UI changes:
#   cargo build -p lazybox-tui
#   PATH="$PWD/target/debug:$PATH" vhs demo/golden.tape
#   # then extract the table (same awk the workflow uses) into golden.ascii:
#   awk '/Tab .* cycle panes/{c=1;b=""} c{b=b $0 ORS} c&&/exit to sidebar/{last=b;c=0} END{printf "%s",last}' \
#     demo/golden.ascii > demo/golden.ascii.tmp && mv demo/golden.ascii.tmp demo/golden.ascii

Output "demo/golden.ascii"

Set FontSize 16
Set Width 1200
# Tall enough that the whole `?` help overlay fits on one frame — the
# bottom row (`]]  exit to sidebar`) carries the `Wait+Screen` sentinel
# below, so it must be on screen for the render to settle. The keybinding table
# keeps growing (issue #224 added the `w c` / `w x` / `w u` scoped-work group;
# #315 added the ask-lazybox search, messages log, theme, and change-default
# rows), so each bump reveals the clipped tail: 720 → 1000 → 1600. Height only
# reveals more of the overlay and never changes the table's byte content
# (that's width/content-driven), so this is generously oversized on purpose to
# absorb the next few rows before it clips the anchor again.
Set Height 1600
Set Theme "Catppuccin Mocha"

Hide
Type "lazybox --test"
Enter
# Boot settle: `--test` provisions a throwaway git repo + spins up the
# daemon before the first full-size frame renders. On a loaded Linux CI
# runner that can outlast any fixed `Sleep` — and pressing `?` before the
# UI has settled at full size paints a clipped overlay whose bottom row
# never repaints, so the golden `Wait+Screen` below times out (blank or
# truncated grab). Block on the seeded workspace row instead: once
# `Test workspace` is on screen the sidebar has painted at full size, so
# boot is provably done regardless of runner speed.
Wait+Screen@45s /Test workspace/
Sleep 1s
Show

# `?` now opens the "Ask Lazybox" assistant (the search-first help surface,
# issue #302), NOT the keybinding table directly. Pressing `?` again from its
# empty prompt swaps to the compact all-shortcuts index — the table this
# golden snapshots. So the tape presses `?` twice: block on the assistant's
# `Ask Lazybox` title before the second press (a raw second `?` racing ahead
# of the assistant's mount would just re-open the assistant, never the index).
Type "?"
Wait+Screen@30s /Ask Lazybox/
Type "?"
# Block on `Wait+Screen` until the index's LAST row is actually on screen:
# `exit to sidebar` is the bottom row's left cell (`]]  exit to sidebar`), so
# once it appears the whole table above it has been painted. We key on
# `exit to sidebar` rather than `snippets` because `snippets` now labels two
# rows (the global `]` browser and the terminal `]]<key>` leader) — the
# workflow's table extraction anchors on the same unique `exit to sidebar`
# marker. This guarantees the final captured frame contains a complete table
# for the workflow to extract — a fixed Sleep only moves WHEN the frame is
# grabbed and can't guarantee it lands on a fully-painted overlay (the
# historical flake: the grab landed mid-paint and the table was truncated
# mid-row with the rest blanked; the sleep was bumped 1.5s → 3s → 6s and still
# raced). The generous timeout absorbs slow boots; the trailing Sleep lets the
# frame settle before capture.
Wait+Screen@30s /exit to sidebar/
Sleep 1s
