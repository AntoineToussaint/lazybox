# Website demo — GitHub-driven work (web/public/demo/05-autowork.*)
#
# SOURCE OF TRUTH for the homepage "github auto-work" clip. Code, not a blob:
# edit here, regenerate, commit the outputs. See demo/README.md for the full
# `--demo` regen flow (render, poster, captions).
#
# Drives the deterministic `lazybox --demo` fleet from the inbox: an agent is
# blocked on a decision, `!` jumps straight to it, and focus mode surfaces the
# question — while the reactive CI red→green flip lands as a footer notice.
#
#   cargo build -p lazybox-tui-boot        # produces target/debug/lazybox
#   PATH="$PWD/target/debug:$PATH" vhs demo/05-autowork.tape

Output "web/public/demo/05-autowork.mp4"

Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30

Hide
Type "lazybox --demo"
Enter
Wait+Screen@45s /rate limiter/
Show

# The reactive inbox: CI, reviews and asking agents across repos.
Sleep 3500ms

# Jump straight to the agent that needs a decision — the sidebar cursor lands
# on it and its question opens in the activity pane.
Type "!"
Wait+Screen@15s /needs input/
Sleep 1400ms
Screenshot "web/public/demo/05-autowork.png"
Sleep 2600ms
