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
Sleep 3s
Show

# Open the keybinding help overlay — fully deterministic, no live shell.
# The overlay's keybinding table paints progressively; capturing too
# early grabs a half-drawn table (rows after the first cut to blank),
# which fails the byte-exact golden diff non-deterministically on the
# Linux runner. Sleep long enough that the full table has settled
# before the end-of-tape capture.
Type "?"
Sleep 3s
