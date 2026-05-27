# TODO

Captured ideas that are real but not the current sprint. Not a wishlist —
each one is a decision deferred with the *why* logged so picking it up
later doesn't require re-deriving the design.

## RowList<T> abstraction (sidebar / activity / picker)

**Problem.** Four components open-code "cursor + multi-select + skip
non-selectable rows":

- `ActivityFeed` (`cursor`, `selected`, `expanded`)
- `Sidebar` (`cursor`, `collapsed_repos`, header-skip in `move_cursor_by`)
- `Choice` modal (`cursor`, `selected` for multi-pick)
- `TerminalStack` (active tab index)

The right-pane click handler narrates the friction directly:

```
// Single click on a card = "select THIS row" (radio).
// Replaces the previous toggle-into-set behaviour, which meant clicking
// row B while row A was selected deselected A (toggle off… no wait, A
// stayed; the bug was clicking the SAME row twice in a row deselected
// it visually without re-selecting on next click — and accumulating
// selection across clicks confused users who expected mailer-style
// "click moves the highlight").
```

That comment exists because the right place for the rule doesn't exist
yet.

**Shape.**

```rust
pub struct RowList<T> {
    items: Vec<T>,
    cursor: usize,
    selected: HashSet<usize>,
    selectable: Box<dyn Fn(&T) -> bool>,
}

impl<T> RowList<T> {
    pub fn move_cursor(&mut self, delta: isize);
    pub fn click(&mut self, idx: usize, mods: ClickMods);
    pub fn focused(&self) -> Option<&T>;
    pub fn select_only(&mut self, idx: usize);
    pub fn toggle_select(&mut self, idx: usize);
    pub fn clear_selection(&mut self);
}

pub enum ClickMods {
    Plain,  // radio: replaces selection with {idx}
    Shift,  // range: extends selection from anchor to idx
    Ctrl,   // toggle: flips idx in/out of selection set
}
```

Then each component holds a `RowList<RowType>` instead of three loose
fields. The "click semantics" rule lives in one file with one test
suite covering Plain / Shift / Ctrl.

**Not doing it now.** Cross-pane refactor under "everything is broken"
is the worst time. Pick up once sync is healthy and the next-tick log
cadence is consistent.

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
action back to Script Editor, which is awful UX). On macOS pilot
now only fires desktop notifications when `terminal-notifier` is on
PATH; otherwise it silently no-ops with a one-time log line.

**Still missing.** A real solution: bundle pilot as a `.app` with
its own Info.plist + LSUIElement + bundle id so we can call
`UNUserNotificationCenter` directly via objc bindings. Then:

- The notification carries pilot's icon (not terminal-notifier's).
- Click can register a custom URL scheme (`pilot://workspace/<key>`)
  to focus the running daemon's TUI on the right row.
- Users don't have to `brew install terminal-notifier` first.

Until that bundle exists, recommending `terminal-notifier` is the
escape hatch (documented in `notify_user` in
`crates/tui-core/src/platform.rs`).
