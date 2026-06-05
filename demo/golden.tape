# Deterministic golden snapshot for CI's stale-demo check.
#
# Why a SEPARATE tape from lazybox.tape: the pretty demo spawns a shell, whose
# prompt carries volatile content (a random tempdir suffix, the wall clock, the
# OS username) — so its text capture changes on every render and can't be a
# golden. This tape never spawns a shell. It boots `lazybox --test` and opens the
# `?` help overlay, which is static lazybox-owned chrome (the keybinding table).
# Lazybox's own rows are byte-stable across renders, so the demo workflow's
# `git diff` only fails when the UI or a keybinding actually changes — i.e.
# when the demo genuinely went stale. (The one exception is row 0, which holds
# VHS's shell prompt, not lazybox UI, and races between '>' and blank on the
# Linux runner; the workflow normalizes that cell before diffing.)
#
# Regenerate alongside lazybox.tape whenever the UI changes:
#   cargo build -p lazybox-tui
#   PATH="$PWD/target/debug:$PATH" vhs demo/golden.tape

Output "demo/golden.ascii"

Set FontSize 16
Set Width 1200
Set Height 720
Set Theme "Catppuccin Mocha"

Hide
Type "lazybox --test"
Enter
# Boot settle: the daemon initializes and the first frame renders at the
# full terminal size before we touch anything. On a loaded Linux CI
# runner this can take longer than it does locally, so give it margin —
# pressing `?` before the UI has settled at full size is what produces a
# short overlay frame.
Sleep 4s
Show

# Open the keybinding help overlay — fully deterministic, no live shell.
# The keybinding table is static (built from `ActionDef::all()`, no live
# data), so its content never changes between renders. The historical
# flake was purely a capture race: VHS's end-of-tape frame selection
# landed on a not-yet-settled frame on the Linux runner, truncating the
# table mid-row with the remaining rows blanked.
#
# A fixed Sleep only moves WHEN the frame is grabbed; it can never
# guarantee the grab lands on a fully-painted overlay, so on a loaded
# runner the race kept resurfacing (the sleep was bumped 1.5s → 3s → 6s
# and still flaked). Instead, block on `Wait+Screen` until the overlay's
# LAST row is actually on screen: `snippets` is the final cell of the
# bottom-right keybinding (`]]<key>  snippets`), so once it appears the
# whole table above it has been painted and the end-of-tape capture is
# guaranteed to grab the settled, complete frame. The generous timeout
# absorbs slow boots without ever capturing a partial frame. This makes
# the render deterministic by construction rather than probabilistically,
# and does not touch the committed golden's content.
Type "?"
Wait+Screen@30s /snippets/
Sleep 1s
