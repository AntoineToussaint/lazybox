# Inbox & sync

The reactive inbox is lazybox's core model: instead of polling GitHub yourself,
provider events flow to you and surface as workspace rows with read/unread
state. This page covers the inbox, the polling loop that feeds it, and the
controls that shape what you see.

See [Providers](providers.md) for where the rows come from and
[TUI & UX](tui-and-ux.md) for the panes that render them.

---

## Reactive inbox

**Status:** stable
**Crate(s):** `tui` (sidebar), `server` (event bus + polling), `core` (`Task`, `Workspace`)
**Config / flags:** `attention.*` toggles (unread, ci_failing, review_pending, agent_asking, mentioned)
**Key bindings:** `j/k` navigate, `Enter` open, `Space` fold/unfold repo group

### What it does
Every PR, issue, or ticket you have a role on becomes a **workspace** row in
the sidebar, grouped by repo. New comments, CI failures, and review requests
update the row in place and mark it unread, so the inbox is a live picture of
what needs your attention rather than a list you refresh by hand.

### How to use it
Launch `lazybox`. The sidebar lists workspaces grouped under their repo. `j/k`
(or arrows) move the cursor, `Enter` opens a workspace into the Activity +
Terminal panes, `Space` folds/unfolds a repo group. Rows carry status chips
(CI, review, unread count, agent state).

### How it works (brief)
Providers poll and emit `Event::WorkspaceUpserted` onto the daemon's broadcast
bus (event types in `crates/ipc`). The sidebar subscribes and rebuilds
its row model. A `Workspace` (`crates/core/src/workspace.rs`) bundles at most
one PR, its linked issues, merged activity, and zero-or-more terminal
`Session`s. `attention.*` config decides which signals count as
"needs attention."

### Test checklist
- [ ] A new comment on a PR you author bumps the row to unread within one poll interval.
- [ ] CI transitioning to failing surfaces a failing chip on the row.
- [ ] Folding a repo group with `Space` hides its children; unfolding restores them.
- [ ] Opening a workspace with `Enter` populates the Activity pane.
- [ ] Toggling `attention.ci_failing: false` stops CI failures from flagging the row.

### Known sharp edges
- Row model is rebuilt from the full task set each poll; very large inboxes haven't been stress-tested.
- A workspace with no branch (e.g. an unlinked issue) opens to activity only — no worktree.

---

## Provider polling / sync loop

**Status:** stable
**Crate(s):** `server` (`polling/mod.rs`, `polling/scheduler.rs`)
**Config / flags:** `providers.github.poll_interval` (default 60s; fallback 90s)
**Key bindings:** —

### What it does
A background loop polls each enabled provider and pushes results onto the event
bus. It schedules **round-robin per repo** with a stalest-first priority so a
large set of repos stays fresh without blowing the GitHub rate budget.

### How to use it
Nothing to do — it runs as soon as the daemon starts. Tune cadence with
`providers.github.poll_interval`. Watch what it's doing via the
[sync-status window](#sync-status-window) (`Shift-D`).

### How it works (brief)
`polling::spawn()` (`crates/server/src/polling/mod.rs`) drives the loop on a
chunked-sleep timer (5s chunks) so a manual refresh can interrupt it. The
scheduler (`polling/scheduler.rs`) keeps a per-repo cursor, fans out
~3 repos/tick (`DEFAULT_ROUND_ROBIN_N`), and periodically does a global sweep
so newly-added repos get discovered. The focused workspace is bumped to the
front of rotation on `Command::FocusWorkspace`. Errors become
`Event::ProviderError`; progress is `Event::PollProgress`. Config is reloaded
each tick, so `config.yaml` edits take effect within one interval without a
restart.

### Test checklist
- [ ] With several repos configured, each is polled in rotation (check `/tmp/lazybox.log` with `RUST_LOG=lazybox=debug`).
- [ ] Editing `poll_interval` in `config.yaml` changes cadence without restarting.
- [ ] A provider auth failure surfaces as a `ProviderError`, not a crash.
- [ ] Rate-limit `retry_after` headers back off rather than hammering the API.
- [ ] The focused workspace's repo is refreshed promptly after focusing it.

### Known sharp edges
- Scope budget is tuned for GitHub's 5000-points/hour PAT limit; a very large org scope can still exhaust it.
- Partial-coverage polls (one half of the PR/issue query fails) are detected and flagged to avoid dropping rows, but the heuristic is conservative.

---

## Manual refresh

**Status:** stable
**Crate(s):** `server`, `tui`
**Config / flags:** —
**Key bindings:** `Shift-R`

### What it does
Forces an immediate re-poll of every provider instead of waiting for the next
interval.

### How to use it
Press `Shift-R` from the sidebar.

### How it works (brief)
`Shift-R` dispatches `Command::Refresh`, which calls `poll_wake.notify_one()`
(`crates/server/src/lib.rs`). The polling loop selects on that `Notify` inside
its chunked sleep and wakes immediately.

### Test checklist
- [ ] `Shift-R` triggers a poll within ~1s (visible in debug logs / sync status).
- [ ] Refresh during an in-flight poll doesn't double-fire or deadlock.

### Known sharp edges
- Refresh re-polls all providers, not just the focused repo.

---

## Sync-status window

**Status:** stable
**Crate(s):** `tui` (`SyncStatus` modal), `server` (events)
**Config / flags:** —
**Key bindings:** `Shift-D`

### What it does
Opens a window showing per-provider poll outcomes: last poll time, in-flight
progress, and any errors surfaced by the loop.

### How to use it
Press `Shift-D` to open it; dismiss with `Esc`.

### How it works (brief)
The modal consumes `Event::PollProgress` and `Event::ProviderError` and renders
the latest state per provider. (`OpenSyncStatus` in
`crates/tui-core/src/action.rs`.)

### Test checklist
- [ ] `Shift-D` opens the window and shows the last poll timestamp per provider.
- [ ] An induced provider error (bad token) shows up here with a readable message.
- [ ] The window updates live while a poll is in flight.

---

## Filter menu

**Status:** stable
**Crate(s):** `tui-core` (`OpenFilterMenu` in `src/action.rs`), `tui` (sidebar)
**Config / flags:** `setup.filters` (per-provider role/type filters seed which rows are fetched)
**Key bindings:** `f`

### What it does
Opens a multi-select **filter menu** over three predicate axes — state
(with-agent, CI-failing, conflict, unread, asking, review-requested,
auto-merge), role (author / reviewer / assignee / mentioned), and kind
(PR / issue) — each shown with a live match count. Filters combine
AND-across-axes / OR-within-axis and render as removable chips in the sidebar
header.

### How to use it
Press `f`, toggle the predicates you want, confirm. Active filters show as
header chips (removable). Filters compose with `/` search.

### How it works (brief)
`OpenFilterMenu` (`crates/tui-core/src/action.rs`) mounts the multi-select;
the sidebar row model is re-filtered by the combined predicate set. Which rows
exist at all is still shaped by `setup.filters` (provider-side role/type
fetch filters).

### Test checklist
- [ ] `f` opens the menu with per-predicate match counts.
- [ ] Toggling `reviewer` hides PRs you only author.
- [ ] Predicates across axes combine with AND; within an axis with OR.
- [ ] Active filters render as removable header chips.
- [ ] Filters compose with `/` search.

### Known sharp edges
- Role is provider-assigned; if a provider can't classify a task's role, role predicates won't match it.

---

## Sort order

**Status:** stable
**Crate(s):** `tui` (`sidebar/handlers.rs`)
**Config / flags:** —
**Key bindings:** `o`

### What it does
Cycles sort order: `recent → by-role → split`. `split` inserts role-section
headers between groups within each repo.

### How to use it
Press `o` (or click the sort chip) to advance.

### Test checklist
- [ ] `o` cycles recent → by-role → split → recent.
- [ ] `split` shows role-section headers inside each repo group.
- [ ] `recent` orders rows by latest activity.

### Known sharp edges
- Sort interacts with the role filter; verify both compose as expected.

---

## Search

**Status:** stable
**Crate(s):** `tui` (`sidebar/handlers.rs`)
**Config / flags:** —
**Key bindings:** `/`

### What it does
Opens an incremental search bar that filters the sidebar by text, scoped to the
current project/group.

### How to use it
Press `/`, type to filter live, `Esc` to clear.

### Test checklist
- [ ] `/` opens the search bar and filters rows as you type.
- [ ] `Esc` closes search and restores the full list.
- [ ] Search composes with the active role filter.

### Known sharp edges
- Search is sidebar-local row filtering, not a provider-side query — it only matches already-fetched workspaces. The provider-token search grammar in `DESIGN.md` (`source:`, `ci:failed`, …) is a design target, not all wired yet.

---

## Mailbox cycle

**Status:** stable
**Crate(s):** `tui` (`sidebar/handlers.rs`)
**Config / flags:** —
**Key bindings:** `Shift-S`

### What it does
Cycles which mailbox the sidebar shows: `Inbox → Inactive → Snoozed → Inbox`.

### How to use it
Press `Shift-S` to advance through the mailboxes.

### Test checklist
- [ ] `Shift-S` cycles Inbox → Inactive → Snoozed → Inbox.
- [ ] Snoozed workspaces appear only in the Snoozed mailbox until their snooze expires.

### Known sharp edges
- Not surfaced in the README key tables; discoverable via the help overlay (`?`).

---

## Read/unread tracking

**Status:** stable
**Crate(s):** `tui`, `store` (persisted)
**Config / flags:** `ui.auto_mark_delay` (default 1s)
**Key bindings:** `m` mark read, `m` on a workspace row = bulk, `z` undo auto-mark (Activity)

### What it does
Each activity row tracks read/unread. Viewing a comment auto-marks it read
after a short delay; `m` marks explicitly; `m` on a workspace row bulk-marks all
of its activity; `z` (in the Activity pane) undoes the most recent auto-mark.

### How to use it
- Activity pane: `m` marks the focused comment read; `z` re-unreads the most recently auto-marked one.
- Sidebar: `m` on a workspace marks all its activity read. With Activity rows multi-selected (`v`), `m` marks just the selection.

### How it works (brief)
Read indices live on the `Workspace` (`read_indices`) and persist via the
store. The auto-mark timer (`ui.auto_mark_delay`) fires after a row stays
focused; `z` disarms the timer and reverts the last auto-mark.

### Test checklist
- [ ] Focusing a comment for >1s auto-marks it read.
- [ ] `z` immediately after an auto-mark re-unreads that row.
- [ ] `m` on a workspace row clears its entire unread count.
- [ ] Read state survives a restart (persisted in `state.db`).
- [ ] With rows multi-selected, `m` marks only the selected set.

### Known sharp edges
- Only the *most recent* auto-mark is undoable with `z`; older ones aren't.

---

## Snooze

**Status:** stable
**Crate(s):** `tui`, `store` (persisted)
**Config / flags:** `ui.short_snooze` (default 4h), `ui.long_snooze` (default 365d)
**Key bindings:** `z` (sidebar) short snooze toggle, `x z` long snooze (confirmed)

### What it does
Hides a workspace from the Inbox until its snooze expires, moving it to the
Snoozed mailbox. `z` on a sidebar row applies a short snooze (~4h, toggleable);
`x z` applies a long snooze (~1 year) behind a confirmation modal.

### How to use it
- Sidebar: `z` snoozes the focused workspace for the short window; press `z` again to un-snooze.
- `x z`, then confirm, applies the long snooze.
- View snoozed items via the Snoozed mailbox (`Shift-S`).

### How it works (brief)
`snoozed_until` on the `Workspace` is set to now + window and persisted. The
sidebar filters out snoozed workspaces from the Inbox mailbox until the
timestamp passes.

### Test checklist
- [ ] `z` snoozes a workspace and removes it from the Inbox.
- [ ] `z` again un-snoozes it.
- [ ] `x z`, then confirm, applies the long snooze.
- [ ] A snoozed workspace reappears in the Inbox after its window elapses.
- [ ] Snooze persists across restart.

### Known sharp edges
- `z` is overloaded: in the sidebar it's snooze; in the Activity pane it's *undo auto-mark-read*. Context matters.

---

## Multi-select & broadcast

**Status:** stable
**Crate(s):** `tui-core` (`BroadcastToSelected`, `UpdateBranchSelected` in `src/action.rs`), `tui` (sidebar)
**Config / flags:** —
**Key bindings:** `v` multi-select rows, `Shift-B` broadcast, `Shift-U` bulk update-branch

### What it does
`v` marks multiple workspace rows in the sidebar (marks survive `j/k`; `Esc`
clears). `Shift-B` then sends one instruction to every selected workspace;
`Shift-U` bulk-updates the branch of every selected PR that's behind its base
(#484).

### How to use it
Mark rows with `v`, then press `Shift-B`. A snippet picker opens first
(`Ctrl-F` skips it for free text) and feeds a compose textarea pre-filled with
the snippet body. Submit delivers per target: running agents get the
settle-gated inject, plain shells a direct write, and session-less workspaces
are skipped and named in the summary notice. `Shift-U` instead issues one
`UpdateBranch` per selected PR behind `main`; up-to-date and non-PR selections
are skipped and counted.

### How it works (brief)
`BroadcastToSelected` / `UpdateBranchSelected`
(`crates/tui-core/src/action.rs`) fan the sidebar's multi-select set out into
per-workspace commands; delivery reuses the same prompt-inject path as a
single-workspace send.

### Test checklist
- [ ] `v` marks survive cursor movement; `Esc` clears the marks.
- [ ] `Shift-B` delivers to agents (injected) and shells (written) and names skipped targets.
- [ ] `Ctrl-F` in the snippet picker jumps straight to free-text compose.
- [ ] `Shift-U` updates only the selected PRs that are behind; the summary counts skips.
