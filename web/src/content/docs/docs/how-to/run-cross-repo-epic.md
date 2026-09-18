---
title: Run a cross-repo epic
description: Drive a tracker dependency graph through blockers, roles, workers, reviews, and ordered merge from one live inbox.
---

Goal: make a parent issue the live control plane for work that spans several
issues or repositories. Lazybox derives status from the tracker graph and its
own sessions, so the operator and every Coordinator see the same blockers,
ready work, and merge order.

## 1. Build the tracker graph

Create or choose a GitHub parent issue, then attach each unit of work as a
sub-issue. Add native blocked-by relationships for hard dependencies. Keep one
deliverable on one tracker record: the issue and the PR that closes it share a
single lazybox workspace, terminal history, worktree, role, and cost record.

The parent appears in the epic tier after the next poll. Its header shows the
live done/total count and the highest-priority blocker. Open the epic overview
from the `E` menu; `E g` opens the full dependency graph. The `blocked` and
`ready` filters use the same derived state, and `E j` jumps to the next blocked
member, declared blockers first.

## 2. Declare blockers where they happen

Dependency edges are automatic. For information or action that is not another
tracker record, the working agent calls `report_blocker` with a concrete reason
and kind. That blocker leads the epic overview and remains visible after the
agent turn ends. Once resolved, the agent calls `clear_blocker`.

A Coordinator should answer “give me status” with one `epic_status` call, not
by crawling GitHub. `epic_ready` returns only unblocked, unclaimed members that
can start now. The `epic:*`, `lazybox:ready|blocked|done`, and `role:*` GitHub
labels are projections for visibility; lazybox writes them only when their
value changes, and they are not a second source of truth.

## 3. Assign roles and start work

Focus a member and press `E r` to set Worker, Reviewer, Planner, Coordinator,
or Integrator. The badge and `role:*` label follow the record. `E p` and `E c`
are shortcuts that set the corresponding role and start its agent.

A Coordinator can call `spawn_worker` with an existing tracker reference and a
brief. The tool refuses non-Coordinators, duplicate live work, the
`agent.max_epic_workers` cap, and requests that would create a side workspace.
New work starts by creating the tracker record through the tool; named
workspaces remain repo-less scratch only.

## 4. Set merge order

Lazybox combines explicit `Merge after:` relationships with dependency-implied
order. `E m` shows the resulting sequence and why a green PR is held. When a
predecessor lands, its successor is released immediately; it does not wait for
the next provider poll. `E g` lets you inspect the same order as a graph before
arming any automation.

## 5. Choose the autonomy level

The three epic switches are independent and off by default:

| Key | Automation | Effect |
| --- | --- | --- |
| `E A` | Dispatch | Start ready Workers up to `agent.max_epic_workers` |
| `E R` | Review | Dispatch a fresh Reviewer when a member becomes reviewable |
| `E M` | Merge | Arm green PRs and merge them in dependency order |

Each switch confirms before it is armed. Dispatch respects `no-auto-fix` and
`do-not-lazybox`; a blocking review note holds merge. Start with status only,
then add one switch at a time so ownership remains obvious.

## Verify the loop

Before leaving an epic unattended, check that:

- the parent header, overview, `epic_status`, and filters agree;
- a declared blocker appears first and clears after `clear_blocker`;
- every `spawn_worker` lands on the referenced issue workspace;
- a green successor stays held until its predecessor merges; and
- disabled automation creates no workers, reviews, or merges.

See the [keybinding reference](/docs/reference/keybindings/) for the complete
`E` menu and [orchestrate multiple agents](/docs/how-to/orchestrate-multiple-agents/)
for broadcasts, handoffs, notes, and direct agent-to-agent questions.
