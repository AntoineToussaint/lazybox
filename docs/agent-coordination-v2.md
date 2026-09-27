# Agent coordination v2 — one task model, one delivery path, a shared plan

Status: **proposal** (2026-09-27). Supersedes nothing yet; builds on
[`mcp-coordination.md`](mcp-coordination.md),
[`orchestration-scoping.md`](orchestration-scoping.md),
[`coordinator-session.md`](coordinator-session.md) and
[`agent-artifact-channel.md`](agent-artifact-channel.md).

## Why

The user runs many agents across many repos and still has to be the glue:
agents lose track of each other, a delegation's result is scraped off a
terminal, and the TODO list (the Hopper) cannot express a plan. Three
findings drive this proposal.

**1. lazybox has seven ways to put text into an agent and no way to get a
result back.** Live inject, snippets, the spawn prompt, broadcast, resume,
auto-fix and credit recovery each have their own gate and their own meaning
of "delivered"; `handle_inject_prompt` returns `()`, and a sibling's answer
to `ask_session` is the last 60 lines of scrollback when its turn ends
(`answered_by_capture`). The structured data that would carry a real answer
— the Stop hook's content and `transcript_path` — is dropped at ingest
(`agents/src/hook.rs`). Every fix to delivery lands in one of the seven
paths (#122, #725, #869, #1384, #1544, the 2026-09-23 Shift-K regression).

**2. Identity is a mutable string key.** Every workspace-keyed row has to be
hand-moved on the issue→PR fold; that bug class has shipped three times
(#1793, #1836, #1837) and notes, requests, reviews and the baked
`LAZYBOX_SESSION_KEY` are still not moved.

**3. The research is consistent: coordination is carried by deterministic
shared state, not by more messages.** A2A, MCP Tasks, Claude Code agent
teams, Copilot's issue+PR and Magentic-One all converge on a *typed task
with a lifecycle*, results returned as *artifacts by reference*, and
*subscriptions* instead of polling. CooperBench (2026) measured two
cooperating agents ~30% *worse* than one, from vague messages and wrong
beliefs about the partner; one structured "this change just landed" notice
fixed 82% of interference in "Passes Alone, Fails Together" (2026). CAID
and AgentRoom credit isolated workspaces + claims + git merge, not chat.
Cognition's rule stands: one writer per scope, everything else read-only.

And one constraint from the user: **every agent must have all the lazybox
context it needs without flushing its context window.**

## Principles

1. **Tasks, not messages.** Anything one party asks of another is a task
   with an id, a lifecycle and a result. Messages are annotations on a task.
2. **One delivery path.** Every "put this in front of an agent" goes through
   one owner with one gate and one definition of delivered, and it reports
   back.
3. **Results by reference.** A finished task returns a short summary plus
   artifacts stored by the daemon — never scraped scrollback.
4. **Push, with a snapshot first.** Agents and the TUI subscribe; the first
   event is current state, then deltas. Polling remains only for recovery.
5. **Progressive disclosure of context.** Always-on context is small and
   budgeted; everything else is pulled on demand or pushed when relevant.
6. **Stable identity.** Rows are keyed by an immutable id; the workspace key
   is a mutable display alias.
7. **Structured state beats chat.** The plan, claims and change notices are
   state; free text is for humans.

## The model

### Task

```text
Task {
  id: TaskId                    // immutable, daemon-minted
  plan: Option<PlanId>          // the TODO tree it belongs to
  parent: Option<TaskId>        // nesting (sub-TODO)
  title, brief                  // brief = objective · done-criteria · boundaries · output shape
  owner: Party                  // who does it: Human | Agent(session) | Unassigned
  requester: Party              // who asked (lazybox itself is a Party)
  links: Vec<Link>              // workspace, issue, PR, URL — the work it points at
  state: Pending | Working | InputRequired{question} | Blocked{reason}
         | Completed | Failed | Canceled
  result: Option<Result { summary, artifacts: Vec<ArtifactRef>, outcome }>
  history: Vec<Event>           // who changed what, when (provenance)
}
```

The state names follow A2A's `TaskState` (`INPUT_REQUIRED` as a first-class
interrupt; terminal states reject further work), so an A2A bridge later is a
mapping, not a redesign.

### Plan = the TODO tree

A plan is a tree of tasks. The Hopper becomes **TODO** and each top-level
TODO line is a plan root:

- **Sub-TODOs** are tasks with a `parent`. They are lightweight — a checklist
  item does not create a workspace (the Hopper's cost today: every line is a
  workspace). Any item can *link* to a workspace, issue or PR, and `Enter`
  jumps to it.
- **Progress** rolls up like GitHub task lists: `▰▰▱ 2/3` per line and a
  total at the top of the TODO group.
- **Auto-check:** a task linked to a PR completes when the PR merges, to an
  issue when it closes, to a workspace when its task is completed.
- **A plan with linked items in several repos *is* a local epic.** The
  existing `EpicRecord` (kv-only epics with explicit members) becomes the
  backing store's coordination view, so `epic_status`, blockers, the ready
  queue and merge-after keep working — no second concept beside epics.
- The same tree is what agents read and update (below), so the user and
  the agents share one plan.

### Party and the one delivery path

`Party` is `Human | Agent(SessionId) | Lazybox`. "lazybox talking to an
agent" (a `w w` work prompt, auto-fix, resume) and "an agent talking to an
agent" (ask, handoff, notify) become the same operation:

```text
deliver(to: SessionId, task: TaskId, kind: Assign | Message | Resume)
  -> DeliveryReceipt { accepted | deferred(reason) | refused(reason) }
```

One owner (the #1688 extraction target) holds the gate: never paste into a
mid-turn agent (today a Working agent is not gated), wait out a chooser,
dedupe per terminal, confirm the submit, and **report back to the requester
as a task event** rather than a TUI toast. The seven current paths become
callers of it. `PromptSource` gains the requesting `Party`, so history shows
who sent what.

### Results and the Stop hook

An agent finishes a task by calling `complete_task(id, summary, artifacts,
outcome)`. For agents that won't, lazybox falls back to the **Stop hook's
content and transcript** (no longer discarded) and then to scrollback, and
marks the result's provenance accordingly. `reply_request` becomes one case
of `complete_task`; `answered_by_capture` becomes the last-resort fallback
instead of the mechanism — and the race where a turn already in flight
answered a new question goes away because delivery no longer pastes into a
Working agent.

Artifacts are the existing `.lazybox/artifacts/` spool (#1822) plus
daemon-stored blobs, addressed by reference so a result costs the requester
a few hundred bytes, not a transcript.

### Subscriptions

`subscribe(scope)` where scope is a task, a plan, a session or a repo.
First event: the current snapshot. Then typed deltas: task state changes,
results, blockers, and **change notices** — "PR #N merged into `repo`,
touching these paths, base `sha`" pushed to every workspace working in that
repo or plan (the 82% finding). For MCP clients this rides a long-poll tool
(`wait_for_events(cursor, timeout)`) until MCP notifications are usable
across agents; the TUI already has the daemon bus.

### Board v2 (the notes board, rebuilt)

Typed entries instead of free text with magic tags:
`{ kind: decision | contract | finding | question, scope: plan | repo | task,
base_sha, supersedes, author: Party }`. Entries anchored to a base commit
show as stale once main moves past it; a new entry can supersede an old one
instead of appending forever. The epic latches read `kind`, not hand-typed
tags (today a typo is dropped with a `debug!`). The global write lock no
longer spans `recompute_all`.

### Claims

`claim(paths, base_sha)` for files an agent is about to change; the daemon
flags overlap between worktrees on the plan before edits collide. Replaces
the GitHub-label claim for in-box exclusion; cross-box claims move from
labels to a single edited comment (already queued).

## Context economy

Today every Claude session carries up to 6.25 KB of mechanics (capped by a
test in `agents/src/session_context.rs`) plus ~1.1 KB of standing rules,
plus the descriptions of ~25 coordination tools — whether or not it ever
coordinates. Codex, Cursor and generic agents get less and have no MCP at
all.

Proposed tiers:

| Tier | Contents | Budget |
|---|---|---|
| **Always-on** | identity (task id, workspace, repo, role), standing rules, one line: "lazybox context: call `lazybox_guide`" | ~1.5 KB, test-enforced |
| **On demand** | `lazybox_guide(topic)` — labels, epics, handoff, artifacts, gh budget… — returned only when asked | per topic |
| **Pushed** | subscription deltas relevant to *this* task (sibling merged, question answered, blocker cleared) | a few hundred bytes each |
| **Queried** | `my_task`, `plan_status(slice)` return only the requested slice | bounded by the query |

And a smaller tool surface: the ~25 MCP tools collapse to about eight
(`my_task`, `plan_status`, `update_task`, `complete_task`, `delegate`,
`ask`, `wait_for_events`, `lazybox_guide`), with the tracker reads folded
behind them. For agents without MCP, the same verbs ship as `lazybox task …`
CLI subcommands (the existing `lazybox task status` fallback, generalized),
so Codex/Cursor stop being second-class.

## Fixes this subsumes

Found by the channel inventory, fixed by construction:

- Hooked Claude sessions get box-wide standing rules only —
  `standing_rules_from_disk(None)` in the hook (`lifecycle.rs`) while the
  spawn-side args are skipped when hooks exist (`spawn_plan.rs`), so
  `repos.<r>.policies` never reaches the default Claude session.
- `ask_session` saves the request before injecting, and inject does not wait
  out a Working target, so the `Done` of a turn already in flight can answer
  the new question with unrelated scrollback.
- After a fold, `reply_request` is refused (requests store string keys) and
  notes scoped to the old key are invisible to the default read.
- Role preambles apply only to a fresh spawn with a prompt; a role set
  afterwards never reaches the agent.
- `list_sessions` drops exited agents, so "did my sibling finish?" is
  unanswerable.

## Phasing

Each phase ships on its own and is useful alone.

1. **TODO.** Rename Hopper → TODO; sub-TODOs, progress roll-up, links,
   auto-check; the task/plan store with immutable ids. *User-visible first.*
2. **Delivery + results.** The single delivery owner with receipts;
   `complete_task`; keep the Stop hook's content; tasks for `w w`, ask and
   auto-fix; fix the ask race and per-repo rules as part of it.
3. **Agent surface + context tiers.** The ~8 tools, `lazybox_guide`, the
   slim briefing with its budget test, and the CLI twin for non-MCP agents.
4. **Subscriptions + change notices + board v2 + claims.**
5. **Across boxes.** Today each box's daemon is its own island (notes,
   requests and sessions are per-daemon). Federate tasks and subscriptions
   over the existing relay/e2e channel, and use the task record as the
   remote-handoff unit. A2A's data model is the wire shape if we ever speak
   to agents outside lazybox.

## Open questions

- Sub-TODOs: plain checklist items that can link to work (proposed), or can
  any item be promoted into its own workspace with one key?
- Auto-check on merge/close: on by default?
- Does `claim` gate edits (refuse) or only warn on overlap? The research
  favours warn-and-show over hard refusal for coding agents.
- Board v2 migration: convert existing notes, or start fresh and keep the old
  board read-only?

## Sources

A2A spec (a2a-protocol.org/latest/specification); MCP 2025-11-25 Tasks and
the 2026-07-28 release; Zed Agent Client Protocol (plans, tool calls);
Claude Code agent teams and cross-session messaging docs; Anthropic,
"How we built our multi-agent research system" and "Effective context
engineering"; Cognition, "Don't build multi-agents" (2025) and its 2026
follow-up; Magentic-One (AutoGen); MetaGPT; MAST (arXiv 2503.13657);
CooperBench (arXiv 2601.13295); "Passes Alone, Fails Together"
(arXiv 2609.25396); CAID (arXiv 2603.21489); AgentRoom (arXiv 2608.23740);
Claim Plane (arXiv 2607.21909).
