---
title: Run an agent per workspace
description: Point lazybox at a task and route its context, agent, model, and effort automatically.
---

Goal: spawn a coding agent (Claude Code, Codex, or Cursor) scoped to a single
workspace's git worktree — or let lazybox route the task's context, agent,
model, and effort for you.

Each workspace gets one worktree; agent and shell sessions — one or several,
as splits or tabs — all operate in that worktree. The agent works directly
with the same `git` and `gh` tools it would have in
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

## Point at the work and press `w w`

`w w` is the primary **work on this** action. The first `w` opens the work
menu; the second tells lazybox to infer the intent from the focused workspace
instead of making you copy context into a prompt.

The generated brief follows this precedence:

1. Activity rows selected with `v` → address exactly those comments.
2. A merge-conflicted PR → rebase and resolve the conflict.
3. A PR with failing CI → diagnose and fix CI.
4. A healthy PR assigned to you for review → review the code.
5. New activity on your own or assigned PR → address the unread feedback.
6. An open issue → implement the issue.
7. A PR without a more specific signal → continue work on the PR.

A closed or merged task is not restarted: lazybox points you to archive it.
On a scratch workspace, it starts the agent without inventing task context.

The action also chooses where the brief goes:

- one agent already running on the workspace → inject the brief into it;
- no running agent → launch `setup.default_agent` (Claude Code when unset) in
  that task's worktree;
- several agent conversations running (including two sessions of the same
  agent) → ask which exact conversation should take the work.

The focused workspace remains the reference frame even while you select rows
in the Activity pane, so you do not need to find a task folder, prompt
template, or existing terminal yourself.

## Let GitHub choose the model and effort

A GitHub task can select its compute profile before an agent starts. Add a
case-insensitive `high`, `medium`, or `low` label, or put `@high`, `@medium`,
or `@low` in the task body:

| GitHub priority | Intended tier |
| --- | --- |
| `high` / `@high` | strongest or deepest configured tier |
| `medium` / `@medium` | balanced configured tier |
| `low` / `@low` | fastest or cheapest configured tier |

This is agent routing, not merely inbox sorting. At spawn time, lazybox maps
the task priority through the target agent's `models.priority` table, then
appends that tier's `args` to the agent command. Those arguments can choose
both a concrete model and its reasoning effort.

Claude ships a built-in mapping: `low` → Haiku (`S`), `medium` → Sonnet (`M`),
and `high` → Opus (`L`). Other agents can define their own meanings for the
same three priorities:

```yaml
agents:
  codex:
    models:
      default: M
      tiers:
        - alias: S
          label: Fast / low effort
          args: ["-m", "your-fast-model", "-c", 'model_reasoning_effort="low"']
        - alias: M
          label: Balanced / medium effort
          args: ["-m", "your-balanced-model", "-c", 'model_reasoning_effort="medium"']
        - alias: L
          label: Deep / high effort
          args: ["-m", "your-strong-model", "-c", 'model_reasoning_effort="high"']
      priority:
        low: S
        medium: M
        high: L
```

Replace the example model ids and flags with values supported by your agent
CLI. The labels become `◆ Fast / low effort`-style terminal badges.

Priority is resolved only when a terminal is spawned. If `w w` injects into an
already-running agent, that session keeps its current model. A priority label
wins over a body marker; if several labels or several markers are present, the
strongest one wins.

### Override the priority in the TUI

Use `w S`, `w M`, or `w L` when you want to choose the tier directly. These
chords build the same contextual brief and target the same running/default
agent as `w w`, but the explicit tier wins over the GitHub priority for a new
spawn. `a S` / `a M` / `a L` spawn the default agent at a tier without the
contextual work brief.

## Trigger the whole workflow from GitHub

Put the priority marker and trigger in the issue body:

```text
@high
@lazybox codex
```

When the next full GitHub sweep finds the trigger, lazybox authenticates it,
opens the issue's workspace, chooses Codex's configured `high` tier, and starts
the agent with the issue-implementation brief. Under normal polling, full sweeps
run at daemon startup and roughly every ten minutes by default, so a new trigger
can wait about ten minutes before it starts. The issue chooses the work, agent,
model, and reasoning effort without opening the TUI. A `high` label plus a bare
`@lazybox` trigger does the same with Claude.

See [Trigger agents with @lazybox mentions](/docs/how-to/lazybox-mentions/)
for the allowlist and autonomous-permission settings.

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
