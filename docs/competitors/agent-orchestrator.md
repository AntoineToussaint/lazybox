# Competitor brief: Agent Orchestrator (aoagents.dev)

Status: **field notes** from a code + docs read (2026-08). Not exhaustive.

## What it is

An orchestration-first control plane for many coding agents. A human talks to
an **orchestrator agent** in natural language; it plans and spawns worker
agents, dispatches work, and reports fleet status back. Codename seen in the
source: "ReverbCode."

## Architecture (from the pulled source)

- **Go daemon** (`backend/`, ~112k LOC) — owns state, runs the orchestrator and
  workers, exposes an API.
- **Electron/React frontend** — the primary desktop UI.
- **`ao` CLI** — a thin client (`ao spawn`, `ao send`, …) over the daemon.
- **Plugin adapters** (`backend/internal/adapters/`): `agent` / `tracker` /
  `scm` / `runtime` / `workspace` / `notifier`. Clean seams for third parties.
- **~23 agent harnesses**; trackers for GitHub / GitLab / Linear.

So — **also a client/daemon split** (Go daemon + Electron + CLI). That is *not*
a lazybox differentiator; it's convergent. The difference is the thesis on top.

## Their thesis vs ours

| | Agent Orchestrator | lazybox |
| --- | --- | --- |
| Center of gravity | The orchestrator agent (delegation) | The human's control surface + reactive inbox |
| Default posture | Autonomous — "stop babysitting" | Human-in-command, high-leverage |
| Headline verb | `spawn` / `send` (talk to a manager) | multi-select, broadcast, snippet, handoff |
| Plugin story | Strong (typed adapter seams, 23 harnesses) | `GenericCli` + provider trait (narrower today) |
| Surfaces | Electron desktop + CLI | TUI + desktop + iPhone over one daemon |

## What they do genuinely well (worth respecting / borrowing)

- **The orchestrator UX is a real, named feature** — not an emergent pattern.
  Conversational spawn/send with status synthesis is polished.
- **Adapter seams** are clean and numerous — a better-documented plugin story
  than lazybox has today.
- **Distribution/timing** — ~8.8k GitHub stars, strong launch momentum.

## Reality-check on the company

- Org **Untrivial.ai**, GitHub org created **2026-07-28** — very young.
- **~6 months** old as a project by the read.
- **YC application** signal; **no confirmed funding round** found.

## So what (implications for lazybox)

1. **Don't chase the orchestrator.** Their headline is the risky, not-yet-there
   end of the spectrum. Our control-surface bet is the safer, shipped one.
   See [`positioning.md`](../positioning.md).
2. **Borrow the plugin polish, not the thesis.** Their adapter seams are a good
   model for tightening our provider/agent registry and `GenericCli` story.
3. **A [coordinator session](../coordinator-session.md), if we build it, is
   garnish** on the control surface — a named home for the
   coordinate-via-separate-workspace pattern we already run by hand — not a
   pivot to autopilot.
4. **Lead with breadth they don't have**: remote multi-client reach (one daemon
   → TUI + desktop + iPhone) and the depth of the control layer.
