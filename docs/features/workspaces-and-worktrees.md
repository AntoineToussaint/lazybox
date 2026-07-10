# Workspaces & git worktrees

A **workspace** is lazybox's unit of work: a task (PR / issue / ticket) plus the
git worktree and terminal sessions attached to it. This page covers the
workspace model, the worktree manager that backs it, the lifecycle actions
(new / merge / archive / adopt / collapse), editor and per-repo integration,
and what persists.

See [Terminals & agents](terminals-and-agents.md) for what runs *inside* a
workspace's worktree.

---

## Workspace model & lifecycle

**Status:** stable
**Crate(s):** `core` (`src/workspace.rs`), `server`
**Config / flags:** —
**Key bindings:** `Enter` open, `Shift-X` archive

### What it does
Groups a task with everything you do about it. A `Workspace` holds at most one
PR, its linked GitHub/Linear issues, merged activity, read/snooze state, and
zero-or-more `Session`s (each an embedded terminal in a worktree).

### How to use it
Workspaces appear in the sidebar. `Enter` opens one; spawning a shell or agent
(`s`, `a c`/`a x`/`a u`, or `w`) attaches a session; `Shift-X` archives it.

### How it works (brief)
`Workspace` (`crates/core/src/workspace.rs`) carries `key`, optional
`project_key`, `branch`, `sessions: Vec<Session>`, `pr`, `gh_issues`,
`linear_issues`, merged `activity`, `read_indices`, `snoozed_until`, and view
timestamps. A `Session` has an `id` (UUID), `kind` (Agent/Shell),
`run_state`, and a `worktree_path`. The server reconciles sessions against the
worktree layout on startup.

### Test checklist
- [ ] A task with a branch yields a workspace you can open and attach sessions to.
- [ ] Multiple sessions can coexist on one workspace (e.g. a shell + an agent).
- [ ] Workspace metadata (sessions, read state) survives a restart.

### Known sharp edges
- A branch-less task is "just an inbox row" — opening it shows activity, not a worktree.

---

## Worktree manager

**Status:** stable
**Crate(s):** `git-ops` (`src/lib.rs`, `src/inspect.rs`)
**Config / flags:** `worktree.mounts`, `worktree.scripts`, `worktree.auto_cleanup_merged`, `worktree.branch_prefix`, `repos.<owner/name>.branch_prefix`
**Key bindings:** — (created implicitly when you spawn a session)

### What it does
Owns the on-disk git layout: a **bare clone per repo** plus a **per-task
worktree** checked out from the latest `main` (or the PR's branch). Applies
configured mounts (symlinks) and materializes scripts into each worktree.

For a branch-less task (an issue, a Linear ticket, a blank workspace) the
manager cuts a fresh branch off the repo default. The name is derived from the
work — `issue-42-standardize-log-output` for a GitHub issue, the workspace key
for a blank workspace — and is deterministic, so a second spawn on the same
item reuses the one branch. Set `worktree.branch_prefix` (empty by default) to
namespace these branches — `lazybox` restores the historical
`lazybox/issue-42` layout — or override it per-repo with
`repos.<owner/name>.branch_prefix` to match a team's convention.

### How to use it
It runs implicitly: spawning a shell/agent on a workspace creates its worktree
if needed. Manage stale worktrees from Settings → Inspect/Clean worktrees.

### How it works (brief)
`WorktreeManager` (`crates/git-ops/src/lib.rs`) keeps bare clones at
`~/.lazybox/v2/repos/<owner>/<repo>.git` and worktrees under
`~/.lazybox/v2/worktrees/`. `checkout()` is idempotent (returns the existing
worktree if present) and falls back from `refs/remotes/origin/<branch>` to a
local branch. `checkout_new_branch()` creates a worktree from a fresh branch off
a base. Mounts symlink shared dirs (`placement: inside|above`); scripts
materialize to `<worktree>/_lazybox/scripts/<name>` (inline body or symlinked
source). Cleanup (`remove_by_path`) removes the worktree and falls back to
`rm -rf` + `git worktree prune`; orphan detection lives in `inspect.rs`.

### Test checklist
- [ ] First session on a repo creates a bare clone, then a worktree.
- [ ] A second session on the same task reuses the existing worktree (idempotent).
- [ ] A pre-PR workspace gets a worktree off a fresh branch from latest `main`.
- [ ] Configured mounts appear as symlinks in new worktrees.
- [ ] Configured scripts appear at `_lazybox/scripts/<name>` and are executable.
- [ ] Archiving removes the worktree from disk and prunes git's worktree list.

### Known sharp edges
- Removal falls back to `rm -rf` if `git worktree remove` fails — make sure the target really is a lazybox worktree before forcing.
- Bare clone + worktrees share one fetch; a corrupt bare clone affects all worktrees of that repo.

---

## New pre-PR workspace

**Status:** stable
**Crate(s):** `tui`, `git-ops`
**Config / flags:** —
**Key bindings:** `n`

### What it does
Creates a fresh workspace with a new branch off the latest `main`, before any
PR exists — for starting work from scratch.

### How to use it
Press `n`, enter a name; lazybox creates the branch + worktree and opens the
workspace ready for an agent or shell.

### How it works (brief)
`NewWorkspace` (`crates/tui-core/src/action.rs`) prompts for a name and calls
`WorktreeManager::checkout_new_branch()` off the repo's base branch.

### Test checklist
- [ ] `n` prompts for a name and creates a worktree on a new branch.
- [ ] The new branch is based on the latest `main`.
- [ ] You can immediately spawn an agent in the new workspace.

### Known sharp edges
- Needs a repo context to branch from; behavior with no scoped repo is undefined.

---

## New project

**Status:** beta
**Crate(s):** `tui`, `core`
**Config / flags:** —
**Key bindings:** `Shift-N`

### What it does
Creates a local **project** container — a grouping for workspaces that isn't
tied to a single upstream PR/issue.

### How to use it
Press `Shift-N` and enter a name.

### How it works (brief)
`NewProject` (`crates/tui-core/src/action.rs`) creates a `ProjectRecord`
persisted via the store; workspaces can reference it via `project_key`.

### Test checklist
- [ ] `Shift-N` creates a named project visible in the sidebar grouping.
- [ ] Projects persist across restart.

### Known sharp edges
- Newer than the PR/issue flow; project grouping semantics are still settling.

---

## Editor integration

**Status:** stable
**Crate(s):** `config` (`EditorEntry`), `tui`
**Config / flags:** `editors:` (custom/override entries)
**Key bindings:** `e`

### What it does
Opens the focused workspace's worktree in your editor. Detects Zed / VS Code /
Cursor / Windsurf / Fleet / IDEA / Gram at startup; custom editors and
overrides come from `editors:`.

### How to use it
Press `e` on a workspace. If multiple editors are detected/configured, a picker
appears. Add entries in config:

```yaml
editors:
  - id: my-editor
    display: "My Editor"
    command: /opt/myeditor/bin/edit
    args: ["--workspace", "{path}"]
```

`{path}` is replaced with the worktree dir at launch.

### How it works (brief)
Detection runs at startup; `e` resolves the editor command and spawns it with
`{path}` substituted (`crates/config` `EditorEntry`).

### Test checklist
- [ ] `e` opens the worktree in the detected editor.
- [ ] With multiple editors, a picker lets you choose.
- [ ] A custom `editors:` entry appears and launches with `{path}` substituted.
- [ ] Overriding a builtin id (e.g. `zed`) replaces its command.

### Known sharp edges
- Detection is best-effort by binary path; an editor installed in a nonstandard location needs a custom `editors:` entry.

---

## Per-repo overrides

**Status:** stable
**Crate(s):** `config` (`RepoConfig`), `git-ops`
**Config / flags:** `repos.<owner/name>.{env, mounts, scripts}`
**Key bindings:** —

### What it does
Injects env vars, symlinks shared folders, and materializes scripts into a
specific repo's worktrees — so different projects can have different
`DATABASE_URL`s or shared vendored code without committing it.

### How to use it
Key by `owner/name` as GitHub reports it:

```yaml
repos:
  tensorzero/tensorzero:
    env:
      DATABASE_URL: postgres://localhost/dev
    mounts:
      - source: ~/shared/tensor-data
        link_at: _imports/data
    scripts:
      - name: cleanup
        source: ~/dev/scripts/rust-cleanup.sh
```

`env` is injected into every shell/agent PTY in that repo's worktrees (stacked
on the daemon env, per-repo wins on collision). `mounts` and `scripts` stack on
top of the global `worktree.*` lists.

### How it works (brief)
`RepoConfig` (`crates/config/src/lib.rs`) carries `env`, `mounts`, `scripts`.
On spawn, the server merges repo env over process env; the worktree manager
applies repo mounts/scripts after `git worktree add`. See the README
"Per-repo overrides" section for `placement` and `content` vs `source` details.

### Test checklist
- [ ] A `repos.<r>.env` var is present in a shell spawned in that repo's worktree.
- [ ] Per-repo env wins over a colliding daemon-env var.
- [ ] A per-repo mount appears as a symlink at `link_at`.
- [ ] A per-repo script appears at `_lazybox/scripts/<name>`.
- [ ] Repo overrides stack on top of global `worktree.mounts`/`scripts`.

### Known sharp edges
- Keyed by exact `owner/name` casing GitHub reports — a mismatch silently applies nothing.

---

## Merge PR

**Status:** stable
**Crate(s):** `tui`, `gh-provider`
**Config / flags:** —
**Key bindings:** `g m`

### What it does
Merges the workspace's PR behind a confirmation modal, when CI is green, the PR
is approved, and there are no conflicts.

### How to use it
Press `g m`; a Confirm modal appears — arrows/Tab navigate
Yes/No, `Enter` confirms.

### How it works (brief)
`MergePr` (`crates/tui-core/src/action.rs`) dispatches the provider's
`merge_workspace` (GitHub GraphQL mutation) after the confirm modal.

### Test checklist
- [ ] `g m` opens a Confirm modal.
- [ ] Confirming merges a green/approved/conflict-free PR; the row reflects merged on next poll.
- [ ] Cancelling leaves the PR untouched.

### Known sharp edges
- This is a real, hard-to-reverse GitHub mutation — the confirm modal is the only guard.

---

## Archive workspace

**Status:** stable
**Crate(s):** `tui`, `git-ops`
**Config / flags:** `worktree.auto_cleanup_merged`
**Key bindings:** `Shift-X`

### What it does
Archives a workspace and cleans up its worktree, killing any running sessions
first.

### How to use it
Press `Shift-X`; confirm in the modal. Running sessions are terminated and the
worktree is removed.

### How it works (brief)
`Archive` (`crates/tui-core/src/action.rs`) confirms, kills sessions, and calls
the worktree manager's removal path. With `worktree.auto_cleanup_merged: true`,
merged PRs' worktrees are reaped automatically.

### Test checklist
- [ ] `Shift-X` opens a Confirm modal.
- [ ] Confirming kills running sessions and removes the worktree from disk.
- [ ] Cancelling leaves sessions and worktree intact.
- [ ] With `auto_cleanup_merged: true`, a merged PR's worktree is reaped without manual archive.

### Known sharp edges
- Destructive: archiving removes the on-disk worktree (uncommitted work in it is lost). The confirm modal is the guard.

---

## Adopt sessions

**Status:** beta
**Crate(s):** `tui`
**Config / flags:** —
**Key bindings:** `Shift-A`

### What it does
Moves every session from one workspace into another — useful when work started
on a pre-PR workspace and you want it under the real PR's workspace.

### How to use it
Press `Shift-A` on the source workspace; pick the target in the picker.

### How it works (brief)
`AdoptSessions` (`crates/tui-core/src/action.rs`) reparents the source's
`Session`s onto the chosen target workspace.

### Test checklist
- [ ] `Shift-A` opens a target picker.
- [ ] Sessions move to the chosen workspace and keep running.
- [ ] The source workspace is left without those sessions.

### Known sharp edges
- The worktree path doesn't move; adopted sessions still point at their original worktree dir.

---

## Collapse into PR

**Status:** beta
**Crate(s):** `tui`
**Config / flags:** —
**Key bindings:** `Shift-J`

### What it does
Folds an issue workspace into the PR that closes it, so the two collapse to a
single row.

### How to use it
Press `Shift-J` on the issue workspace; if multiple candidate PRs exist, a
picker appears.

### How it works (brief)
`CollapseIntoPr` (`crates/tui-core/src/action.rs`) links the issue's workspace
to the closing PR using the `closes_issues` relationship on the `Task`.

### Test checklist
- [ ] `Shift-J` collapses an issue into its closing PR's row.
- [ ] With multiple closing PRs, a picker disambiguates.
- [ ] The collapsed row shows both issue and PR activity.

### Known sharp edges
- Relies on the provider correctly populating `closes_issues` (e.g. `Closes #N` in the PR body).

---

## State persistence

**Status:** stable
**Crate(s):** `store` (`src/sqlite.rs`)
**Config / flags:** state DB at `~/.lazybox/v2/state.db` (rooted at `LAZYBOX_HOME`)
**Key bindings:** —

### What it does
Persists workspace activity, read/unread, snooze, project records, and the
terminal scrollback ring across launches, so the inbox picks up where you left
off.

### How to use it
Automatic. `lazybox --fresh` wipes it; `LAZYBOX_HOME` points it elsewhere for a
side-by-side profile.

### How it works (brief)
`SqliteStore` (`crates/store/src/sqlite.rs`) is a key/value table: workspaces
are stored as `workspace:<key>` → JSON and projects as `project:<key>` → JSON,
listed by prefix scan. The connection is guarded by a `parking_lot::Mutex`.
The `Store` trait (`src/traits.rs`) is the swap point for other backends.

### Test checklist
- [ ] Read/unread, snooze, and sessions survive a restart.
- [ ] `lazybox --fresh` clears the DB and re-runs setup.
- [ ] `LAZYBOX_HOME=~/.lazybox-dev lazybox` uses a separate DB (zero shared state).
- [ ] Deleting a workspace removes its row from the store.

### Known sharp edges
- Storage is JSON-blob-in-KV, not a normalized schema — fine for current scale, but ad-hoc SQL queries over fields aren't possible without parsing the blobs.
