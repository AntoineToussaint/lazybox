---
title: Configuration reference
description: The complete ~/.lazybox/config.yaml schema with an annotated example.
---

lazybox reads `~/.lazybox/config.yaml`. Every key is optional — lazybox runs
with an empty config by auto-detecting agents and editors and reading GitHub
credentials from `gh auth token`. The sections below document every top-level
key.

`LAZYBOX_HOME` overrides the base directory lazybox writes to, which moves the
config, state, and worktree paths accordingly.

This page tracks the config struct in
[`crates/config/src/lib.rs`](https://github.com/AntoineToussaint/lazybox/blob/main/crates/config/src/lib.rs),
which is the canonical source of truth for defaults and field names.

## Top-level keys

| Key | Purpose |
| --- | --- |
| [`editors`](#editors) | Override / extend the detected editors |
| [`repos`](#repos) | Per-repo env, mounts, scripts, branch prefix |
| [`agent_shortcuts`](#agent_shortcuts) | Single-char keys → agent ids |
| [`agent`](#agent) | Agent command, args, permission prompts |
| [`worktree`](#worktree) | Global mounts, scripts, branch prefix, merged-cleanup |
| [`terminal`](#terminal) | Terminal escape chord + scrollback behavior |
| [`ui`](#ui) | View state, key remaps, timings, browser |
| [`display`](#display) | Sort, filtering, glyphs |
| [`attention`](#attention) | Which signals raise the per-repo badge |
| [`providers`](#providers) | GitHub polling + filters |
| [`slack`](#slack) | Slack mirror tokens + channels |
| [`hooks`](#hooks) | Periodic maintenance scripts |
| [`mention`](#mention) | Auto-spawn on `@lazybox` mention |
| [`auto_fix`](#auto_fix) | Auto-fix PRs on CI failure / conflict |
| [`shell`](#shell) | Shell command for the `s` spawn |

## Annotated example

```yaml
# ── editors ──────────────────────────────────────────────────────────
# Overrides/extends the editors lazybox auto-detects (Zed, VS Code, Cursor,
# Windsurf, Fleet, IDEA, Gram). {path} expands to the worktree directory.
editors:
  - id: zed
    display: Zed
    command: zed
    args: ["{path}"]

# ── repos ────────────────────────────────────────────────────────────
# Per-repo overrides keyed by owner/name.
repos:
  acme/widgets:
    # Injected into every shell/agent PTY in this repo's worktrees.
    env:
      DATABASE_URL: postgres://localhost/widgets_dev
      RUST_LOG: widgets=debug
    # Symlink shared directories into each worktree.
    mounts:
      - source: /Users/me/widgets-shared/node_modules
        link_at: node_modules
        placement: inside     # inside | above
    # Materialize executables at <worktree>/_lazybox/scripts/<name>.
    scripts:
      - name: seed-db
        content: |
          #!/usr/bin/env bash
          set -euo pipefail
          psql "$DATABASE_URL" -f db/seed.sql
    # Override the worktree.branch_prefix for this repo only.
    branch_prefix: at        # → at/issue-42

# ── agent_shortcuts ──────────────────────────────────────────────────
# Single-char keys → agent ids. Defaults: c→claude, x→codex, u→cursor.
agent_shortcuts:
  a: aider

# ── agent ────────────────────────────────────────────────────────────
agent:
  # Autonomous @lazybox work runs Claude with --dangerously-skip-permissions.
  autonomous_skip_permissions: true
  # Skip permission prompts for interactively spawned agents too.
  skip_permissions: false

# ── worktree ─────────────────────────────────────────────────────────
worktree:
  branch_prefix: ""          # "" → issue-42; "lazybox" → lazybox/issue-42
  auto_cleanup_merged: false # reap worktrees when their PR merges
  mounts:
    - source: ~/shared/cache
      link_at: .cache
      placement: inside

# ── terminal ─────────────────────────────────────────────────────────
terminal:
  escape_char: "]"           # repeat this char to exit to the sidebar
  escape_count: 2            # how many in a row (≥ 2)
  escape_window_ms: 600      # window between presses
  native_scrollback: true    # keep scrollback in the lazybox client

# ── ui ───────────────────────────────────────────────────────────────
ui:
  # Remap any catalog action. Keys are snake_case action ids; values are
  # key-spec strings. Unset actions keep their default binding.
  action_keys:
    merge_pr: Ctrl-m
    refresh: Ctrl-r
    spawn_shell: t
  terminal_escape_char: "]"
  short_snooze: 4h
  long_snooze: 365d
  browser: Google Chrome
  log_path: /tmp/lazybox.log

# ── providers ────────────────────────────────────────────────────────
providers:
  github:
    poll_interval: 60        # seconds
    detect_needs_reply: true  # show/hide needs-reply badges
    filters:
      - org: acme
      - repo: acme/widgets
      - watch: acme/infra     # all open PRs, regardless of involvement

# ── slack ────────────────────────────────────────────────────────────
slack:
  bot_token: xoxb-your-bot-token
  app_token: xapp-your-app-token
  anchor_channel: lazybox-inbox
  per_workspace_channels: true
```

## `editors`

A list of editor definitions that override or extend the detected set
(Zed, VS Code, Cursor, Windsurf, Fleet, IDEA, Gram). An entry whose `id`
matches a builtin overrides it; a new `id` adds an editor.

| Field | Type | Description |
| --- | --- | --- |
| `id` | string | Stable identifier |
| `display` | string | Name shown in the picker |
| `command` | string | Executable to run |
| `args` | list of string | Arguments; `{path}` expands to the worktree directory |

## `repos`

A map keyed by `owner/name` (the `repo.full_name` GitHub returns). Each entry
applies only to worktrees / spawns in that repo.

### `env`

| Type | Description |
| --- | --- |
| map of string → string | Environment variables injected into every shell and agent PTY in that repo's worktrees. Layered over the daemon env and global `agent.env`; the per-repo value wins. |

### `mounts`

A list of symlink definitions, stacked on top of [`worktree.mounts`](#worktree):

| Field | Type | Description |
| --- | --- | --- |
| `source` | path | Existing directory to link (`~/…` expanded) |
| `link_at` | path | Where the symlink is created (relative) |
| `placement` | `inside` \| `above` | `inside` the worktree, or `above` it (parent). Default `inside`. |

### `scripts`

A list of executables materialized at `<worktree>/_lazybox/scripts/<name>`,
stacked on top of [`worktree.scripts`](#worktree):

| Field | Type | Description |
| --- | --- | --- |
| `name` | string | File name under `_lazybox/scripts/` |
| `content` | string | Inline script body |
| `source` | path | Alternative to `content`: symlink this file |

Provide either `content` or `source` per script, never both.

### `branch_prefix`

| Type | Description |
| --- | --- |
| string | Override [`worktree.branch_prefix`](#worktree) for this repo. `"at"` → `at/issue-42`; `""` drops the prefix (`issue-42`); omit to inherit the global value. |

See [Per-repo env & mounts](/docs/how-to/per-repo-env-and-mounts/) for a
walkthrough.

## `agent_shortcuts`

Single-character keys mapped to agent ids. Press the key in the sidebar to
spawn that agent on the focused workspace.

| Type | Default | Description |
| --- | --- | --- |
| map of char → string | `c → claude`, `x → codex`, `u → cursor` | Remap the built-ins or add custom CLIs (e.g. `a: aider`). |

## `agent`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | `Claude Code` | Display name of the default agent |
| `command` | string | `claude` | Executable to spawn |
| `args` | list of string | `[]` | Extra args for the first launch |
| `resume_args` | list of string | `["--continue"]` | Args used to resume a session |
| `asking_patterns` | list of string | `(y/n)`, `do you want`, … | Output substrings that mark the agent as waiting on input |
| `autonomous_skip_permissions` | bool | `true` | Autonomous `@lazybox` work runs Claude with `--dangerously-skip-permissions` (blast radius bounded to the worktree) |
| `skip_permissions` | bool | `false` | Skip permission prompts for interactively spawned agents too |

## `worktree`

Global worktree layout — applied to every checkout, with `repos.<owner/name>`
overrides stacked on top.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `mounts` | list | `[]` | Symlinks to add to every worktree (same fields as [`repos.mounts`](#mounts)) |
| `scripts` | list | `[]` | Executables to materialize in every worktree (same fields as [`repos.scripts`](#scripts)) |
| `auto_cleanup_merged` | bool | `false` | When a tracked PR merges, reap the worktrees backing its sessions — only the ones with no locked / uncommitted / unpushed work and no live terminal |
| `branch_prefix` | string | `""` | Prefix for branches lazybox cuts itself (issues, blank workspaces). `""` → `issue-42`; `"lazybox"` → `lazybox/issue-42`. Overridable per repo. |

## `terminal`

How you exit an embedded terminal back to the inbox, and how scrollback works.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `escape_char` | char | `]` | Char that, when repeated `escape_count` times, exits to the sidebar |
| `escape_count` | int | `2` | How many `escape_char` presses in a row trigger the escape (must be ≥ 2) |
| `escape_window_ms` | int | `600` | Time window between consecutive presses to count as one run |
| `native_scrollback` | bool | `true` | Keep scrollback local to the lazybox client so the wheel / `Shift-PageUp` scroll instantly. Set `false` to let tmux own the alternate screen. |

## `ui`

View state lazybox writes back automatically (collapsed repos, splitter sizes),
plus knobs you can set by hand.

### `action_keys` — remapping keys

`action_keys` is the key-remapping surface. Keys are the snake_case action ids;
values are key-spec strings. lazybox consults this map first and falls back to
the catalog default for any action you don't list. An unparseable value falls
back to the default rather than breaking the keyboard, so a typo is harmless.

```yaml
ui:
  action_keys:
    merge_pr: Ctrl-m       # was Shift-M
    refresh: Ctrl-r        # was Shift-R
    spawn_shell: t         # was s
    quit: Ctrl-q           # single press (default is the q q chord)
```

**Key-spec format:**

- Modifiers `Ctrl-`, `Shift-`, `Alt-`, stackable in that order
  (`Ctrl-Shift-D`).
- A single character (`m`, `,`). An uppercase letter implies Shift, so `M` and
  `Shift-M` are equivalent.
- Named keys: `Tab`, `Enter`, `Esc`, `Space`, `Backspace`, `Up`, `Down`,
  `Left`, `Right`, `Home`, `End`, `PageUp`/`PgUp`, `PageDown`/`PgDn`,
  `Delete`/`Del`, `Insert`.
- A two-key chord repeats the same key separated by a space (`q q`). Function
  keys (`F5`) are not supported yet.

**Common action ids** (the full set is the snake_case names of every catalog
action in
[`crates/tui-core/src/action.rs`](https://github.com/AntoineToussaint/lazybox/blob/main/crates/tui-core/src/action.rs)):

| Action id | Default | Does |
| --- | --- | --- |
| `refresh` | `Shift-R` | Re-poll every provider |
| `open_help` | `?` | Help modal |
| `open_settings` | `,` | Settings palette |
| `quit` | `q q` | Quit |
| `work` | `w` | Spawn the default agent with a contextual prompt |
| `spawn_shell` | `s` | Open a shell in the worktree |
| `open_editor` | `e` | Open the worktree in your editor |
| `mark_all_read` | `m` | Mark the workspace read |
| `toggle_snooze` | `z` | Snooze (~4h) |
| `merge_pr` | `Shift-M` | Merge the PR |
| `request_reviewers` | `Shift-V` | Request reviewers |
| `add_assignees` | `Shift-G` | Change assignees |
| `manage_labels` | `Shift-L` | Edit labels |
| `open_in_browser` | `Shift-O` | Open the PR / issue in the browser |
| `archive` | `Shift-X` | Archive the workspace |
| `new_workspace` | `n` | New pre-PR workspace |
| `new_project` | `Shift-N` | New project / pick a repo |

See the [keybindings reference](/docs/reference/keybindings/) for the full
default keymap.

### Other `ui` keys

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `collapsed_repos` | list of string | `[]` | Repo names whose sidebar group starts collapsed (written back automatically) |
| `sidebar_pct` | int | `40` | Sidebar width as a percent of the screen |
| `right_top_pct` | int | `25` | Activity-row height as a percent of the right column |
| `auto_mark_delay` | duration | `1s` | How long the cursor sits on an unread row before it auto-marks read |
| `quit_double_tap_window` | duration | `800ms` | Window for the second `q` of the quit chord |
| `terminal_escape_char` | char | `]` | Char that returns focus from the terminal to the sidebar |
| `split_step_percent` | int | `3` | Percent a `Shift-arrow` nudges the focused splitter |
| `task_body_max_rows` | int | `8` | Max rows the description section expands to |
| `short_snooze` | duration | `4h` | `z` snooze duration |
| `long_snooze` | duration | `365d` | `Shift-Z` long-snooze duration |
| `log_path` | path | `/tmp/lazybox.log` | Where the client writes its log |
| `browser` | string | OS default | Preferred browser for `o` / terminal links. macOS: the app name for `open -a` (`"Google Chrome"`); Linux: the executable. |
| `show_tips` | bool | `true` | Show progressive feature-discovery tips (opt-out) |

Duration values take a unit suffix (`30s`, `15m`, `4h`, `365d`).

`tour_seen` and `tips_seen` also live here but are managed by lazybox; you
rarely set them by hand.

## `display`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `sort_by` | `priority` \| `updated` | `priority` | Default sort order |
| `show_archived` | bool | `false` | Include archived workspaces |
| `activity_days` | int | `7` | Only show sessions active within this many days (`0` = all) |
| `hide_approved_by_me` | bool | `true` | Hide PRs you've already approved |
| `assignee_is_reviewer` | bool | `false` | Treat assignees as reviewers |
| `show_inactive_in_inbox` | bool | `false` | Surface merged / closed PRs in the main Inbox |
| `ascii_glyphs` | bool | `false` | Use ASCII `p`/`i`/`l` instead of unicode row glyphs |

## `attention`

Which signals contribute to the "needs attention" badge on a repo header. All
default to `true`.

| Field | Type | Description |
| --- | --- | --- |
| `unread` | bool | Unread activity |
| `ci_failing` | bool | Failing CI |
| `review_pending` | bool | A review is requested of you |
| `agent_asking` | bool | An agent is waiting on input |
| `mentioned` | bool | You were mentioned |
| `desktop_notify` | bool | Fire an OS desktop notification when an agent needs input (independent of `agent_asking`) |

## `providers`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `github.poll_interval` | seconds | `60` | How often the GitHub provider polls |
| `github.detect_needs_reply` | bool | `true` | Show needs-reply badges when GitHub reports a reply is needed |
| `github.filters` | list | `[]` | Narrow which PRs appear (empty = everything) |

Each `filters` entry has exactly one of:

| Field | Description |
| --- | --- |
| `org` | PRs involving you in this org |
| `repo` | PRs involving you in `owner/name` |
| `watch` | All open PRs in `owner/name`, regardless of involvement |

## `slack`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `bot_token` | string | — | Bot token (`xoxb-…`); env `SLACK_BOT_TOKEN` wins if set |
| `app_token` | string | — | App-level token (`xapp-…`) for Socket Mode; env `SLACK_APP_TOKEN` wins |
| `anchor_channel` | string | `lazybox` | Channel the mirror anchors to |
| `channel_prefix` | string | `""` | Prefix for per-workspace channel names |
| `per_workspace_channels` | bool | `true` | Give each workspace its own channel (vs. one anchor channel with threads) |
| `allowed_users` | list of string | `[]` | Slack user ids (`U…`) allowed to drive agents from chat (empty = everyone, with a startup warning) |

See [Mirror to Slack](/docs/how-to/mirror-to-slack/) for setup.

## `hooks`

Periodic maintenance scripts lazybox runs from `hooks.dir/<bucket>/`. lazybox
never inspects what they do.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Master switch |
| `dir` | path | `~/.lazybox/hooks` | Directory with `daily/`, `hourly/`, `on_idle/` subfolders |
| `schedule.daily` | duration | `24h` | How often the `daily/` bucket runs |
| `schedule.hourly` | duration | `1h` | How often the `hourly/` bucket runs |
| `schedule.on_idle` | duration | `15m` | How long the inbox must be quiet before the `on_idle/` bucket runs |
| `script_timeout` | duration | `5m` | Max runtime per script before SIGTERM |

## `mention`

Auto-spawn settings for `@lazybox` mentions in issues / comments.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `allowed_logins` | list of string | `[]` | GitHub logins whose `@lazybox` mentions auto-spawn the default agent. Empty = just the authenticated viewer. |

## `auto_fix`

Auto-inject fix work when a PR you authored fails CI or hits a merge conflict.
Opt-in — it pushes commits to your PRs with no manual nudge.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch |
| `opt_out_labels` | list of string | `[]` | Labels (case-insensitive) that opt a PR out |
| `max_attempts` | int | `3` | Attempts per PR, per failure-kind, per `window` |
| `cooldown` | duration | `1h` | Minimum gap between attempts on the same PR (floored at 60s) |
| `window` | duration | `24h` | Rolling window the `max_attempts` budget is measured over |

## `shell`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `command` | string | `bash` | Shell launched by the `s` spawn |
