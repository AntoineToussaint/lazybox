---
title: Orchestrate multiple agents
description: Select workspaces across repositories, compose one reviewed instruction, and deliver it safely to every live agent or shell.
---

Goal: coordinate the same next step across several workspaces without visiting
each terminal. You will select targets across repositories, seed one instruction
from a saved workflow or free text, review it once, and see exactly where it
landed.

This is useful at fleet scale: ten repositories and fifteen sessions can all be
visible in one inbox, while a broadcast targets only the agents that need this
particular review, test pass, or change of direction.

Before starting, open at least two workspaces with live sessions (see
[Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/)). A
selection can mix coding agents and plain shells.

## 1. Select workspaces across repositories

Press `v` on a sidebar row to mark it. The mark survives `j`/`k` navigation, so
you can cross repo-group boundaries and keep marking arbitrary rows. A header
chip keeps the selected count visible.

Press `v` again to unmark the focused row. `Esc` clears the entire selection.

## 2. Open broadcast and confirm the targets

Press `Shift-B`. The snippet picker opens with a header like:

```text
Broadcast to 4: api #842, web #311, mobile #907, sdk #144
```

That target recap remains visible in the compose step. Read it before typing:
the broadcast acts on the selected set captured when you opened the flow, so
you always know which workspaces are about to receive the instruction.

## 3. Compose the instruction once

Choose either starting point.

### Seed it from a snippet

Pick a saved workflow such as `audit` (**Full pre-ship review**) from the
categorized snippet picker. The compose textarea opens pre-filled with the
snippet body. Edit it for this run, append project-wide context, or combine it
with a one-off request before submitting.

This keeps the repeatable part reviewed and reusable without making the final
message rigid. See [Use snippets](/docs/how-to/use-snippets/) for the built-in,
global, and repo-local libraries.

### Go straight to free text

Press `Ctrl-F` in the snippet picker to open an empty compose textarea. This is
the fast path for an instruction you do not expect to reuse. If no snippets are
configured, lazybox opens the empty composer directly.

In either mode, `Esc` cancels composition without clearing the sidebar marks.

## 4. Submit and read the result

On submit, lazybox routes the same reviewed body according to each target:

| Target | Delivery |
| --- | --- |
| Running coding agent | Settle-gated injection: paste first, then submit after the terminal repaint quiesces |
| Plain shell | Direct PTY write with the command submitted |
| No live session | Skip it and name the workspace in the summary |

The agent path is the same path used for a normal single-session prompt. It
avoids treating a pasted multiline body and its submit key as one burst, which
some agent composers interpret as a soft newline instead of a send.

A mixed result is explicit:

```text
sent to 3 workspaces (1 skipped: no session — docs #622)
```

- If at least one target received the instruction, lazybox clears the selection.
  Start a session and reselect any skipped rows if they still need the message.
- If every target was skipped, the selection stays marked so you can start the
  missing sessions and retry.
- A session-less target is never treated as a successful delivery.

## 5. Confirm Recent and per-workspace history

For a snippet-seeded broadcast, lazybox records the snippet in **Recent once**
for the broadcast, not once per target. It also records the snippet key on every
workspace that actually received it, so each delivered row's `]N` cue continues
to show the correct number of distinct saved workflows used there. Skipped rows
remain unchanged.

Free-text broadcasts do not alter snippet Recent or `]N` cues. Agent deliveries
still appear in that session's prompt history (`]]h`) like a normal prompt.

## Safe coordination patterns

- Broadcast a shared next step, then tell each agent to use its own repository's
  commands, conventions, and current diff.
- Prefer a saved snippet for workflows that need consistent review criteria;
  tailor the composed body when this run has special constraints.
- Re-read the target recap before submitting instructions that mutate branches,
  publish artifacts, or contact external systems.
- Use a mixed agent/shell selection when the shell rows can safely execute the
  exact composed text as a command. Otherwise, broadcast to agents and handle
  shells separately.

Good examples include "run the full pre-ship review and fix blocking findings,"
"address the open review comments, then run this repo's checks," and "stop
implementation and report the smallest viable design."

## Other fleet moves

### Bulk-update branches behind main

When several of your PRs have fallen behind their base branch, select them and
press `Shift-U`. lazybox issues one branch update per **behind** PR (merging the
base into the head). Up-to-date PRs and any non-PR rows in the selection are
skipped and counted in the result notice — you never trigger a needless update.

This is the multi-select twin of `g u` (["Update branch"](/docs/reference/keybindings/))
on a single workspace.

### Hand work from one agent to another

`x s` (**send to session**) is an agent-to-agent handoff — the primitive behind
planner→executor workflows:

1. Focus the workspace whose agent produced something worth passing on (a plan,
   a diagnosis, a list of files).
2. Press `x s`. lazybox captures that agent's on-screen output.
3. Pick the **target** workspace. The list shows only workspaces with a running
   agent to receive the brief, and excludes the source workspace — so a handoff
   can never loop back into itself or land somewhere with no agent.
4. Edit the brief — the captured output is the starting point, not the final
   text — then submit. lazybox injects it into the target session's agent and
   submits it.

A visible `source → target` notice records the trail, so a chain of handoffs
stays legible.

## See also

- [Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/) — spawn
  the agents you'll be orchestrating.
- [Use snippets](/docs/how-to/use-snippets/) — the reusable prompts a broadcast
  draws from.
- [Keybindings reference](/docs/reference/keybindings/) — every chord, by pane.
