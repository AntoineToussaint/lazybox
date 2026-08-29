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
| [`open_with`](#open_with) | Config-driven "Open with…" apps launched on a workspace (`x o`) |
| [`repos`](#repos) | Per-repo env, mounts, scripts, branch prefix |
| [`agent`](#agent) | Permission prompts, LLM gateway, agent state-detection timers |
| [`agents`](#agentsid) | Custom CLI definitions and per-agent model-tier overrides |
| [`server`](#server) | Daemon-level tuning: ring buffer, credential cache, polling backoff |
| [`worktree`](#worktree) | Global mounts, scripts, branch prefix, merged-cleanup |
| [`scan`](#scan) | Roots and depth for read-only external-checkout discovery |
| [`terminal`](#terminal) | Terminal escape chord + scrollback behavior |
| [`ui`](#ui) | View state, key remaps, keymap preset, theme, timings, browser |
| [`display`](#display) | Sort, filtering, glyphs |
| [`attention`](#attention) | Which signals raise the per-repo badge + notification delivery |
| [`providers`](#providers) | GitHub polling + filters, Linear teams / branch templates / cadence |
| [`slack`](#slack) | Slack mirror tokens + channels |
| [`hooks`](#hooks) | Periodic maintenance scripts |
| [`mention`](#mention) | Auto-spawn on `@lazybox` mention |
| [`auto_fix`](#auto_fix) | Auto-fix PRs on CI failure / conflict |
| [`merge_on_green`](#merge_on_green) | Opt bot authors into merge-on-green |
| [`conventions`](#conventions) | Commit / PR conventions injected into the agent-work brief |
| [`shell`](#shell) | Shell command for the `s` spawn |
| [`sandbox`](#sandbox) | Remote dev-box lifecycle for `lazybox sandbox …` and the `r`-spawn |
| [`remote`](#remote) | Client-side `--connect` port-forward supervisor (`remote.tunnel`) |
| [`account`](#account) | Cached non-secret platform organization, plan, and entitlement association |

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
  # Leave unset to let the trigger decide: on for your own work, off for a
  # mention/label from someone other than you. Set true/false to pin it.
  # autonomous_skip_permissions: true
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

## `open_with`

A list of arbitrary apps launched on the focused workspace through the
"Open with…" picker (`x o`), decoupled from the single `editors:` code
editor. Empty by default.

| Field | Type | Description |
| --- | --- | --- |
| `name` | string | Name shown in the picker |
| `command` | string | Executable to run (`open` on macOS hands off to Launch Services) |
| `args` | list of string | Arguments; `{path}` (worktree), `{url}` (PR/issue), `{branch}`, `{repo}` expand at launch. Defaults to `["{path}"]` |
| `key` | string | Optional favorite key that binds this app to a direct chord, skipping the picker (remappable via `ui.action_keys.open_with_app.<name>`) |

Apps whose token the workspace can't supply (a `{url}` app with no PR) are
hidden from the picker; a `{path}` app on a workspace with no worktree yet
provisions one first, and on a remote (`--connect`) daemon declines with a
pointer to `s` (the worktree is server-side). See
[Workspaces & worktrees](/docs/features/workspaces-and-worktrees/).

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

## `server`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `ring_buffer_bytes` | int | `2097152` | Per-terminal ring buffer capacity in bytes for scrollback history replay on reconnect. Range: 64 KiB – 100 MiB. Raise for sessions with very long outputs; lower to reduce per-terminal memory usage. |
| `cred_cache_ttl_secs` | int | `300` | Credential cache TTL in seconds. Command-provider credentials (e.g., `gh auth token`) are cached for this duration before running the command again, reducing subprocess churn. Default is 5 minutes. |
| `polling_backoff_cap_secs` | int | `120` | Ceiling in seconds for provider polling's exponential backoff on transient errors (timeouts, rate limits, 5xx). The backoff is clamped here so a persistently failing provider retries no slower than this. |

## `agent`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `autonomous_skip_permissions` | bool | unset | Whether autonomous `@lazybox` work runs Claude with `--dangerously-skip-permissions`. **Unset** resolves per spawn from *who* triggered it: `true` for your own work (`w`, your own mentions/labels), but `false` for a spawn a foreign actor triggered — a mention from someone other than you, or a `lazybox:` label on an issue you didn't author — so their attacker-influenceable issue text can't drive an unattended skip-permissions agent on your host. Set `true`/`false` to pin it either way. |
| `skip_permissions` | bool | `false` | Skip permission prompts for interactively spawned agents too |
| `llm_gateway_url` | string | unset | Global LLM-gateway base URL. When set, every spawned agent gets it injected as the base-URL env var its CLI reads (`ANTHROPIC_BASE_URL` for Claude, `OPENAI_BASE_URL` for Codex / Cursor). A per-repo `env` entry for the same var wins. Auth keys are deliberately not managed here. |
| `working_watchdog_secs` | int | `15` | Fail-safe window: seconds a `Working` agent terminal may sit with no meaningful screen change before the daemon classifies the screen and forces the turn out of `Working`. `0` disables the watchdog. |
| `quiet_classify_secs` | int | `5` | Quiet-timer window: seconds of PTY silence before a `Working` turn settles to `Done`. Cannot be disabled (`0` falls back to 5); raise it to be less eager to call a turn finished. |
| `metering_proxy` | bool | `false` | Route every spawned agent's LLM traffic through lazybox's local metering proxy — the real data source behind the header's usage summary. The proxy forwards each request to the true upstream (or `llm_gateway_url` when set) and reads token counts off the response, so both Claude and Codex (and interactive terminal sessions) report real per-provider quota. Opt-in: it inserts a loopback hop in front of every agent API call. |
| `max_live_agents` | int | `32` | Advisory ceiling on concurrently live agent terminals across all workspaces. Over the cap, spawns and startup recovery **warn** (a footer notice naming `]]x`) but are never refused — lazybox advises, it does not forbid. `0` disables the warnings. |
| `nice` | int | `10` | Scheduling niceness for spawned agent processes and their children, so a large fleet yields under contention and never starves the interactive UI (liveness over throughput). `0` disables (agents run at normal priority). Clamped to `0..=20`. |
| `strict_mcp` | bool | `false` | Launch unattended (skip-permissions) Claude spawns with `--strict-mcp-config`, disabling every ambient MCP server you configured. Default `false`: autonomous agents inherit your normal MCP setup. Read-only reviewer spawns stay strict regardless. |
| `reap_closed_after` | duration | `48h` | How long after a workspace's PR/issue merges or closes its persistent sessions may keep running before the daemon reaps them (an idle agent is a ~110 MB memory ratchet). Reaped sessions stop being restored at startup; `w w` respawns one fresh and prompt history persists. `0s` disables reaping entirely. |

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
| `pinned_repos` | list of string | `[]` | Repo names pinned to the top of the sidebar, in pin order (`p` toggles). A list, not a set — the order you pinned in is the display order. Written back automatically. |
| `focused_workspaces` | list of string | `[]` | Workspace keys you've starred ("focused"), in focus order — lifted into a synthetic `★ Focused` section at the top and numbered for `]]<digit>` focus jumps. Written back automatically. |
| `spaces` | list | `[]` | User-defined Spaces — the grouping tier above repo headers (`x m` moves a source into one). Each entry names a bucket and lists its assigned source labels; the list position is the display order. Written back automatically. |
| `collapsed_spaces` | list of string | `[]` | Space names whose groups start collapsed (`Space` on a Space header). Written back automatically. |
| `last_space` | string | _(unset)_ | Space most recently assigned via `x m`, preselected in the move-to-Space picker. Written back automatically. |
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
| `focus_layout` | `single` \| `split-v` \| `split-h` \| `grid` | `single` | Focus-mode multi-workspace layout: `single` (fullscreen terminal), two side by side, two stacked, or a 2×2 grid over the starred roster. `]]v` cycles it inside focus mode and persists back here. |
| `usage_summary` | bool | `true` | Show the always-visible per-provider usage summary in the sidebar header (a compact `Claude ▓▓▓░░ 62% · 76k left` widget per agent with a live terminal). Set `false` to hide the row. |
| `usage_budgets` | map of string → int | `{}` | Plan-window token budget per agent id (`claude: 200000000`), the denominator for the usage summary's percentage. OAuth/plan agents expose no usage API, so set the window size here to unlock the `62% · 76k left` bar; without one an agent degrades to a bare token total. |
| `today_summary` | bool | `true` | Show the always-visible "today" stats strip in the sidebar header: a terse `today  3 sessions · 4 merged · $2.14` of the day's persisted usage rollup. Groups drop lowest-first (cost, then merged) when the sidebar is narrow. Set `false` to hide the row. |
| `usage_limit_alerts` | bool | `true` | Raise an escalating footer alert as agents hit their provider usage limit (transient notice → sticky banner naming the resume action → retracted once they recover). The `⏳ N limited` header count and per-row pill are always shown; this only gates the footer escalation. Set `false` to keep the passive signals only. |
| `auto_wait_on_limit` | bool | `false` | Auto-press "Wait" when a Claude agent hits its usage / monthly limit, so N agents hitting the cap at once don't each need a manual visit — re-auth with another account and then `Shift-K` to resume them. Re-read on every transition. |
| `show_agent_model` | bool | `true` | Show each running agent's model + reasoning effort next to its sidebar badge (`C Opus`, `X gpt-5.5 · xhigh`) and on its terminal tab. Set `false` to keep the sidebar compact. |
| `credit_recovery_prompt` | string | built-in | Prompt submitted after a credit chooser has cleared and the provider composer is ready (the `Ctrl-k` recover-credit flow). |
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

### `providers.github`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `poll_interval` | seconds | `60` | How often the GitHub provider polls |
| `detect_needs_reply` | bool | `true` | Show needs-reply badges when GitHub reports a reply is needed |
| `filters` | list | `[]` | Narrow which PRs appear (empty = everything) |
| `background_budget_share` | float | `0.55` | Maximum share of each observed GitHub primary rate-limit budget that scheduled polling may consume; the remainder stays available to interactive `gh`, agents, and bursts. |
| `include_accessible_repos` | bool | `false` | Widen the inbox scope to every repo you can reach — owned, org-member, and direct-collaborator — not just the scopes you ticked in setup. Involved PRs/issues in any of those surface without a manual tick; repos you can't access stay hidden. |

Each `filters` entry has exactly one of:

| Field | Description |
| --- | --- |
| `org` | PRs involving you in this org |
| `repo` | PRs involving you in `owner/name` |
| `watch` | All open PRs in `owner/name`, regardless of involvement |

### `providers.linear`

Linear issue polling and the branch/checkout wiring for working on Linear
tickets. A ticket's own `repo` is the synthetic `linear/<team>`; the `teams` map
routes each team to a clonable GitHub repo.

```yaml
providers:
  linear:
    scope: [assigned, created, subscribed]
    handle: antoine
    teams:
      OBI: obin-ai/obin-platform
    branch_template: "{handle}/{type}-{id}-{slug}"
    label_types:
      Bug: fix
      Feature: feat
    poll_interval_secs: 60
    idle_poll_interval_secs: 300
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `scope` | list | `[assigned, created]` | Which issues the poller requests, as `or` clauses: any subset of `assigned` / `created` / `subscribed`. `subscribed` is opt-in because Linear auto-subscribes you to entire teams. An unrecognized entry warns and is dropped; an empty list falls back to the default. |
| `handle` | string | git `user.name` | Personal git handle for the `{handle}` branch token. |
| `teams` | map of string → string | `{}` | Linear team key → GitHub `owner/repo` to clone when working on that team's issues. An unmapped team fails loudly rather than cloning the synthetic `linear/<team>`. |
| `branch_template` | string | _(unset)_ | Branch-name template. Tokens: `{handle}`, `{type}`, `{id}`, `{slug}`; empty tokens and their orphaned separators collapse. Unset → the generic `linear-<id>-<slug>`. |
| `label_types` | map of string → string | `{}` | Linear label name → commit-type token (`feat` / `fix` / `chore` / …) for the `{type}` branch token. Multiple matches resolve by a fixed precedence (`fix` > `feat` > `chore` > `docs`). |
| `poll_interval_secs` | int | `60` | Base cadence for the Linear poll while tickets are actively changing (decoupled from GitHub's hot loop — Linear changes less often). |
| `idle_poll_interval_secs` | int | `300` | Cadence the poll backs off to once several consecutive sweeps returned an unchanged ticket set. Clamped up to at least `poll_interval_secs`. |

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

Adding **any login other than your own** is a trust decision: that person can
turn an issue comment into an autonomous agent run — built from
attacker-influenceable issue text — with full `git`/`gh` and your machine's
credentials. A mention from a non-viewer login therefore keeps its permission
prompts on ([`agent.autonomous_skip_permissions`](#agent) resolves to `false`)
unless you set that flag explicitly. See [Trigger agents with @lazybox mentions](/docs/how-to/lazybox-mentions/#allow-other-people-to-trigger-mentions).

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

## `account`

Cached, non-secret association written by `lazybox account claim`. An empty
block means the box is unlinked; claim codes and private keys are never stored.

```yaml
account:
  platform_url: https://platform.lazybox.ai
  organization_id: org_42
  organization_name: Example
  device_id: dev_7
  plan: pro
  entitlement_active: true
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `platform_url` | string | _(unset)_ | Platform that accepted the claim. |
| `organization_id` | string | _(unset)_ | Stable linked organization id. |
| `organization_name` | string | _(unset)_ | Optional display name returned by the platform. |
| `device_id` | string | _(unset)_ | Platform device record for this box. |
| `plan` | string | _(unset)_ | Plan cached from the successful claim. |
| `entitlement_active` | bool | _(unset)_ | Whether the claim response reported an active entitlement. |
| `entitlement_reason` | string | _(unset)_ | Optional inactive/unknown explanation. |

## `remote`

Client-side remote-access wiring for `lazybox --connect`. The block is omitted
from a written config when unset. Its one sub-block, `remote.tunnel`, replaces
the operator-run `autossh` of the bring-your-own-remote runbook: when set,
`--connect` spawns and keepalive-supervises an SSH (or IAP-tunneled SSH) forward
that carries the daemon socket and any workload ports before it dials.

To provision and drive the box itself (not just forward to one you already run),
use the higher-level [`sandbox`](#sandbox) block, which owns the box lifecycle
and the `r`-spawn.

### `remote.tunnel`

```yaml
remote:
  tunnel:
    mode: ssh              # ssh | iap
    host: me@box           # ssh destination (mode: ssh)
    remote_socket: /home/me/.lazybox/run/daemon.sock
    ports: [3000, 8082]    # workload TCP ports, localhost→localhost
```

The forward carries the daemon Unix socket (`--connect`'s endpoint) plus any
workload TCP ports, all bound to `localhost` on the client, supervised with
capped-backoff keepalive so a dropped link is re-established without operator
intervention.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `mode` | `ssh` \| `iap` | `ssh` | Transport: a direct `ssh -N -L …` (`ssh`), or SSH-over-GCP-Identity-Aware-Proxy for a box with no public IP (`iap`). |
| `host` | string | _(unset)_ | SSH destination (`user@host` or a `~/.ssh/config` alias). Required for `mode: ssh`. |
| `instance` | string | _(unset)_ | GCE instance name. Required for `mode: iap`. |
| `user` | string | _(unset)_ | Login user for `mode: iap` (the instance's default OS-Login user otherwise). |
| `zone` | string | _(unset)_ | GCE zone for `mode: iap` (falls back to gcloud's active zone). |
| `project` | string | _(unset)_ | GCP project for `mode: iap` (falls back to gcloud's active project). |
| `remote_socket` | string | _(unset)_ | Absolute path of the daemon socket on the box, forwarded to the local `--connect` socket. Unset → only the workload ports are forwarded. sshd does not expand `~`, so give an absolute path. |
| `local_socket` | path | `--connect` path | Local socket the forward binds. |
| `ports` | list of int | `[]` | Workload TCP ports forwarded `localhost:<p>` → `localhost:<p>` on the box (e.g. `3000`, `8082`). |
| `server_alive_interval` | int | `15` | SSH `ServerAliveInterval` — seconds of link idle before a keepalive probe. |
| `server_alive_count_max` | int | `3` | How many missed probes before the link is torn down and re-established. |

## `sandbox`

Remote dev-box lifecycle wiring, read by `lazybox sandbox <ensure|wake|sleep|
status|connect|rebuild|destroy>` and by the sidebar `r`-spawn (which brings the
box up lazily on demand). Names the provider placement/template, the deployment
overlay, and the socket the connect forward carries. Every field is optional, so
the block round-trips out of a written config when unset, and each command's
flags override what is set here. Omitted from a written config when empty.

```yaml
sandbox:
  provider: gcp
  project: my-proj
  region: us-central1
  zone: us-central1-a
  terraform_dir: ./terraform/sandbox/gcp
  deployment: ./.lazybox/sandbox.yaml
  remote_socket: /home/me/.lazybox/run/daemon.sock
  ports: [3000, 8082, 8787]
```

E2B uses `template` and `timeout_seconds` instead of the GCP placement fields:

```yaml
sandbox:
  provider: e2b
  template: lazybox-e2b
  timeout_seconds: 3600
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `provider` | `gcp` \| `e2b` | _(unset)_ | Provider id. |
| `project` | string | _(unset)_ | GCP project. |
| `region` | string | _(unset)_ | GCP region. |
| `zone` | string | _(unset)_ | GCP zone (e.g. `us-central1-a`). |
| `template` | string | _(unset)_ | E2B template id or alias. |
| `timeout_seconds` | int | _(unset)_ | E2B running timeout; on expiry E2B performs a full-memory auto-pause. |
| `terraform_dir` | path | `terraform/sandbox/gcp` | Terraform module `ensure` / `destroy` run against. |
| `deployment` | path | _(unset)_ | Deployment overlay YAML deep-merged onto the embedded default; unset → the generic default recipe. |
| `user` | string | _(unset)_ | SSH / gcloud login user for the IAP connect. |
| `remote_socket` | string | _(unset)_ | Absolute daemon-socket path on the box the connect forward carries. |
| `local_socket` | path | `--connect` path | Local socket the forward binds. |
| `ports` | list of int | `[]` | Workload TCP ports the connect forward carries. |
| `install_lazybox` | bool | `true` | Whether `ensure` provisions a box that builds + runs the lazybox daemon on boot. Set `false` for a bring-your-own-stack deployment that manages its own. |
| `auto_connect` | bool | `false` | Connect to the box in the background at launch. Off by default: nothing touches the billed box until you connect (with `Shift-C`, or lazily on the first `r`-spawn). Governs only startup, not on-demand spawns. |
| `require_connect` | bool | `false` | Hard-gate a remote (`r`-)spawn on an already-live connection. Off by default: an `r`-spawn while disconnected lazily brings the box up. Set `true` to refuse a spawn while disconnected and point at `Shift-C`. |

### `sandbox.auth`

How the provider authenticates to the cloud, so the box lifecycle runs off
configured credentials rather than whatever ambient `gcloud auth login` / ADC
the machine happens to have. Credentials are injected explicitly into every
provider call and scoped to a lazybox-owned gcloud config, so your own `gcloud`
is never touched. An empty block means ambient credentials.

```yaml
sandbox:
  auth:
    service_account_key: ~/.lazybox/gcp-sa.json
    impersonate_service_account: deploy@my-proj.iam.gserviceaccount.com
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `service_account_key` | path | _(unset)_ | Service-account key (or any `GOOGLE_APPLICATION_CREDENTIALS`-compatible credential file). The headless / CI path. |
| `impersonate_service_account` | string | _(unset)_ | Service account to impersonate; the base credentials (a `service_account_key`, else ambient) mint tokens for it. |
| `config_dir` | path | lazybox-owned dir | Override the provider-scoped `CLOUDSDK_CONFIG` directory. |

See [Remote over SSH](/docs/how-to/remote-over-ssh/) for the end-to-end setup.
