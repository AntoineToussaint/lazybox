# TODO

Captured ideas that are real but not the current sprint. Not a wishlist —
each one is a decision deferred with the *why* logged so picking it up
later doesn't require re-deriving the design.

## Issue ↔ PR merge: sidebar relationship chip

**Status.** Shift+J ("join into PR") now triggers a manual
collapse from the focused issue row into the PR that closes it,
bypassing the dedupe state so a previously-dismissed prompt is
actionable. Daemon side is `polling::handle_collapse_into_pr`;
TUI dispatch surfaces a "no PR closes this" footer notice when
the relationship isn't known locally.

**Still missing.** Sidebar relationship chip: when both rows are
visible, the issue row should show `→ PR #N` so the user knows
the relationship exists before they press Shift+J. Pure render
surface — daemon already broadcasts the data the chip needs (PR's
`closes_issues` includes the issue's task id).

## macOS desktop notifications: ship a .app bundle

**Status.** The Script-Editor-on-click surprise is fixed: the
`osascript` fallback was dropped (newer macOS attributes the click
action back to Script Editor, which is awful UX). On macOS lazybox
now only fires desktop notifications when `terminal-notifier` is on
PATH; otherwise it silently no-ops with a one-time log line.

**Still missing.** A real solution: bundle lazybox as a `.app` with
its own Info.plist + LSUIElement + bundle id so we can call
`UNUserNotificationCenter` directly via objc bindings. Then:

- The notification carries lazybox's icon (not terminal-notifier's).
- Click can register a custom URL scheme (`lazybox://workspace/<key>`)
  to focus the running daemon's TUI on the right row.
- Users don't have to `brew install terminal-notifier` first.

Until that bundle exists, recommending `terminal-notifier` is the
escape hatch (documented in `notify_user` in
`crates/tui-core/src/platform.rs`).
