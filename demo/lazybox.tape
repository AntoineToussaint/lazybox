# lazybox demo — the reactive PR inbox, end to end in one workflow.
#
# This tape is the SOURCE OF TRUTH for the README GIF. It is code, not a
# binary blob: edit it here, regenerate, commit the outputs.
#
# Regenerate locally:
#   cargo build -p lazybox-tui-boot       # produces target/debug/lazybox
#   PATH="$PWD/target/debug:$PATH" vhs demo/lazybox.tape
#
# Requires `vhs` (brew install vhs) and a `lazybox` binary on PATH.
# Rendering is intentionally local/manual; CI uses deterministic TUI snapshots.
#
# Deterministic render: FontSize / Width / Height / Theme are pinned so the
# output only changes when the tape or the UI changes — never from host drift.

Output "demo/lazybox.gif"
Output "demo/lazybox.mp4"

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 720
Set Theme "Catppuccin Mocha"
Set Padding 20
Set TypingSpeed 70ms
Set LoopOffset 25%

# Boot the throwaway, no-GitHub sandbox: a seeded workspace, no credentials,
# no setup wizard. Hide the launch command so the loop opens on the payoff.
Hide
Type "lazybox --test"
Enter
Sleep 3.5s
Show

# The inbox: a workspace is selected, its activity in the right pane.
Sleep 2s

# Open the workspace.
Enter
Sleep 1.5s

# Spawn a shell in the workspace's git worktree.
Type "s"
Sleep 2.5s

# It's a real terminal in a real worktree — run something.
Type "git status"
Enter
Sleep 2.5s
Type "git log --oneline"
Enter
Sleep 3s

# Static frame for the README's prefers-reduced-motion fallback.
Screenshot "demo/lazybox.png"

# Back out to the inbox.
Type "]]"
Sleep 2.5s
