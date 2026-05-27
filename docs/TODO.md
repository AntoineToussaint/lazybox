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

## Projects: new-project row not selectable + new-workspace invisible

**Two stacked bugs the user hit during a "create project, then create
workspace inside it" flow.**

### Bug 1: New project row isn't selectable

After `N` (new project) creates a local project, its `RepoHeader`
row in the sidebar is shown but the cursor can't land on it for
the purposes of `n` (new workspace). The catalog gate is
`focused_project_key()` returning `None` because the cursor sits
on a row whose `selected_session_key` is `None` (the header).

- Likely location: `crates/tui/src/components/sidebar/mod.rs`
  `focused_project_key()` — currently handles `VisibleRow::Workspace`,
  `Session`, `RepoHeader`, `RoleHeader`. RepoHeader maps to project
  by name lookup. For a brand-new local project, the lookup might
  miss because the project was just upserted but the sidebar's
  `projects` map isn't refreshed before the user navigates.
- Verify: log `focused_project_key()` result when cursor sits on a
  fresh-created project header. If `None`, the bug is the
  projects-map sync; if `Some`, the bug is somewhere downstream in
  `Action::NewWorkspace` dispatch.

### Bug 2: New workspace doesn't render after creation

`Command::CreateWorkspace { name, project_key }` succeeds on the
daemon side (presumably — needs verification) but the new
workspace doesn't appear in the sidebar. Three likely causes:

1. Daemon doesn't broadcast `WorkspaceUpserted` for sandbox/pre-PR
   workspaces — only PR/issue-attached ones go through the polling
   upsert path.
2. The sidebar's `recompute_visible` filter (mailbox membership)
   drops the new workspace because it has no primary task.
3. The store write succeeds but the broadcast event has no
   subscribers (UI was disconnected at the moment, store-only).

Trace: `grep -E "CreateWorkspace|WorkspaceUpserted" /tmp/pilot.log`
after pressing `n` on a new project. The first thing to confirm is
whether the daemon even saw the command.

**Fix shape (probably)**:
- After `CreateWorkspace`, daemon explicitly upserts the empty
  workspace (already does this presumably) AND broadcasts the
  event, AND ensures `mailbox_membership` accepts empty workspaces
  in the Inbox.

## Issue ↔ PR merge: manual trigger key + label

**Status.** The auto-detect side now works: `closes_issues` is
populated from BOTH `closingIssuesReferences` and the PR title
fallback, `merge_closing_issue_workspaces` is invoked from the
upsert path, Esc on the merge modal is now a silent dismissal
(was pinning rejected_merge for the whole session), and
`prompted_merge` re-fires after 5 minutes so dismissals self-heal.

**Still missing.** The user should be able to *manually* fold an
issue into its PR without waiting for the next poll's re-prompt.
Two pieces:

- Key binding (e.g. `Shift+M`) on a workspace row that triggers
  `Command::TriggerMerge { issue_workspace_key, pr_workspace_key }`.
  The daemon side runs the same `absorb_issue_workspace` path that
  the modal Y reaches today, bypassing `rejected_merge` /
  `prompted_merge`. The binding is only available when the
  resolver can identify a target PR (issue has a PR that closes
  it, or PR has the issue in `closes_issues`).
- Sidebar label: when the issue and its PR are both in the inbox,
  the issue row should show `→ PR #N` as a hint chip so the user
  knows the relationship exists.

Both are pure UX surface — the underlying daemon path already
exists in `handle_confirm_merge`.

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
