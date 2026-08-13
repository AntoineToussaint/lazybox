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
**Key bindings:** `Enter` open, `x` workspace menu (`x x` archive)

### What it does
Groups a task with everything you do about it. A `Workspace` holds at most one
PR, its linked GitHub/Linear issues, merged activity, read/snooze state, and
zero-or-more `Session`s (each an embedded terminal in a worktree).

### How to use it
Workspaces appear in the sidebar. `Enter` opens one; spawning a shell or agent
(`s`, `a c`/`a x`/`a u`, or `w w`) attaches a session; `x x` archives it.

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

## Worktree GC (CLI)

**Status:** stable
**Crate(s):** `tui-boot` (`src/worktree_gc.rs`), reuses `git-ops::inspect`
**Config / flags:** — (`--force`, `--dry-run` on `gc`)
**Key bindings:** — (CLI only; the in-TUI twin is Settings → Inspect/Clean worktrees, `Shift-D`)

### What it does
Makes the worktree leak visible and reclaimable from the command line, so it can
be cleaned up before the disk fills — without opening the TUI:

- `lazybox worktree list` — read-only report of every managed worktree with its
  size, branch, and orphan reasons, plus three totals: **worktree bytes on disk**
  (the `worktrees/` tree; bare clones under `repos/` are not counted), bytes
  **auto-reclaimable** by `gc`, and bytes in orphans **needing review** (the disk
  hogs usually land here — see below).
- `lazybox worktree gc` — reclaims the *safe* orphaned worktrees (merged/closed
  upstream, session stopped, or untracked; no uncommitted/unpushed work, not
  locked, and backed by a bare clone so the delete can be verified). Confirms
  first unless `--force`; `--dry-run` reports without deleting.

### How to use it
```
lazybox worktree list             # inventory + disk totals
lazybox worktree gc --dry-run      # preview what would be reclaimed
lazybox worktree gc                # reclaim (asks y/N first)
lazybox worktree gc --force        # reclaim without the prompt
```

### How it works (brief)
Both paths call `WorktreeManager::inspect_worktrees` with a `TrackedSession` list
read straight from `state.db` (no daemon), then `gc` reaps the rows where
`is_orphaned() && is_safe_to_delete && bare_path.is_some()` via
`delete_inspected(force=false)` — the same safety gate the TUI inspector uses.
The `bare_path.is_some()` clause matters: without a backing bare clone the
inspector can't verify a checkout holds no unpushed work, so `delete_inspected`
refuses those without force. `gc` therefore leaves them (and any dirty / unpushed
/ locked orphan) for the "needs review" bucket, which `list`/`gc` size and point
at the TUI inspector's per-row force. `gc` also refuses to run while a daemon (or
the embedded one behind a live TUI) is running — a standalone reap can't see the
daemon's in-memory live-terminal map — and re-checks that after the inspection
walk before deleting anything.

### Test checklist
- [ ] `lazybox worktree list` reports every worktree with sizes, plus the auto-reclaimable and needs-review totals.
- [ ] `lazybox worktree gc` only offers orphaned + safe + bare-clone-backed worktrees; dirty/unpushed/locked/no-bare ones are surfaced as "needs review" and skipped.
- [ ] `gc` without `--force` aborts on any answer other than `y`/`yes` (and on EOF from a pipe).
- [ ] `gc` refuses while lazybox is running (and re-checks after the inspection walk).

### Known sharp edges
- `gc` never reclaims an orphan that is dirty / unpushed / locked, or that has no backing bare clone (its content can't be verified disposable). Those are sized under "needs review" and reclaimed deliberately in the TUI inspector, which carries a per-row force.

---

## New pre-PR workspace

**Status:** stable
**Crate(s):** `tui`, `git-ops`
**Config / flags:** —
**Key bindings:** `x n`

### What it does
Creates a fresh workspace with a new branch off the latest `main`, before any
PR exists — for starting work from scratch.

### How to use it
Press `x n`, enter a name; lazybox creates the branch + worktree and opens the
workspace ready for an agent or shell.

### How it works (brief)
`NewWorkspace` (`crates/tui-core/src/action.rs`) prompts for a name and calls
`WorktreeManager::checkout_new_branch()` off the repo's base branch.

### Test checklist
- [ ] `x n` prompts for a name and creates a worktree on a new branch.
- [ ] The new branch is based on the latest `main`.
- [ ] You can immediately spawn an agent in the new workspace.

### Known sharp edges
- Needs a repo context to branch from; behavior with no scoped repo is undefined.

---

## New project

**Status:** beta
**Crate(s):** `tui`, `core`
**Config / flags:** —
**Key bindings:** `x p`

### What it does
Creates a local **project** container — a grouping for workspaces that isn't
tied to a single upstream PR/issue.

### How to use it
Press `x p` and enter a name.

### How it works (brief)
`NewProject` (`crates/tui-core/src/action.rs`) creates a `ProjectRecord`
persisted via the store; workspaces can reference it via `project_key`.

### Test checklist
- [ ] `x p` creates a named project visible in the sidebar grouping.
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

### Stopgap: one alternate app via `editors:`
Before `open_with:` existed, a single non-editor app (e.g. Obsidian) could be
added as an `editors:` entry and picked with `e`:

```yaml
editors:
  - id: obsidian
    display: Obsidian
    command: open
    args: ["-a", "Obsidian", "{path}"]
```

This still works, but `editors:` is scoped to *the* code editor (it carries
go-to-`file:line` semantics). For multiple distinct "open with" targets, use
`open_with:` below.

---

## Open with… (arbitrary apps)

**Status:** stable
**Crate(s):** `config` (`OpenWithEntry`), `tui-core` (`editors::OpenWithApp`), `tui`
**Config / flags:** `open_with:`
**Key bindings:** `x o`

### What it does
A config-driven list of arbitrary apps to launch on the focused workspace —
Obsidian, Finder, a browser, anything — decoupled from the single `e` code
editor. `x o` opens an "Open with…" picker over the configured apps (one app
launches directly).

### How to use it
Add apps under `open_with:`; each is a launch command with optional `args`:

```yaml
open_with:
  - name: Obsidian
    command: open
    args: ["-a", "Obsidian", "{path}"]
  - name: Finder
    command: open
    args: ["{path}"]
  - name: Preview PR in browser
    command: open
    args: ["{url}"]
```

Tokens substituted at launch: `{path}` (the worktree dir), `{url}` (the PR /
issue URL), `{branch}`, and `{repo}` (`owner/repo`). `args` defaults to
`["{path}"]` when omitted. An app that references a token the workspace can't
supply (e.g. `{url}` with no PR) fails with a footer notice naming the token,
rather than launching with a stray placeholder.

### How it works (brief)
`open_with:` entries load at startup and reuse the editor launch primitive
(`tui-core` `editors::launch_open_with`): command + args + token substitution,
detaching a GUI binary and handing `open` off to Launch Services. `editors:` /
`e` are unchanged — `open_with:` is the general escape hatch.

### Test checklist
- [ ] `x o` with 2+ apps shows a picker; a single app launches directly.
- [ ] `{path}` / `{url}` / `{branch}` / `{repo}` are substituted at launch.
- [ ] An app using `{url}` on a workspace with no PR fails with a named notice.
- [ ] With no `open_with:` configured, `x o` points at the config file.

### Known sharp edges
- Local-only, like the editor (#742): a remote (`--connect`) worktree path lives on the box, so `x o` declines on a remote daemon and points at `s` for a server shell.

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

## Update branch

**Status:** stable
**Crate(s):** `tui-core` (`UpdateBranch` / `UpdateBranchSelected` in `src/action.rs`), `server`, `gh-provider`
**Config / flags:** —
**Key bindings:** `g u` (single PR), `Shift-U` (bulk, over the sidebar multi-select)

### What it does
The "Update branch" button, in the TUI (#484): merges the base branch into the
PR's head so a behind-`main` PR catches up without leaving the inbox. `Shift-U`
fans it out over the sidebar's multi-select set.

### How to use it
On a PR that's behind its base, press `g u`. For a batch: mark rows with `v`,
then `Shift-U` — each selected behind PR gets its own `UpdateBranch`;
up-to-date and non-PR selections are skipped and counted in the summary.

### How it works (brief)
`UpdateBranch` dispatches the provider's update-branch mutation; the action
only surfaces when the PR is actually behind its base.

### Test checklist
- [ ] `g u` appears only on a PR behind its base.
- [ ] A successful update reflects on the next poll (behind-ness clears).
- [ ] `Shift-U` updates each selected behind PR and counts the skips.

---

## Automation policies menu

**Status:** stable
**Crate(s):** `tui-core` (`ManagePolicies` in `src/action.rs`), `tui`, `server`
**Config / flags:** `auto_fix.*` (global auto-fix), `auto_fix.opt_out_labels` (default `no-auto-fix`, `do-not-lazybox`)
**Key bindings:** `g p` (menu), `g g` (toggle merge-on-green directly),
`Shift-A` (toggle both auto-fix kinds directly)

### What it does
One surface for the focused PR/issue's automation (#363): lazybox's
merge-on-green arm, the per-session auto-fix arm/disarm, and GitHub-native
auto-merge status — each toggled in place. Armed policies surface as sidebar
row pills: `ARM` (merge-on-green) and `FIX` (auto-fix); the focused auto-fix
row expands the pill to name whether CI failures, conflicts, or both are armed.

### How to use it
Press `g p` on a workspace and toggle entries in place. `g g` flips
merge-on-green without opening the menu (own PR, no conflicts, no changes
requested; lazybox merges once CI passes, and only while lazybox runs).
`Shift-A` arms or disarms CI-failure and merge-conflict auto-fix together. The
per-session auto-fix arm overrides the global `no-auto-fix` /
`do-not-lazybox` label opt-out, which the menu still reflects.

### Test checklist
- [ ] `g p` lists merge-on-green, auto-fix, and GitHub auto-merge with current state.
- [ ] `Shift-A` toggles both auto-fix kinds without opening the policy menu.
- [ ] Arming merge-on-green shows `ARM`; auto-fix shows `FIX`, expanded on the focused row.
- [ ] A per-session auto-fix arm wins over the label opt-out.
- [ ] A red PR waits for an existing agent to reach Done before auto-fix injects.
- [ ] Merge-on-green only fires on a green, conflict-free, own PR while lazybox runs.

---

## Archive workspace

**Status:** stable
**Crate(s):** `tui`, `git-ops`
**Config / flags:** `worktree.auto_cleanup_merged`
**Key bindings:** `x x`

### What it does
Archives a workspace and cleans up its worktree, killing any running sessions
first.

### How to use it
Press `x x`; confirm in the modal. Running sessions are terminated and the
worktree is removed.

### How it works (brief)
`Archive` (`crates/tui-core/src/action.rs`) confirms, kills sessions, and calls
the worktree manager's removal path. With `worktree.auto_cleanup_merged: true`,
merged PRs' worktrees are reaped automatically.

### Test checklist
- [ ] `x x` opens a Confirm modal.
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
**Key bindings:** `x a`

### What it does
Moves every session from one workspace into another — useful when work started
on a pre-PR workspace and you want it under the real PR's workspace.

### How to use it
Press `x a` on the source workspace; pick the target in the picker.

### How it works (brief)
`AdoptSessions` (`crates/tui-core/src/action.rs`) reparents the source's
`Session`s onto the chosen target workspace.

### Test checklist
- [ ] `x a` opens a target picker.
- [ ] Sessions move to the chosen workspace and keep running.
- [ ] The source workspace is left without those sessions.

### Known sharp edges
- The worktree path doesn't move; adopted sessions still point at their original worktree dir.

---

## Import local checkout (linked, no-worktree workspace)

**Status:** beta
**Crate(s):** `tui`, `server`, `git-ops`, `core`
**Config / flags:** `scan.roots`, `scan.max_depth`
**Key bindings:** `x i`

### What it does
Meets your repos where they already live. If you keep a canonical dev folder
(`~/development/<owner>/<repo>`, `~/code/…`) with one clone per repo, `x i`
scans those roots, maps each checkout to its GitHub repo, and imports a chosen
one as a **linked workspace**: lazybox points at the existing checkout and runs
every agent/shell **directly in it** — no worktree provisioned, no bare clone,
and the current branch is never switched. The row carries a `⎇ local` badge so
it's always clear you're working in your real tree.

### How to use it
Set your dev roots in `~/.lazybox/config.yaml`:

```yaml
scan:
  roots:
    - ~/development
    - ~/code
```

Press `x i`, pick a discovered checkout, and confirm the "runs in your real
checkout" warning. The repo's PR/issue/CI activity groups under the same
project header as any other workspace on that repo.

### How it works (brief)
`ImportCheckout` (`crates/tui-core/src/action.rs`) sends `ScanCheckouts`; the
daemon walks `scan.roots` (`git-ops::scan_external_checkouts`) and replies with
`CheckoutsDiscovered`. Picking one sends `ImportLocalCheckout`, which
re-describes the path, maps `origin` → `owner/repo`
(`core::github_owner_repo_from_url`), and creates a `Workspace` with
`linked_checkout` set (`server::polling::import_local_checkout`). At spawn time
`resolve_or_create_session` lands sessions straight in that path with no
provisioning; the spawn is treated as on-main so one agent runs per checkout
(no duplicate agents in your real tree) and no session is persisted that a
cleanup path could delete.

### Test checklist
- [ ] `x i` scans `scan.roots` and lists discovered checkouts mapped to their repos.
- [ ] Importing creates a `⎇ local` row; agents/shells run in the existing checkout.
- [ ] The current branch is respected (no forced switch); dirty state is surfaced before import.
- [ ] A second agent spawn reuses the first (no duplicate in the real tree).

### Known sharp edges
- Sessions edit your **real** working directory, not an isolated worktree — the import confirm + `⎇ local` badge are the guards.
- The repo's PR/issue/CI activity flows in only when that repo is polled (you're involved in its PRs, or it's in a `watch` filter); the imported workspace itself is a tracking row, and polled PRs/issues appear as sibling workspaces under the shared project.

---

## Collapse into PR

**Status:** beta
**Crate(s):** `tui`
**Config / flags:** —
**Key bindings:** `x j`

### What it does
Folds an issue workspace into the PR that closes it, so the two collapse to a
single row.

### How to use it
Press `x j` on the issue workspace; if multiple candidate PRs exist, a
picker appears.

### How it works (brief)
`CollapseIntoPr` (`crates/tui-core/src/action.rs`) links the issue's workspace
to the closing PR using the `closes_issues` relationship on the `Task`.

### Test checklist
- [ ] `x j` collapses an issue into its closing PR's row.
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
