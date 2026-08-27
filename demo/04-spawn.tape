# Website demo — Worktree + agent (web/public/demo/04-spawn.*)
#
# SOURCE OF TRUTH for the homepage "worktree + agent" clip. Code, not a blob:
# edit here, regenerate, commit the outputs. See demo/README.md for the full
# `--demo` regen flow (render, poster, captions).
#
# Drives the deterministic `lazybox --demo` fleet, preselected onto the live
# Codex agent working obin-ai/web#88 in its own worktree branch (feature/88),
# and records it streaming real work — editing, building — in the right pane
# beside the reactive inbox.
#
#   cargo build -p lazybox-tui-boot        # produces target/debug/lazybox
#   PATH="$PWD/target/debug:$PATH" vhs demo/04-spawn.tape

Output "web/public/demo/04-spawn.mp4"

Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30

Hide
Type "lazybox --demo --workspace github-obin-ai-web-88"
Enter
Wait+Screen@45s /rate limiter/
Show

# The task's dedicated worktree + agent, live: sidebar for fleet context, the
# branch in the header, the agent streaming its work on the right.
Wait+Screen@30s /built in/
Sleep 1800ms
Screenshot "web/public/demo/04-spawn.png"
Sleep 600ms
