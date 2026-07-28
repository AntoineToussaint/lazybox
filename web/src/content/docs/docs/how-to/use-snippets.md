---
title: Use snippet workflows
description: Turn recurring agent instructions into fast, reusable workflows with recent-first and per-workspace memory.
---

Snippets are **repeatable agent workflows with memory**. Turn “review this
change,” “fix CI,” “integrate feedback,” or “run the final checks” into one
visible action; lazybox remembers what you used recently and which workflows
each workspace has already received.

lazybox ships 41 categorized workflows, so start by using one before you create
anything.

## Send and repeat a built-in workflow

1. Focus an agent terminal and type `]]s`.
2. Move through the categorized list with `↑`/`↓` and inspect the full body in
   the live preview.
3. Type `rev`. Because that key is a unique match, lazybox sends and submits the
   built-in review workflow immediately. The fast path is `]]srev`; there is no
   extra `Enter`.
4. Return later and type `]]s` again. The last workflow is selected in the
   **Recent** group, so press `Enter` to repeat it.

Typing in the picker filters case-insensitively across key, description, and
category. `Enter` sends the highlighted row; `Esc` or `Ctrl-C` cancels. Exact
keys auto-submit only when they are unambiguous.

## Find the right workflow before sending

The terminal picker groups workflows by category and shows a live preview with
the selected workflow’s complete body and origin: `built-in`, `global`, or
`repo`.

To browse the whole catalog without a focused terminal, press `]` from the
sidebar or activity pane. You can also choose **Browse snippets** from the `,`
Settings palette. The read-only browser lists every merged key, origin,
description, and body; press `e` there to open the global YAML file.

## Read the workflow memory

Two persisted cues answer different questions:

- **What do I reuse most?** The five most recently sent keys float to the top of
  every picker, newest first, with the last one selected. Recent is
  de-duplicated and stored in `~/.lazybox/v2/state.db`, so `]]s` then `Enter`
  still repeats your last workflow after a restart.
- **What has this workspace recently received?** Each workspace remembers an
  MRU of its 12 most recently distinct sent snippet keys. The sidebar renders
  that bounded count as `]N`: `]2` means two different workflows are in
  recent history; `]12` means older keys have fallen out. Repeating one moves
  it to the front without increasing the count.

Only a workflow that was actually delivered enters either history. Opening or
cancelling the picker records nothing.

## Create or improve one with Ask Lazybox

Press `?` and ask in plain language:

> Add a snippet called `feedback` that integrates review feedback, runs the
> relevant tests, and commits the result.

Ask Lazybox proposes the key, category, description, body, and destination in a
confirm-with-preview. If the key already exists, the preview explicitly says it
will replace the workflow. Accepting validates and writes the global
`~/.lazybox/snippets.yaml`, then **hot-reloads** the catalog. Use
`]]sfeedback` immediately; no restart is needed. Declining writes nothing.

This flow can also improve a built-in or global workflow by replacing its key
in the global layer. Launch-directory workflows remain file-owned: edit
`<launch-dir>/.lazybox/snippets.yaml` directly when this client needs an
override.

## Define global and launch-directory workflows in YAML

For direct control, add an entry under `snippets:`:

```yaml
snippets:
  feedback:
    description: Integrate review feedback and verify it
    category: Review
    body: |
      Read the unresolved review comments, implement each requested
      change that is still applicable, run the relevant tests, and
      commit the result. Report any comment you did not address and why.
```

The outer key (`feedback`) is what you type after `]]s`. A useful description
and category make large libraries searchable; the body is the complete
instruction sent to the agent.

The catalog merges from least to most specific:

| Scope | Path | Best for |
| --- | --- | --- |
| Built-in | Shipped with lazybox | 41 daily engineering workflows |
| Global | `~/.lazybox/snippets.yaml` | Personal habits across every repository |
| Launch directory | `<launch-dir>/.lazybox/snippets.yaml` | Overrides for this client catalog |

Precedence is **built-in → global → launch directory**, so starting lazybox
from a project can redefine `test` with that checkout's command or tighten
`rev` around local conventions. The picker labels this winning directory
layer as `repo`.

The directory layer is resolved once when the client starts and is shared by
all of its workspaces. Moving the sidebar selection does not load another
workspace's `.lazybox/snippets.yaml`; restart lazybox from a different
directory to select a different directory layer.

Hand-edited files load at startup. Restart lazybox after creating, updating, or
removing a YAML entry. Removing an override reveals the less-specific
definition beneath it.

## Broadcast a workflow across workspaces

To seed the same process across several agents:

1. In the sidebar, press `v` on each target workspace.
2. Press `Shift-B` and choose a snippet. Press `Ctrl-F` instead if you want only
   free text.
3. Review or extend the pre-filled body in the compose textarea, then submit.

Running agents receive the instruction, plain shells receive a direct write,
and workspaces without a session are skipped and named in the summary. The
snippet enters Recent once and is recorded on every workspace that actually
received it, so their `]N` badges show where the workflow has begun.

## See also

The [full snippets reference on GitHub](https://github.com/AntoineToussaint/lazybox/blob/main/docs/snippets.md)
covers the complete key protocol, body-writing style, file format, and delivery
behavior. See [Orchestrate multiple agents](/docs/how-to/orchestrate-multiple-agents/)
for the rest of the multi-select and broadcast workflow.
