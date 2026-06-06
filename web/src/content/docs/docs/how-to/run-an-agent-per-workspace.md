---
title: Run an agent per workspace
description: Spawn Claude Code, Codex, or Cursor scoped to one worktree, including autonomous runs.
---

Goal: spawn a coding agent (Claude Code, Codex, or Cursor) scoped to a single
workspace's git worktree — including letting lazybox pick the right prompt for
the row's state, and running autonomously.

Each workspace gets one worktree and one agent session. The agent operates
directly in that worktree with the same `git` and `gh` tools it would have in
any checkout; lazybox does not wrap those actions behind an approval layer.

## Prerequisites

- A workspace is open in the sidebar (see [Add a repo](/docs/how-to/add-a-repo/)).
- The agent's CLI is installed and on your `PATH` (for example the `claude`
  binary for Claude Code). lazybox detects installed agents at startup.

## Spawn an agent

With a workspace selected in the sidebar, press one of:

| Key | Agent |
| --- | --- |
| `c` | Claude Code |
| `x` | Codex |
| `u` | Cursor |
| `s` | Plain shell (no agent) |

An embedded terminal opens, running the agent in that workspace's worktree.
Press `]]` (twice) to return to the sidebar; the session keeps running.

## Let lazybox choose the prompt: `w`

Press `w` for **work** instead of picking an agent manually. lazybox spawns
Claude Code with a prompt tailored to the selected row's current state:

- failing CI → fix CI
- merge conflict → fix the conflict
- review comments → address the comments
- an issue → implement the issue

This is the fastest way to act on whatever the inbox is telling you about a row.

## Autonomous runs and skip-permissions

For hands-off work, lazybox can run Claude with permission prompts disabled. The
blast radius is bounded to the workspace's worktree. Configure this in
`~/.lazybox/config.yaml`:

```yaml
agent:
  # Autonomous @lazybox-triggered work runs with --dangerously-skip-permissions.
  autonomous_skip_permissions: true
  # Also skip permission prompts for interactively spawned agents.
  skip_permissions: false
```

:::caution
`--dangerously-skip-permissions` lets the agent run tools without asking.
lazybox confines the agent to the worktree, but only enable this once you are
comfortable with autonomous edits in that directory.
:::

See the [configuration reference](/docs/reference/configuration/#agent) for the
full schema.

## Related

- [Per-repo env and mounts](/docs/how-to/per-repo-env-and-mounts/) to give every
  agent session the environment and shared files it needs.
- The [keybindings reference](/docs/reference/keybindings/) for every sidebar
  action.
