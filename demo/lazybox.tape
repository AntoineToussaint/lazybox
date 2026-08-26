# lazybox demo — the reactive multi-repo agent inbox, alive.
#
# SOURCE OF TRUTH for the homepage hero + README GIF. It is code, not a
# binary blob: edit here, regenerate, commit the outputs.
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
# run. High-DPI (FontSize 32 @ 2560×1440) so text is crisp on retina and
# downscales cleanly on the site — the old 1200×720 renders looked low-res.

Output "demo/lazybox.gif"
Output "demo/lazybox.mp4"

Set Shell "bash"
Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30
Set LoopOffset 30%

# Boot the deterministic fleet scenario; hide the launch command so the loop
# opens on the payoff — the inbox already coming alive.
Hide
Type "lazybox --demo"
Enter
# Block on the seeded selection painting at full size (robust to runner speed).
Wait+Screen@45s /rate limiter/
Show

# The reactive inbox: repos auto-grouped into Spaces, agents lighting up with
# their states (working / asking / done / rate-limited), terminals streaming.
Sleep 5s

# The payoff: a PR's CI flips red→green on its own and a notice lands — the
# event arrived, no manual refresh. Static frame for the reduced-motion poster.
Sleep 3s
Screenshot "demo/lazybox.png"
Sleep 3s
