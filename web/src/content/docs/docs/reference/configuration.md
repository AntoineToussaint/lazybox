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
| [`setup`](#setup) | Wizard output: enabled providers / agents, filters, scopes, default agent |
| [`desktop`](#desktop) | Private desktop analytics opt-in |
| [`editors`](#editors) | Override / extend the detected editors |
| [`repos`](#repos) | Per-repo env, mounts, scripts, branch prefix |
| [`agent`](#agent) | Permission prompts, LLM gateway, agent state-detection timers |
| [`agents`](#agentsid) | Custom CLI definitions and per-agent model-tier overrides |
| [`worktree`](#worktree) | Global mounts, scripts, branch prefix, merged-cleanup |
| [`scan`](#scan) | Roots and depth for read-only external-checkout discovery |
| [`terminal`](#terminal) | Terminal escape chord + scrollback behavior |
| [`ui`](#ui) | View state, key remaps, keymap preset, theme, timings, browser |
| [`display`](#display) | Sort, filtering, glyphs |
| [`attention`](#attention) | Which signals raise the per-repo badge + notification delivery |
| [`providers`](#providers) | GitHub polling + filters |
| [`slack`](#slack) | Slack mirror tokens + channels |
| [`hooks`](#hooks) | Periodic maintenance scripts |
| [`mention`](#mention) | Auto-spawn on `@lazybox` mention |
| [`auto_fix`](#auto_fix) | Auto-fix PRs on CI failure / conflict |
| [`merge_on_green`](#merge_on_green) | Opt bot authors into merge-on-green |
| [`conventions`](#conventions) | Commit / PR conventions injected into the agent-work brief |
| [`shell`](#shell) | Shell command for the `s` spawn |

Snippet workflows are **not** part of `config.yaml` — they live in their own
files: `~/.lazybox/snippets.yaml` (global) and
`<launch-dir>/.lazybox/snippets.yaml` (loaded once for that client and wins on
key conflict). See
[Use snippet workflows](/docs/how-to/use-snippets/) for fast submission,
Recent/`]N` memory, Ask Lazybox hot reload, and broadcast.

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

# ── setup ────────────────────────────────────────────────────────────
# Written by the first-run wizard and the `,` Settings palette; editable
# by hand.
setup:
  providers: [github, linear]
  agents: [claude, codex, aider]
  default_agent: claude      # what `w w` spawns; unset → claude

# ── desktop privacy ──────────────────────────────────────────────────
desktop:
  analytics_enabled: false   # explicit opt-in; fixed event names only

# ── agent ────────────────────────────────────────────────────────────
agent:
  # Autonomous @lazybox work runs Claude with --dangerously-skip-permissions.
  autonomous_skip_permissions: true
  # Skip permission prompts for interactively spawned agents too.
  skip_permissions: false
  # Point every spawned agent at your own LLM gateway (injected as
  # ANTHROPIC_BASE_URL / OPENAI_BASE_URL depending on the agent).
  llm_gateway_url: "http://gateway.internal"

# ── agents (per-agent overrides) ─────────────────────────────────────
# Model-tier menu the `w S`/`w M`/`w L` and `a S`/`a M`/`a L` chords pick
# from. Claude ships a built-in Haiku/Sonnet/Opus menu; other agents
# define theirs here.
agents:
  codex:
    models:
      default: M             # tier a bare spawn uses; unset → agent default
      tiers:
        - alias: M
          label: GPT-5
          args: ["-m", "gpt-5"]
  aider:
    name: Aider
    command: aider
    args: [--model, sonnet]
    resume_args: [--resume]
    asking_patterns: ["Proceed?"]

# ── worktree ─────────────────────────────────────────────────────────
worktree:
  branch_prefix: ""          # "" → issue-42; "lazybox" → lazybox/issue-42
  auto_cleanup_merged: false # reap worktrees when their PR merges
  mounts:
    - source: ~/shared/cache
      link_at: .cache
      placement: inside

# ── checkout discovery ──────────────────────────────────────────────
# Used when `lazybox scan` receives no ROOTS on the command line.
scan:
  roots: [~/code, ~/work]
  max_depth: 4

# ── terminal ─────────────────────────────────────────────────────────
terminal:
  escape_char: "]"           # press twice to open the terminal command menu
  escape_window_ms: 600      # window between the two presses
  agent_dead_on_arrival_ms: 10000 # preserve fast/failed exits for inspection

# ── shell ────────────────────────────────────────────────────────────
shell:
  # Unset/empty → OS login shell, then $SHELL, then /bin/sh.
  command: /bin/zsh

# ── ui ───────────────────────────────────────────────────────────────
ui:
  keymap_preset: default     # base keymap layer: default | vim
  theme: Lazybox Dark        # written back by the `t` theme picker
  terminal_new_layout: split # ordinary new terminals: split | tabs (`]]t` toggles)
  activity_pane_default: full # right pane start mode: full | summary | hidden (`Shift-P` cycles)
  confirm_default:           # which Confirm button Enter highlights, by source
    destructive_shortcut: yes # a destructive chord (x x, g m, …): the chord is the intent
    event: no                # an unsolicited prompt (merged-PR removal): don't destroy on a stray Enter
  # Remap any catalog action. Keys are snake_case action ids; values are
  # key-spec strings. Unset actions keep their default binding.
  action_keys:
    merge_pr: Ctrl-m
    refresh: Ctrl-r
    spawn_agent.claude: c    # restore a top-level Claude spawn key
    spawn_agent.aider: "a z" # custom agents choose their own chord
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
| map of string → string | Environment variables injected into every shell and agent PTY in that repo's worktrees. Layered over the daemon env; a per-repo value wins over the global LLM-gateway injection (`agent.llm_gateway_url`) and the agent's own spawn defaults. |

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

## `setup`

Written by the first-run wizard and the `,` Settings palette; safe to edit by
hand.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `providers` | list of string | `[]` | Provider ids currently enabled (`github`, `linear`) |
| `agents` | list of string | `[]` | Agent ids currently enabled (`claude`, `codex`, …) |
| `filters` | map | `{}` | Per-provider role/type filter keys (e.g. `github: [pr.author, pr.reviewer]`) |
| `scopes` | map | `{}` | Per-provider scope ids (orgs / repos); empty = all |
| `default_agent` | string | unset | Agent the `w w` work shortcut spawns; unset falls back to `claude` |
| `wizard_completed` | bool | `false` | Set once the wizard finishes, so an all-empty block doesn't re-trigger it |

## `desktop`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `analytics_enabled` | bool | `false` | Record the desktop client's fixed, content-free event names and timestamps locally. Provider and terminal contents cannot enter this boundary. |

## `agent`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `autonomous_skip_permissions` | bool | `true` | Autonomous `@lazybox` work runs Claude with `--dangerously-skip-permissions` (blast radius bounded to the worktree) |
| `skip_permissions` | bool | `false` | Skip permission prompts for interactively spawned agents too |
| `llm_gateway_url` | string | unset | Global LLM-gateway base URL. When set, every spawned agent gets it injected as the base-URL env var its CLI reads (`ANTHROPIC_BASE_URL` for Claude, `OPENAI_BASE_URL` for Codex / Cursor). A per-repo `env` entry for the same var wins. Auth keys are deliberately not managed here. |
| `working_watchdog_secs` | int | `15` | Fail-safe window: seconds a `Working` agent terminal may sit with no meaningful screen change before the daemon classifies the screen and forces the turn out of `Working`. `0` disables the watchdog. |
| `quiet_classify_secs` | int | `5` | Quiet-timer window: seconds of PTY silence before a `Working` turn settles to `Done`. Cannot be disabled (`0` falls back to 5); raise it to be less eager to call a turn finished. |

The legacy `name`, `command`, `args`, `resume_args`, and `asking_patterns`
fields under `agent` remain accepted so old files parse, but new custom agents
belong under [`agents.<id>`](#agentsid).

## `agents.<id>`

Per-agent definitions and overrides keyed by agent id (`claude`, `codex`, …).
An entry with `command` registers a generic agent CLI at daemon startup. Add
that id to `setup.agents`, then give it a chord through
`ui.action_keys.spawn_agent.<id>`. Entries without `command` simply customize
a built-in. Claude ships a Haiku (`S`) / Sonnet (`M`) / Opus (`L`) model menu;
other agents have no built-in menu.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | agent id | Display name for a custom CLI |
| `command` | string | unset | Executable for a custom CLI; its entry is registered only when this is set |
| `args` | list of string | `[]` | Arguments appended to `command` for a fresh session |
| `resume_args` | list of string | unset | Arguments appended to `command` for resume; unset reuses `args` |
| `asking_patterns` | list of string | `[]` | Output markers that classify the custom agent as **Input Needed** |
| `models.default` | string | unset | Alias of the tier a bare spawn uses; unset → the agent's own default model |
| `models.tiers` | list | `[]` | Ordered tier menu. Each entry: `alias` (the chord key — a single uppercase letter binds as `Shift`, e.g. `S` → `w S`), `label` (shown in the popup and the `◆` tab badge), `args` (appended to the spawn argv) |
| `models.priority` | map | `{}` | `high` / `medium` / `low` → tier alias, used when a spawn declares no explicit tier but the task carries a priority |
| `auto_update` | bool | `false` | Let lazybox apply this agent's CLI updates automatically when the scheduled out-of-band check finds a newer version. Off by default: the check still runs and surfaces "update available", but installing waits for the manual "update agent CLIs" action. |

## `worktree`

Global worktree layout — applied to every checkout, with `repos.<owner/name>`
overrides stacked on top.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `mounts` | list | `[]` | Symlinks to add to every worktree (same fields as [`repos.mounts`](#mounts)) |
| `scripts` | list | `[]` | Executables to materialize in every worktree (same fields as [`repos.scripts`](#scripts)) |
| `auto_cleanup_merged` | bool | `false` | When a tracked PR merges, reap the worktrees backing its sessions — only the ones with no locked / uncommitted / unpushed work and no live terminal |
| `branch_prefix` | string | `""` | Prefix for branches lazybox cuts itself (issues, blank workspaces). `""` → `issue-42`; `"lazybox"` → `lazybox/issue-42`. Overridable per repo. |

## `scan`

Defaults for the read-only [`lazybox scan`](/docs/reference/cli/#lazybox-scan)
checkout inventory. Positional roots and `--depth` override `scan.roots` and
`scan.max_depth` for a single invocation.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `roots` | list of paths | `[]` | Directories to walk when the command receives no roots; leading `~/` is expanded |
| `max_depth` | int | `4` | Maximum directory levels to descend below each root |

## `terminal`

How you exit an embedded terminal back to the inbox, and how scrollback works.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `escape_char` | char | `]` | Press twice to open the non-timed terminal command menu (`q` then exits to the sidebar) |
| `escape_window_ms` | int | `600` | Time window between the two `escape_char` presses |
| `agent_dead_on_arrival_ms` | int | `10000` | Grace period in milliseconds: an agent that exits cleanly before engaging is treated as failed-to-start and its final screen stays open with a restart affordance. Non-zero/signal exits are always preserved. |

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
    merge_pr: Ctrl-m       # was g m
    refresh: Ctrl-r        # was Shift-R
    quit: Ctrl-q           # single press (default is the q q chord)
    spawn_agent.claude: c  # restore a top-level Claude spawn key (default a c)
    spawn_agent.aider: "a z"
```

Agent spawn chords are remapped with `spawn_agent.<agent-id>` keys (the
built-in defaults are `a c` Claude, `a x` Codex, `a u` Cursor; custom agents
have no implicit chord).
Alternatives are separated by `|` (`"g r | Shift-V"`).

**Key-spec format:**

- Modifiers `Ctrl-`, `Shift-`, `Alt-`, stackable in that order
  (`Ctrl-Shift-D`).
- A single character (`m`, `,`). An uppercase letter implies Shift, so `M` and
  `Shift-M` are equivalent.
- Named keys: `Tab`, `Enter`, `Esc`, `Space`, `Backspace`, `Up`, `Down`,
  `Left`, `Right`, `Home`, `End`, `PageUp`/`PgUp`, `PageDown`/`PgDn`,
  `Delete`/`Del`, `Insert`.
- Function keys `F1`–`F12` (`F5`; `F8` is a shipped default — it toggles mouse
  capture).
- Multi-key sequences separate the strokes with a space: a leader chord
  (`g m`), a double press (`q q`).

**Common action ids** (the full set is the snake_case names of every catalog
action in
[`crates/tui-core/src/action.rs`](https://github.com/AntoineToussaint/lazybox/blob/main/crates/tui-core/src/action.rs)):

| Action id | Default | Does |
| --- | --- | --- |
| `refresh` | `Shift-R` | Re-poll every provider |
| `open_help` | `?` | Ask Lazybox |
| `open_settings` | `,` | Settings palette |
| `quit` | `q q` | Quit |
| `work` | `w w` | Spawn the default/running agent with a contextual prompt |
| `spawn_shell` | `s` | Open a shell in the worktree |
| `open_editor` | `e` | Open the worktree in your editor |
| `mark_all_read` | `m` | Mark the workspace read |
| `toggle_snooze` | `z` | Snooze (~4h) |
| `merge_pr` | `g m` | Merge the PR |
| `toggle_auto_merge` | `g g` | Toggle lazybox's merge-on-green arm |
| `manage_policies` | `g p` | Open the unified automation-policy menu |
| `request_reviewers` | `g r` | Request reviewers |
| `add_assignees` | `g a` | Change assignees |
| `manage_labels` | `g l` | Edit labels |
| `open_in_browser` | `g o` | Open the PR / issue in the browser |
| `archive` | `x x` | Archive the workspace |
| `new_workspace` | `x n` | New pre-PR workspace |
| `new_project` | `x p` | New project / pick a repo |
| `adopt_sessions` | `x a` | Move sessions into another workspace |
| `collapse_into_pr` | `x j` | Join an issue workspace into its closing PR |
| `long_snooze` | `x z` | Snooze the workspace for about a year |
| `close_issue` | `x c` | Close an issue upstream |

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
| `terminal_escape_char` | char | unset | Legacy alias for `terminal.escape_char`; when set, this compatibility value wins |
| `split_step_percent` | int | `3` | Percent a `Shift-arrow` nudges the focused splitter |
| `task_body_max_rows` | int | `8` | Max rows the description section expands to |
| `short_snooze` | duration | `4h` | `z` snooze duration |
| `long_snooze` | duration | `365d` | `x z` long-snooze duration |
| `log_path` | path | `/tmp/lazybox.log` | Where the client writes its log |
| `browser` | string | OS default | Preferred browser for `g o` / terminal links. macOS: the app name for `open -a` (`"Google Chrome"`); Linux: the executable. |
| `keep_awake` | bool | `false` | Hold an OS sleep inhibitor (`caffeinate` on macOS, `systemd-inhibit` on Linux) for exactly as long as at least one agent terminal is actively `Working`; released the moment everything goes idle. Re-read on every agent transition — no restart needed. |
| `keymap_preset` | `default` \| `vim` | unset | Base keymap layer shipped in-tree; your `action_keys` still layer on top (`vim` moves pane-cycling to `Ctrl-w`) |
| `theme` | string | unset | Active UI theme by exact name (`"Lazybox Dark"`, `"Lazybox Light"`, `"High Contrast"`, …). Written back by the `t` theme picker (live preview; `Esc` restores); unknown / unset keeps the default theme. Full theme list: [docs/themes.md](https://github.com/AntoineToussaint/lazybox/blob/main/docs/themes.md). |
| `show_tips` | bool | `true` | Show progressive feature-discovery tips (opt-out) |
| `terminal_new_layout` | `split` \| `tabs` | `split` | How an ordinary second terminal opens. Explicit `]]\|` / `]]-` splits are unaffected; `]]t` toggles and persists this value. |
| `activity_pane_default` | `full` \| `summary` \| `hidden` | `full` | Where the right (activity) pane starts for a workspace you haven't toggled. `summary` shows a one-line count of new activity / failing CI; `hidden` folds it away. `Shift-P` cycles the three per workspace; a workspace with nothing to show still auto-hides. |
| `confirm_default.destructive_shortcut` | `yes` \| `no` | `yes` | Which button `Enter` highlights on a Confirm modal raised by a destructive chord (`x x` archive, `g m` merge, …). The chord is the intent, so `Enter` confirms; set `no` to require an explicit arrow-then-Enter. |
| `confirm_default.event` | `yes` \| `no` | `no` | Which button `Enter` highlights on a Confirm modal raised unsolicited by a provider event (a merged-PR "remove this workspace?"). Defaults to `no` so a stray `Enter` can't destroy a workspace you didn't ask about. |

Duration values take a unit suffix (`30s`, `15m`, `4h`, `365d`).

`tour_seen` and `tips_seen` (the list of tip ids already shown, so a tip never
repeats) also live here but are managed by lazybox; you rarely set them by
hand.

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

Which signals contribute to the "needs attention" badge on a repo header. The
booleans all default to `true`.

| Field | Type | Description |
| --- | --- | --- |
| `unread` | bool | Unread activity |
| `ci_failing` | bool | Failing CI |
| `review_pending` | bool | A review is requested of you |
| `agent_asking` | bool | An agent is waiting on input |
| `mentioned` | bool | You were mentioned |
| `desktop_notify` | bool | Fire OS desktop notifications when an agent needs input or finishes, and when a workspace gains an enabled attention signal |
| `notifier` | `auto` \| `osc` \| `subprocess` | How the desktop banner is delivered. `auto` (default) picks per environment — subprocess helpers (`terminal-notifier` / `osascript` on macOS, `notify-send` on Linux) locally, the terminal's OSC escape sequence over SSH; `osc` / `subprocess` force one path. |

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
| `allowed_logins` | list of string | `[]` | GitHub logins whose `@lazybox [agent] [model-alias]` directives can auto-spawn an agent. Empty = just the authenticated viewer. |

## `auto_fix`

Auto-inject fix work when a PR you authored fails CI or hits a merge conflict.
Opt-in — it pushes commits to your PRs with no manual nudge.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | Master switch |
| `opt_out_labels` | list of string | `["no-auto-fix", "do-not-lazybox"]` | Labels (case-insensitive) that opt a PR out |
| `max_attempts` | int | `3` | Attempts per PR, per failure-kind, per `window` |
| `cooldown` | duration | `1h` | Minimum gap between attempts on the same PR (floored at 60s) |
| `window` | duration | `24h` | Rolling window the `max_attempts` budget is measured over |

## `merge_on_green`

Tuning for lazybox's "merge on green" arm (`g g`). By default the daemon only
auto-merges PRs **you** authored; this opts specific other authors in so their
green PRs land automatically once armed. The canonical use is a green Dependabot
bump: add its login, arm merge-on-green on the PR, and lazybox merges it the
moment CI passes. Logins match case-insensitively and a trailing `[bot]` is
ignored, so `dependabot` covers `dependabot[bot]` too. Arming merge-on-green on
a third party's PR that isn't listed here is refused with a reason rather than
lighting a pill that never fires.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `allow_authors` | list of string | `[]` | Non-author logins whose green PRs may auto-merge. Empty keeps the safe own-PRs-only behavior. |

## `conventions`

Commit / PR naming conventions injected into the agent-work brief. Defaults
reproduce the built-in guidance (Conventional Commits, a `Closes #N.` line that
collapses an issue and its PR into one row), so an unset block changes nothing.
Honored on autonomous spawns (`@lazybox` mentions, `lazybox:` labels, auto-fix)
and on the interactive `w` work command.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `commit_style` | `conventional` \| `none` \| `custom` | `conventional` | Commit-message / PR-title-prefix style. `conventional` = [Conventional Commits](https://www.conventionalcommits.org/); `none` = no convention; `custom` = use `custom_instruction`. An unknown value falls back to `conventional`. |
| `custom_instruction` | string | _(unset)_ | House style injected verbatim when `commit_style: custom`. A blank value falls back to the default guidance. |
| `include_closes` | bool | `true` | Keep the `Closes #N.` body line that collapses an issue and its PR. Set `false` to have the brief tell the agent NOT to add it (repos that close issues manually). |

## `shell`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `command` | string | OS login shell | Shell launched by the `s` spawn. When unset or empty, lazybox reads the account's login shell from the OS passwd database, then falls back to `$SHELL` and `/bin/sh`. |

The Settings window shows the effective command and whether it is automatic or
configured. Changes apply to newly opened shells; an existing tmux-backed shell
keeps running its current process, so close that terminal and open a fresh one
to pick up the new command. Older generated configs may contain
`command: bash`; lazybox treats that former default as automatic. Use an
explicit path such as `/bin/bash` to select Bash intentionally.
