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

**Status.** On macOS lazybox prefers `terminal-notifier` when it's
on PATH (real icon, verifiable exit status, sane click target).
When it's missing, `osascript -e 'display notification …'` is still
the last-resort fallback — it ships with every macOS install, so a
stock Mac gets a banner rather than silence. The tradeoff stands:
newer macOS attributes the osascript banner's click action to
Script Editor, and `display notification` exits 0 even when the
banner is suppressed, so lazybox logs a one-time hint pointing at
`terminal-notifier` (see `notify_user` in
`crates/tui-core/src/platform.rs`).

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
