# Website demo — GitHub controls (web/public/demo/03-policies.*)
#
# SOURCE OF TRUTH for the homepage "GitHub controls" clip. Code, not a blob:
# edit here, regenerate, commit the outputs. See demo/README.md for the full
# `--demo` regen flow (render, poster, captions).
#
# Drives the deterministic `lazybox --demo` fleet, preselected onto a mergeable
# PR (acme/api#210), opens the `g` GitHub leader menu, then `g p` for the
# unified automation-policies surface.
#
#   cargo build -p lazybox-tui-boot        # produces target/debug/lazybox
#   PATH="$PWD/target/debug:$PATH" vhs demo/03-policies.tape

Output "web/public/demo/03-policies.mp4"

Set FontSize 32
Set Width 2560
Set Height 1440
Set Theme "Catppuccin Mocha"
Set Padding 30

Hide
Type "lazybox --demo --workspace github-acme-api-210"
Enter
Wait+Screen@45s /rate limiter/
Show

# Open the GitHub leader menu on the focused PR.
Sleep 1400ms
Type "g"
Wait+Screen@15s /auto-merge on green/
Sleep 1800ms
Screenshot "web/public/demo/03-policies.png"

# Drop into the automation-policies surface.
Type "p"
Wait+Screen@15s /Automation policies/
Sleep 2200ms
Escape
Sleep 400ms
Escape
Sleep 600ms
