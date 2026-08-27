# Website demo — Snippets (web/public/demo/02-snippets.*)
#
# SOURCE OF TRUTH for the homepage "Snippets" clip. Code, not a blob: edit
# here, regenerate, commit the outputs. See demo/README.md for the full
# `--demo` regen flow (render, poster, captions).
#
# Drives the deterministic `lazybox --demo` fleet, preselected onto the live
# Claude agent working obin-ai/lazybox#1332, enters focus mode, and opens the
# `]]s` snippet picker — the reusable-prompt library with a live body preview.
#
#   cargo build -p lazybox-tui-boot        # produces target/debug/lazybox
#   PATH="$PWD/target/debug:$PATH" vhs demo/02-snippets.tape

Output "web/public/demo/02-snippets.mp4"

Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30

# Boot preselected on the working Claude agent, then drop into focus mode
# while still hidden so the clip opens cleanly on the near-fullscreen terminal.
Hide
Type "lazybox --demo --workspace github-obin-ai-lazybox-1332"
Enter
Wait+Screen@45s /rate limiter/
Sleep 2s
Type "."
Wait+Screen@15s /typing to/
Show

# The agent is mid-work; open the snippet picker over it.
Sleep 1200ms
Type "]]s"
Wait+Screen@15s /Snippets/
Sleep 1600ms

# Walk to the flagship "deepreview" snippet — its body previews on the right.
Down
Sleep 500ms
Down
Sleep 1800ms
Screenshot "web/public/demo/02-snippets.png"

# Fire it into the running agent.
Enter
Sleep 1600ms
