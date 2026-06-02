# Deterministic golden snapshot for CI's stale-demo check.
#
# Why a SEPARATE tape from pilot.tape: the pretty demo spawns a shell, whose
# prompt carries volatile content (a random tempdir suffix, the wall clock, the
# OS username) — so its text capture changes on every render and can't be a
# golden. This tape never spawns a shell. It boots `pilot --test` and opens the
# `?` help overlay, which is static pilot-owned chrome (the keybinding table).
# Pilot's own rows are byte-stable across renders, so the demo workflow's
# `git diff` only fails when the UI or a keybinding actually changes — i.e.
# when the demo genuinely went stale. (The one exception is row 0, which holds
# VHS's shell prompt, not pilot UI, and races between '>' and blank on the
# Linux runner; the workflow normalizes that cell before diffing.)
#
# Regenerate alongside pilot.tape whenever the UI changes:
#   cargo build -p pilot-tui
#   PATH="$PWD/target/debug:$PATH" vhs demo/golden.tape

Output "demo/golden.ascii"

Set FontSize 16
Set Width 1200
Set Height 720
Set Theme "Catppuccin Mocha"

Hide
Type "pilot --test"
Enter
Sleep 3s
Show

# Open the keybinding help overlay — fully deterministic, no live shell.
Type "?"
Sleep 1.5s
