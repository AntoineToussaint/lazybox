---
title: Orchestrate multiple agents
description: Drive a fleet of agents at once — multi-select, broadcast one instruction to many workspaces, bulk-update branches, and hand work from one agent to another.
---

Once you have more than one agent running, lazybox stops being an inbox and
starts being a control surface for a fleet. This guide covers the four moves
that scale you from one workspace to many: **multi-select**, **broadcast**,
**bulk branch updates**, and **agent-to-agent handoff**.

All of these live in the sidebar. Open a few workspaces with agents first (see
[Run an agent per workspace](/docs/how-to/run-an-agent-per-workspace/)).

## Select several workspaces

Press `v` on a sidebar row to mark it. The mark survives `j`/`k` navigation, so
you can move down the list marking rows as you go. Press `v` again to unmark;
`Esc` clears the whole selection.

The selection is the target set for the two bulk actions below. A header chip
shows how many rows are marked.

## Broadcast one instruction to all of them

With rows selected, `Shift-B` sends **one instruction to every selected
workspace** at once:

1. A snippet picker opens — pick a reusable workflow (see
   [Use snippet workflows](/docs/how-to/use-snippets/)), or press `Ctrl-F` to skip
   straight to free text. (With no snippets configured, it jumps straight to the
   compose step.)
2. A compose textarea opens, pre-filled with the snippet body. Edit it into the
   exact instruction you want fanned out.
3. Submit. lazybox delivers per target:
   - **Running agents** receive the text through the same settle-gated inject
     path `w w` uses (paste, then a separate submit once the repaint quiesces).
   - **Plain shells** get the text written directly.
   - **Session-less workspaces** (no terminal open) are skipped and named in
     the summary notice, so you know exactly who got it and who didn't.

Use it for "rebase on main and re-run the tests," "address the review comments,"
or any instruction that applies across a batch of PRs.

## Bulk-update branches behind main

When several of your PRs have fallen behind their base branch, select them and
press `Shift-U`. lazybox issues one branch update per **behind** PR (merging the
base into the head). Up-to-date PRs and any non-PR rows in the selection are
skipped and counted in the result notice — you never trigger a needless update.

This is the multi-select twin of `g u` (["Update branch"](/docs/reference/keybindings/))
on a single workspace.

## Hand work from one agent to another

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
- [Use snippet workflows](/docs/how-to/use-snippets/) — the repeatable
  instructions with memory that a broadcast draws from.
- [Keybindings reference](/docs/reference/keybindings/) — every chord, by pane.
