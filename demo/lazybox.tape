# lazybox demo — the reactive multi-repo agent inbox, alive.
#
# SOURCE OF TRUTH for the homepage hero (web/public/demo/01-inbox.*) + the
# README GIF. It is code, not a binary blob: edit here, regenerate, commit the
# outputs. See demo/README.md for the full `--demo` regen flow.
#
# Regenerate locally:
#   cargo build -p lazybox-tui-boot        # produces target/debug/lazybox
#   PATH="$PWD/target/debug:$PATH" vhs demo/lazybox.tape
#
# Requires `vhs` (brew install vhs) + a `lazybox` binary on PATH.
#
# Deterministic: `lazybox --demo` boots a throwaway multi-repo inbox (no
# GitHub, no PTY) and replays the built-in "fleet" scenario — several agents
# working across repos, one asking, one done, one rate-limited, live terminals
# streaming, and a reactive CI red→green flip. It renders identically every
# run. Preselected onto the Claude agent working obin-ai/lazybox#1332 so the
# right pane streams real work while the fleet lights up on the left, and the
# clip runs to the moment a PR's CI flips green on its own. High-DPI
# (FontSize 32 @ 2560×1440) so text is crisp on retina and downscales cleanly.

Output "demo/lazybox.gif"
Output "demo/lazybox.mp4"

Set Shell "bash"
Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30
Set LoopOffset 30%

# Boot the deterministic fleet scenario, preselected on the working Claude
# agent; hide the launch command so the loop opens on the payoff — the inbox
# already coming alive.
Hide
Type "lazybox --demo --workspace github-obin-ai-lazybox-1332"
Enter
# Block on the seeded selection painting at full size (robust to runner speed).
Wait+Screen@45s /rate limiter/
Show

# The reactive inbox: repos auto-grouped into Spaces, agents lighting up with
# their states (working / asking / done / rate-limited), and the focused agent
# streaming its work — reading, editing, building — on the right.
Wait+Screen@30s /Finished in/

# The payoff: with the agent done, the footer-fade PR's CI flips red→green on
# its own and the sidebar updates — the event arrived, no manual refresh.
# Static frame for the reduced-motion poster.
Sleep 2500ms
Screenshot "demo/lazybox.png"
Sleep 2s
