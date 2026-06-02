# Configuration reference

pilot reads `~/.pilot/config.yaml`. Every key is optional — pilot runs with an
empty config by auto-detecting agents and editors and reading GitHub credentials
from `gh auth token`. The sections below document each top-level key.

`PILOT_HOME` overrides the base directory pilot writes to, which moves the
config, state, and worktree paths accordingly.

## Annotated example

```yaml
# ── editors ──────────────────────────────────────────────────────────
# Overrides/extends the editors pilot auto-detects (Zed, VS Code, Cursor,
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
      - source: /Users/me/widgets-secrets
        link_at: .secrets
        placement: above
    # Materialize executables at <worktree>/_pilot/scripts/<name>.
    scripts:
      - name: seed-db
        content: |
          #!/usr/bin/env bash
          set -euo pipefail
          psql "$DATABASE_URL" -f db/seed.sql
      - name: lint
        source: /Users/me/widgets-shared/lint.sh

# ── slack ────────────────────────────────────────────────────────────
slack:
  bot_token: xoxb-your-bot-token
  app_token: xapp-your-app-token
  anchor_channel: pilot-inbox
  per_workspace_channels: true

# ── agent ────────────────────────────────────────────────────────────
agent:
  # Autonomous @pilot work runs Claude with --dangerously-skip-permissions.
  autonomous_skip_permissions: true
  # Skip permission prompts for interactively spawned agents too.
  skip_permissions: false

# ── providers ────────────────────────────────────────────────────────
providers:
  github:
    poll_interval: 60        # seconds

# ── ui ───────────────────────────────────────────────────────────────
ui:
  log_path: /tmp/pilot.log
```

## `editors`

A list of editor definitions that override or extend the detected set
(Zed, VS Code, Cursor, Windsurf, Fleet, IDEA, Gram).

| Field | Type | Description |
| --- | --- | --- |
| `id` | string | Stable identifier |
| `display` | string | Name shown in the picker |
| `command` | string | Executable to run |
| `args` | list of string | Arguments; `{path}` expands to the worktree directory |

## `repos`

A map keyed by `owner/name`. Each entry accepts:

### `env`

| Type | Description |
| --- | --- |
| map of string → string | Environment variables injected into every shell and agent PTY in that repo's worktrees |

### `mounts`

A list of symlink definitions:

| Field | Type | Description |
| --- | --- | --- |
| `source` | path | Existing directory to link |
| `link_at` | path | Where the symlink is created (relative) |
| `placement` | `inside` \| `above` | `inside` the worktree, or `above` it (parent) |

### `scripts`

A list of executables materialized at `<worktree>/_pilot/scripts/<name>`:

| Field | Type | Description |
| --- | --- | --- |
| `name` | string | File name under `_pilot/scripts/` |
| `content` | string | Inline script body |
| `source` | path | Alternative to `content`: read the body from this file |

Provide either `content` or `source` per script.

## `slack`

| Field | Type | Description |
| --- | --- | --- |
| `bot_token` | string | Slack bot token (`xoxb-…`) |
| `app_token` | string | Slack app-level token (`xapp-…`) |
| `anchor_channel` | string | Channel the mirror anchors to |
| `per_workspace_channels` | bool | Give each workspace its own channel |

See [Mirror to Slack](../how-to/mirror-to-slack.md) for setup.

## `agent`

| Field | Type | Description |
| --- | --- | --- |
| `autonomous_skip_permissions` | bool | Autonomous @pilot work runs Claude with `--dangerously-skip-permissions` (blast radius bounded to the worktree) |
| `skip_permissions` | bool | Skip permission prompts for interactively spawned agents |

## `providers`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `github.poll_interval` | seconds | `60` | How often the GitHub provider polls |

## `ui`

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `log_path` | path | `/tmp/pilot.log` | Where logs are written |
