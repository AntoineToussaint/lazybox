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

Each explicit `a c` / `a x` / `a u` starts a **new** agent, even beside an idle
one of the same kind — a workspace can hold several agent conversations at once
(one-agent-per-workspace is gone). The daemon still collapses a genuine spawn
race (a double-fire lands one backend, not two) and adopts an issue→PR handoff,
so you never silently fork two backends. The reuse-first path is unchanged: bare
`w` / `w w` and autonomous work (`@lazybox` mentions, auto-fix) still inject into
a running agent rather than spawning a second one.

## Add another agent CLI

Generic agent definitions are loaded from `agents.<id>` at daemon startup.
Enable the same id and assign its chord explicitly:

```yaml
setup:
  agents: [claude, aider]
  default_agent: claude

agents:
  aider:
    name: Aider
    command: aider
    args: [--model, sonnet]
    resume_args: [--resume]
    asking_patterns: ["Proceed?"]

ui:
  action_keys:
    spawn_agent.aider: "a z"
```

After restarting lazybox, `a z` launches `aider --model sonnet` in the focused
workspace's managed worktree. Selecting Aider as `setup.default_agent` makes
`w w` launch it; `a c` still overrides that default with Claude for one task.
The command runs directly, without a shell, so each argument stays a separate
YAML list item.

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
case-insensitive `model:<tier>` label, or put `@model:<tier>` in the task body.
The token names a tier of the target agent's own menu, matched against its
alias, its label, or the model id it pins — so all three of these select
Claude's Opus tier:

| Declaration | Matched on |
| --- | --- |
| `model:l` | the tier alias |
| `model:opus` | the tier label |
| `model:claude-opus-5` | the model id the tier pins |

Claude's built-in menu ships four tiers: `S` Haiku, `M` Sonnet, `L` Opus
(the default), and `XL` Fable — the strongest model, reachable only when a task
asks for it by name. Create the labels with descriptions that say what they
do, not how urgent the work is:

```bash
repo=owner/name
gh label create model:s  --repo "$repo" --color 0E8A16 \
  --description "Agent runs on the small/fastest tier (Claude: Haiku). Model choice only — not priority."
gh label create model:m  --repo "$repo" --color 1D76DB \
  --description "Agent runs on the balanced tier (Claude: Sonnet). Model choice only — not priority."
gh label create model:l  --repo "$repo" --color 5319E7 \
  --description "Agent runs on the strong tier (Claude: Opus). Model choice only — not priority."
gh label create model:xl --repo "$repo" --color B60205 \
  --description "Agent runs on the strongest tier (Claude: Fable). Model choice only — not priority."
```

This is agent routing, not inbox sorting: the label picks the engine, never
the order work is picked up in, and nothing about it starts an agent. Keep
`critical` / `now` / `high` for ordering, and let `model:*` carry capability —
an urgent typo fix should not burn the strongest model, and a gnarly
non-urgent refactor should.

At spawn time lazybox resolves the declaration to a tier and appends that
tier's `args` to the agent command. Those arguments can choose both a concrete
model and its reasoning effort. Every agent defines its own menu, so the same
`model:l` label routes to whatever that agent calls its strong tier:

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
```

Replace the example model ids and flags with values supported by your agent
CLI. The labels become `◆ Fast / low effort`-style terminal badges.

The model is resolved only when a terminal is spawned. If `w w` injects into an
already-running agent, that session keeps its current model.

Resolution rules, in order:

- **A label wins over a body marker**, and a `model:` declaration wins over a
  legacy key from the same source — so a repo can migrate label by label.
- **A token no tier defines is skipped, not fatal.** If a task carries
  `model:v2` (a repo that versions its own ML models, say) alongside `high`,
  the unresolvable token falls through and `high` still routes. Only when
  nothing the task declares names a tier does the spawn keep the agent's
  default.
- **Two labels that name different tiers select nothing.** GitHub does not
  promise an order for a task's labels, so `model:s` plus `model:xl` would
  otherwise make the model a coin flip between polls; lazybox falls back to the
  default and logs the contradiction instead. Labels that name the *same* tier
  by different spellings (`model:l` and `model:opus`) agree and are fine.
- **An agent started by someone else reads labels only.** Attaching a label
  needs write access to the repository; anyone can open an issue and write its
  body. So when lazybox starts an agent on a trigger it did not get from you,
  it ignores `@model:` and `@best` markers in the body and honors only the
  task's labels — a drive-by issue cannot pick your most expensive tier on the
  unattended path.

### The deprecated `best` / `high` / `medium` / `low` keys

The model axis was originally declared with an urgency word: a `best`,
`high`, `medium`, or `low` label (or the matching `@` marker). Those
names say *when* to do the work but decide *what runs it*, so they are
deprecated in favour of `model:*` — a `high` label changes nothing about
ordering or pickup, which is the opposite of what it reads like.

They still work. Each routes through the target agent's `models.priority`
table, which for Claude maps `best` → Fable (`XL`), `high` → Opus (`L`),
`medium` → Sonnet (`M`), `low` → Haiku (`S`). A spawn that resolves through
one logs a deprecation naming the `model:` label that replaces it. Other
agents can remap them:

```yaml
agents:
  codex:
    models:
      priority:
        low: S
        medium: M
        high: L
        best: L
```

A `model:*` declaration outranks a legacy one on the same task, so a repo can
migrate label by label. If several legacy labels or markers are present, the
strongest wins.

### Override the model in the TUI

Use `w S`, `w M`, or `w L` when you want to choose the tier directly. These
chords build the same contextual brief and target the same running/default
agent as `w w`, but the explicit tier wins over the task's declaration for a
new spawn. `a S` / `a M` / `a L` spawn the default agent at a tier without the
contextual work brief. Only single-character aliases get a chord, so Claude's
`XL` (Fable) tier is reached by label, not by keystroke.

## Trigger the whole workflow from GitHub

Put the model marker and trigger in the issue body:

```text
@model:l
@lazybox codex
```

When the next full GitHub sweep finds the trigger, lazybox authenticates it,
opens the issue's workspace, chooses Codex's `L` tier, and starts the agent
with the issue-implementation brief. Under normal polling, full sweeps run at
daemon startup and roughly every ten minutes by default, so a new trigger can
wait about ten minutes before it starts. The issue chooses the work, agent,
model, and reasoning effort without opening the TUI. A `model:l` label plus a
bare `@lazybox` trigger does the same with Claude.

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

## Recover credit- and rate-limited agents

When an agent hits a provider limit, lazybox surfaces it (a `⏳` pill on the row
and header count) and gives you keys to recover a whole fleet without visiting
each terminal:

| Chord | Does |
| --- | --- |
| `Ctrl-k` | Recover the focused blocked agent from a provider **credit** chooser: select its "Wait for credit" option, wait for the composer, and submit the configured continuation prompt (`ui.credit_recovery_prompt`). Chooser detection is Codex-style today. |
| `Shift-K` | **Resume every** workspace blocked on a usage / **rate** limit at once — a settle-gated "continue" injected into each limit-blocked agent. For when the limit has reset. |
| `a R` | **Restart every** limited agent (blocked `⏳` or parked `💤`) **with fresh credentials**: lazybox stops its process, respawns the same conversation in the same pane (`--resume`), then submits the continuation prompt (`ui.credit_recovery_prompt`). Use it after switching Claude account / API key externally — a running process never re-reads its credentials, so a plain "continue" would only hit the limit again. |
| `x w` | **Reset** the focused agent's conversation context in place (injects the agent's own clear command — `/clear` for Claude, `/new` for Codex). Session, worktree, and prompt history survive; only the model's context resets. Confirmed first. |

Set `ui.auto_wait_on_limit: true` to auto-press "Wait" the moment a Claude agent
hits its limit, so a fleet all capping at once doesn't each need a manual visit.
Route agent traffic through the metering proxy (`agent.metering_proxy: true`) to
see live per-provider usage in the sidebar header before any limit is hit.

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

- [Orchestrate multiple agents](/docs/how-to/orchestrate-multiple-agents/) to
  select live sessions across repositories and inject one reviewed instruction
  into all of them.
- [Per-repo env and mounts](/docs/how-to/per-repo-env-and-mounts/) to give every
  agent session the environment and shared files it needs.
- [Manage automation policies](/docs/how-to/manage-automation-policies/) to
  control merge and auto-fix behavior for the workspace.
- The [keybindings reference](/docs/reference/keybindings/) for every sidebar
  action.
