# Coordinator session (sketch)

Status: **optional future sketch**, not committed roadmap. The control surface
is the thesis ([`positioning.md`](positioning.md)); this is garnish on top of
it. Written only so the idea isn't lost.

## Concept

A normal lazybox workspace whose agent is tagged as a **coordinator** and owns
a set of child worker workspaces (a "fleet"). You talk to it in its terminal
like any agent; it (or you) dispatches to the fleet and watches it come back —
a named home for the coordinate-via-separate-workspace pattern people already
run by hand.

```
┌ Sidebar ───────────────┐  ┌ Coordinator: "auth-refactor" ────────────────┐
│ ▸ ◆ auth-refactor  🎧  │  │ you ▸ split the auth refactor across the      │
│     ⤷ ⇄ worker: api   │  │        api, web, and docs repos               │
│     ⤷ ⇄ worker: web ! │  │ orch ▸ spawned 3 workers. api: PR open, CI ✓  │
│     ⤷ ○ worker: docs ✗│  │        web: asking you a question  !          │
│ ▸ obin-ai/…            │  │        docs: CI failing ✗ — routing the log   │
└────────────────────────┘  └───────────────────────────────────────────────┘
```

## The only new state

`fleet_parent: Option<WorkspaceKey>` on a workspace (children point at their
coordinator). Everything else derives from that one link.

## Verbs — all map to plumbing that already exists

| Coordinator verb | Built on | Status |
| --- | --- | --- |
| `spawn worker <brief>` | start-anywhere (`Shift-W`) + set `fleet_parent` + inject brief | ~wiring |
| `send to worker N <msg>` | `send-to-session` (`x s`), target = a fleet child | exists |
| `broadcast <msg>` | `Shift-B` scoped to fleet children | exists |
| `fleet status` | per-workspace derived PR/CI/review/asking state | exists (scope it) |

## The on-thesis twist

Not a dashboard (AO's move) — a **fleet-scoped reactive inbox**: only this
coordinator's children's CI failures, review comments, and "agent is asking"
events, with read/unread triage. Reuses the entire attention engine, filtered
to `fleet_parent == me`. You triage the fleet like an inbox instead of scanning
a board.

## Autonomy dial (earn it)

1. **Manual** — you run the verbs.
2. **Assisted** — the orchestrator agent *proposes* spawns/sends; you confirm.
3. **Auto** — the agent drives, with existing guardrails (auto-fix arm/disarm,
   merge-on-green latches). Ship 1, earn 3.

## Phased (each slice dogfoodable)

1. `fleet_parent` link + "spawn worker under this coordinator" + sidebar
   grouping (reuse repo-group render). *Dogfoodable immediately.*
2. Coordinator verbs in the action catalog, scoped to the fleet (mostly
   re-pointing `send-to-session` / broadcast).
3. Fleet-scoped inbox — a `fleet_parent == me` filter over the reactive inbox +
   a fleet-status readout in the activity pane. **The differentiator.**
4. Autonomy dial — let the orchestrator agent call the verbs via a structured
   run / hook, human-confirm first.

Scope read: Phases 1–3 are a small link + reuse of existing actions/filters;
Phase 4 is the only genuinely new surface. This is naming and smoothing the
coordinator workspace we already run, not building an orchestrator from zero.
