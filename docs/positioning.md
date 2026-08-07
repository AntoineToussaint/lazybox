# Positioning: lazybox is a control surface, not an orchestrator

Status: **working thesis** (strategy note, not a spec). Written after
comparing lazybox to [Agent Orchestrator](competitors/agent-orchestrator.md)
(aoagents.dev).

## The one-liner

> **Agent Orchestrator** automates the fleet so you touch it less.
> **lazybox** gives you *fine-grained, high-leverage control* of the fleet so
> you can direct many agents precisely — human-in-command, at scale.

## The spectrum

Every "multiple coding agents" product sits somewhere on one axis: how much
the human stays in the loop.

```
manual, one agent  ─────────────────────────────────►  fully autonomous fleet
      (raw CLI)          [ lazybox ]        [ AO's orchestrator ]
                     high-leverage control      delegation / autopilot
```

- The **right end** (AO's bet): a manager agent plans and farms out work —
  "stop babysitting." Powerful when it works; still unreliable in 2026, and
  most people don't yet trust an agent to spawn/merge on their behalf.
- The **left end**: raw `claude` / `codex` in a terminal, one agent, all
  bookkeeping by hand.
- **lazybox owns the high-leverage middle**: the human controls *every* agent,
  but gets power tools that make controlling *many* tractable. Hands on the
  stick, but with a cockpit instead of a single throttle.

"Orchestrator isn't there yet" is a feature, not a gap. Betting the product on
autonomous delegation is premature; betting it on **precise control at scale**
matches what people actually need today.

## The moat: the control layer

This is the part an orchestrator-first design tends to skip, and where lazybox
is unusually deep. None of it is one headline feature — it's ~a hundred small,
battle-tested affordances, which is exactly what's hard to copy quickly.

| Capability | What it does | Key |
| --- | --- | --- |
| **Multi-select + broadcast** | Mark N workspaces, send one instruction to all — settle-gated inject per running agent, direct write for shells, session-less skipped and named. | `v` / `Shift-B` |
| **Snippet system** | Category-grouped command palette for agents: live body preview, auto-submit on unique key (`]]srev`), MRU "Recent" group, persisted across restart. | `]` / `]]s` |
| **Send-to-session handoff** | Capture one agent's on-screen output, brief it, inject into another agent (source excluded — no loop-back); a `source → target` notice records the trail. | `x s` |
| **Prompt history + recall** | Every prompt sent to an agent, newest-first, re-sendable; in-flight drafts and last-prompt recall survive restart. | `]]h` / `]]r` |
| **Model-tier dispatch** | Fire the same task at small / medium / large per agent, tier label rides a tab badge. | `w S/M/L`, `a S/M/L` |
| **On-main group** | Run an agent/shell on the repo's shared main checkout instead of an isolated worktree, confirmed, badged `⎇ main`. | `b c/x/u/s` |
| **Bulk branch update** | Rebase/merge base into head across every selected behind-`main` PR at once. | `Shift-U` |
| **Fleet triage** | Focus mode, rich multi-axis filters, mailboxes, jump-to-failing-CI / jump-to-asking. | `.` / `f` / `Shift-F` / `!` |

That's a **cockpit**, not an autopilot.

## Reactive inbox = the other half

The control layer is *how you act*; the reactive inbox is *why you know to
act*. Events flow to you — new comments, CI failures, review requests, "agent
is asking" — with read/unread tracking, instead of you polling GitHub. Paired
with the control layer, the loop is: **the inbox tells you which of your many
agents needs a hand; the control layer lets you give it in one or two keys.**

## Where the "orchestrator" idea fits (optional, later)

lazybox already supports the orchestration *pattern* by hand — a coordinator
workspace whose agent directs others via `send-to-session` and broadcast. If we
ever first-class it (see the coordinator-session sketch), it should feel like
*"the control surface got a memory"* — a named home for the coordinator
workspace you already run — **not** *"lazybox became an orchestrator."* The
control surface stays the thesis; a coordinator session is garnish on top of
it.

## Why this is defensible

- **Already shipped.** This isn't a roadmap item — it's what lazybox is today.
  We're naming what we have, not chasing a competitor's headline.
- **Right side of the trust curve.** Human-in-command degrades gracefully; an
  autopilot that mis-delegates fails loudly.
- **Breadth, not one feature.** The edge is the density of the control layer
  plus remote multi-client reach (TUI + desktop + iPhone over the client/daemon
  split) plus `GenericCli` (any agent, no recompile). Convergent competitors
  copy a feature; the surface takes years.
