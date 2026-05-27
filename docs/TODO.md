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

## `w` (work) doesn't inject "fix CI" prompt into a running claude

**Symptom.** User pressed `w` on a CI-failing PR with an existing
claude session already running. Expectation: "fix CI failures"
prompt streams into the running claude prompt input. Actual:
nothing happens, OR a new agent is spawned instead of injecting.

- Likely location: `crates/tui-core/src/intent.rs::resolve_work` +
  the `Action::Work` dispatch.
- The work resolver returns `Intent::SpawnAgent { prompt }` —
  which the dispatcher uses to spawn a *new* claude. When a
  claude is already running on this workspace, we should inject
  the prompt into the existing session's PTY instead.
- The "inject into existing" path exists for Slack replies (see
  `crates/server/src/slack.rs::handle_inbound`'s
  `encode_for_pty` + `backend.write`). The local `w` flow should
  use it.

## Auto-mark-read on activity hover not firing

**Symptom.** Cursor sits on an unread activity for >1s, doesn't
flip to read.

- Likely location: `crates/tui/src/components/right_pane/mod.rs`
  `rearm_mark_timer` (line ~285), `tick` (line ~339).
- `arm()` resets `armed_at` to `Instant::now` on every call. Every
  consumed keypress calls `rearm_mark_timer(true)` (line ~1262).
  If anything else continuously fires rearm (e.g., set_workspace
  on every poll cycle's WorkspaceUpserted), the timer never
  reaches the 1s threshold.
- Verify: log `mark_timer.armed_at` + `auto_mark_delay` at every
  tick. If `armed_at` jumps around without crossing the delay
  threshold, that's the cause.

## Issue ↔ PR merge fails when PR has `closes_issues` but issue stays as standalone row

**Symptom** (screenshot 2026-05-27). PR #222 titled
"Add dynamic credential source for per-request secrets (closes #31)"
shows in the inbox AND issue #31 ("Add support for dynamic
credentials in headers") shows as a SEPARATE row under
`@ assignee`. The merge-into-PR collapse never fired.

- Look at:
  - `crates/server/src/polling/mod.rs::merge_closing_issue_workspaces`
    (line ~1391). It only runs when `workspace.pr.closes_issues`
    is non-empty.
  - `crates/gh-provider/src/graphql.rs::pr_to_task` — does it
    populate `closes_issues` from `closingIssuesReferences`?
  - The PR title contains `(closes #31)` but the canonical
    `closingIssuesReferences` GraphQL field is what populates
    `closes_issues`. If the GH side has the link, `closes_issues`
    should have it.
- Verify: `grep "closingIssuesReferences\|closes_issues" /tmp/pilot.log`.
  Look for "routing issue upsert into PR workspace
  (closingIssuesReferences)" — that's the merge log line.
- Edge case: when the ISSUE polls in AFTER the PR, `upsert` should
  detect the existing PR workspace claiming it (line 1157-1164).
  When the PR polls in AFTER the issue, the
  merge_closing_issue_workspaces path handles it.
- One specific concern: the body-text fallback parser at
  `pilot_core::Workspace::from_task` mentioned in comments may
  only check the body, not the title. A PR with `closes #31` only
  in the title (no body link, no GitHub-side
  `closingIssuesReferences`) would never collapse.

## Archive (`Shift-X`) → workspace reappears on next sync

**Symptom.** User archived a workspace (`Shift-X` confirmed),
then the next poll's `WorkspaceUpserted` for the same task
re-created the row.

- Likely location: `polling::delete_workspace` removes the row
  but doesn't prevent the next poll's `upsert` from re-creating
  it. The `archived` state needs to live somewhere durable so
  the upsert path checks "did the user already archive this?"
  and skips OR routes to the Snoozed mailbox.
- Suggested shape: a `archived: HashSet<WorkspaceKey>` on
  `TickState` (or persisted in the store under a kv key) that
  the upsert path consults before writing. The user can re-add
  the workspace by un-archiving (no UI for that yet — add
  Settings → Restore Archive).

## Spawning claude from an Issue doesn't create the worktree

**Symptom.** User pressed `c` on an issue row, claude spawned in
the same dir as pilot's CWD (or failed silently). The expected
behavior is "create a worktree on a fresh branch named after the
issue, spawn claude in that worktree."

- Likely location: `spawn_handler::handle_spawn` should auto-
  create a session + worktree when the workspace has no
  sessions yet. For PR workspaces this exists via
  `worktree_path_for_session`. For issue workspaces with no
  upstream branch, the path lookup might return `None` and the
  spawn falls through to a no-worktree mode.

## Mouse copy only works on multi-line, then copies one line

**Symptom.** Single-line drag-to-select doesn't copy at all;
multi-line copies but only one line of the selection ends up in
the clipboard.

- FIXED (this commit): rewrote `extract_text` from a rectangular
  selection model to row-major flowing-text (first row from
  anchor → end; middle rows whole; last row from start → focus).
- The single-line-doesn't-copy case might still hit an edge with
  drags shorter than one cell width. Test interactively after
  rebuild.

## Pressing `w` on an issue doesn't inject "implement/solve this issue" into running agent

**Symptom.** `w` on an issue row with a running claude doesn't
ingest the issue's "implement / solve this issue" prompt into the
existing claude session.

- Related to the `w → InjectPrompt` rewrite fix landed in
  e6c1bf2 — that fix covers the catalog dispatch path generally,
  but issue-implement prompt might be a different branch in
  `intent::resolve_work` that doesn't hit `Intent::SpawnAgent`
  the same way.
- Verify: `grep "w on" /tmp/pilot.log` after pressing `w` on an
  issue row + observe whether the resolved Intent is
  `SpawnAgent { prompt: Some("…implement…") }` or something
  else. Check `crates/tui-core/src/intent.rs::resolve_work`
  branches for issue handling.

## `m` (mark) is workspace-only — no "mark this one activity"

**Symptom.** User clicks an activity row to select it, presses
`m` — expects "mark THIS activity as read." Actually marks ALL
of the workspace's activities as read.

- The action catalog has one `m` mapping: `MarkAllRead`. Should
  be context-sensitive:
  - cursor on workspace, no activity selected → mark all.
  - one activity selected → mark only that one.
  - multiple selected → mark the selected set.
- Likely path: in the right pane's `m` handler, check
  `self.feed.selected()` first; if non-empty, fire one
  `Command::MarkActivityRead` per selected index instead of
  `Command::MarkRead`.
