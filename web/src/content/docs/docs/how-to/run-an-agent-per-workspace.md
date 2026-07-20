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

With a workspace selected in the sidebar, press `a` to open the agent menu
(a which-key popup), then the agent's key:

| Chord | Agent |
| --- | --- |
| `a c` | Claude Code |
| `a x` | Codex |
| `a u` | Cursor |
| `s` | Plain shell (no agent) |

An embedded terminal opens, running the agent in that workspace's worktree.
Press `]]` then `q` to return to the sidebar; the session keeps running.
(Prefer the old top-level keys? Remap them via `ui.action_keys`, keyed
`spawn_agent.<id>` — e.g. `spawn_agent.claude: "c"`.)

## Let lazybox choose the prompt: `w w`

Press `w w` for **work** instead of picking an agent manually. The first `w`
opens the work menu; the second chooses the default or already-running agent.
lazybox spawns
your **default agent** (`setup.default_agent`, falling back to Claude Code)
with a prompt tailored to the selected row's current state:

- failing CI → fix CI
- merge conflict → fix the conflict
- review comments → address the comments
- an issue → implement the issue

This is the fastest way to act on whatever the inbox is telling you about a row.

## Pick a model tier

Both leaders carry model-tier chords: `w S` / `w M` / `w L` run the work
prompt at a small / medium / large model, and `a S` / `a M` / `a L` spawn the
default agent at that tier. Claude ships a built-in Haiku / Sonnet / Opus
menu; other agents define theirs under `agents.<id>.models` in
`~/.lazybox/config.yaml` (see the
[configuration reference](/docs/reference/configuration/#agentsid)). The
picked tier's label rides a `◆ Opus`-style badge on the terminal tab.

## Choose splits or tabs

The first terminal occupies the workspace's terminal pane. By default, each
additional ordinary shell or agent spawn opens as a side-by-side split. Set a
tabs-first default instead:

```yaml
ui:
  terminal_new_layout: tabs
```

From inside a terminal, `]]t` flips this preference between `split` and `tabs`
and saves it. The change affects the next spawn, not terminals already open.
Explicit `]]|` and `]]-` commands always create a side-by-side or stacked split
regardless of the preference.

## Recover input and failed agents

Press `]]r` to restore the in-flight draft—or, when there is no draft, the last
submitted agent prompt—into the composer without sending it. The prompt is
persisted, so a lazybox restart does not erase the last command you were
working with.

Terminal exits are explicit rather than inferred from a quiet screen:

- a cleanly finished agent terminal closes automatically;
- a crash, signal exit, or non-zero exit stays frozen on its final screen with
  the exit code and a restart affordance;
- an agent that exits before it ever engages is treated as failed-to-start and
  also remains inspectable;
- the workspace and its worktree survive the failed process.

The failed-to-start grace period is configurable with
`terminal.agent_dead_on_arrival_ms`.

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
- [Manage automation policies](/docs/how-to/manage-automation-policies/) to
  control merge and auto-fix behavior for the workspace.
- The [keybindings reference](/docs/reference/keybindings/) for every sidebar
  action.
