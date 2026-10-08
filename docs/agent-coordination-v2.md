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
paths (#725, #869, #1384, #1544, the 2026-09-23 Shift-K regression).

**2. Identity is a mutable string key.** Every workspace-keyed row has to be
hand-moved on the issue→PR fold; that bug class has shipped twice (#1793,
declared blockers; #1837, MCP session tokens) and notes, requests, reviews and
the baked `LAZYBOX_SESSION_KEY` are still not moved. A third defect has the
same root and needs no fold at all: #1836 pruned the blackboard by a
*sanitized* key prefix while reads filtered on the stored scope, so two
workspace keys that sanitize alike — `github:my-org/tools#42` and
`github:my/org-tools#42` both become `github_my_org_tools_42` — evicted each
other's notes. An immutable id closes that class only if it is *also*
collision-free under `sanitize_key`, which is why [Task](#task) below pins the
id to a uuid rather than to today's `TaskId`.

**3. The research converges on deterministic shared state, and splits on how
hard to enforce it.** A2A, MCP Tasks, Claude Code agent teams, Copilot's
issue+PR and Magentic-One all converge on a *typed task with a lifecycle*,
results returned as *artifacts by reference*, and *subscriptions* instead of
polling. CooperBench (2026) measured two cooperating agents ~30% *worse* than
a single agent doing both tasks, from vague messages and wrong beliefs about
the partner.

"Passes Alone, Fails Together" (2026) is the one to quote in full, because half
of it is easy to misuse: a structured "this change just landed" notice
*recovered 82% of runs* — but on **constructed** interference, where 97% of
runs interfered; on its mined set of 834 runs over 417 real Django pairs,
exactly one run interfered, and the authors state outright that the constructed
rates "do not estimate how often these problems occur in practice". So the
notice is a cheap repair *when* interference happens, and the paper says
nothing about how often that is. That is why change notices below are scoped to
declared claim overlap instead of every workspace in a repo.

The two closest system papers disagree about the workspace model, so they
transfer differently. CAID — centralized delegation, isolated workspaces,
branch-and-merge over git worktrees — is the architecture lazybox already has,
and its evidence transfers directly. AgentRoom reaches a comparable result from
the *opposite* model, a CRDT-backed **shared** workspace whose MCP primitives
are file-level claim, status and broadcast, and concludes that "coordination,
not parallelism or CRDT-merge, bears the load" — what transfers from it is the
worth of the claim and status primitives, not the shared filesystem or the
merge strategy. Cognition's rule stands: one writer per scope, everything else
read-only.

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
  id: WorkId                    // uuid v4, daemon-minted at create — NOT lazybox_core::TaskId
  plan: Option<PlanId>          // the TODO tree it belongs to
  parent: Option<WorkId>        // nesting (sub-TODO)
  title, brief                  // brief = objective · done-criteria · boundaries · output shape
  owner: Option<Party>          // who does it; None = unassigned
  requester: Party              // who asked
  links: Vec<Link>              // workspace, issue, PR, URL — the work it points at
  lifecycle: Pending | Underway | AwaitingAnswer{question} | Held{reason}
             | Completed | Failed | Canceled
  result: Option<Result { summary, artifacts: Vec<ArtifactRef>, outcome }>
  history: Vec<Event>           // who changed what, when (provenance)
}
```

`WorkId` is a fresh uuid, and deliberately **not** `lazybox_core::TaskId`. That
existing type is `{ source, key }` rendered `github:owner/repo#N` — minted by
the provider, not the daemon — and it does not identify a unit of work across
its life: at the issue→PR fold the addressing key becomes
`workspace_key_for(&pr_task)`, after which the daemon hand-rebadges terminals,
history, blocking and MCP tokens one by one. Using it as the primary key would
re-import the exact migration burden this pillar exists to delete. The tracker
records belong in `links` instead, so a fold rewrites a link and never an id.
The id must also be injective under `sanitize_key` — which maps every
non-alphanumeric byte to `_` — because kv keys are derived from it: a uuid is,
and `github:my-org/tools#42` is not (that collision *is* #1836).
`SessionId(Uuid)` in `core` is the precedent to copy.

`lifecycle` maps onto A2A's `TaskState` — `Pending`/`submitted`,
`Underway`/`working`, `AwaitingAnswer`/`input-required` as a first-class
interrupt, and terminal states that reject further work — so an A2A bridge
later is a mapping, not a redesign. (`Held` is lazybox's own, an
operator-owned blocker; at a bridge it degrades to `input-required` carrying
the reason as the question.)

The names avoid `Working`, `InputNeeded` and `InProgress` on purpose, because
lazybox already has two other state enums and a third must not be confusable
with either. `AgentState` (`crates/ipc/src/lib.rs`) is liveness **observed**
from the PTY and Claude's lifecycle hooks. `lazybox_core::TaskState` (`Open |
InProgress | InReview | Closed | Merged | Draft`) is the **tracker record's**
state: it belongs to the issue or PR in `links` and is owned by the provider,
not by us. `lifecycle` is the third and distinct thing: intent **declared** by
whoever owns the task. `lifecycle` and `AgentState` disagree routinely and
neither is wrong when they do — an agent parked at a permission prompt is
`AgentState::InputNeeded` while its task is legitimately `Underway`, and an
agent whose turn has ended is `Done` while its task stays `Underway` until it
reports. Two rules settle which is authoritative for what. The delivery gate
reads **`AgentState`**: of the two it is the only one that knows whether a turn
is in flight. And `lifecycle` never transitions on an `AgentState` change alone
— with one exception that must not be silent: `AgentState::Exited` under a task
still `Underway` marks it `Failed` with provenance `agent-exited`, rather than
leaving a dead task `Underway` forever or quietly calling it `Completed`.

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
- **A plan with linked items in several repos *is* a local epic**, backed by
  the existing `EpicRecord`, so `epic_status`, blockers, the ready queue and
  merge-after keep working — no second concept beside epics. That mapping has
  one condition, because `EpicRecord` is *not* an explicit-members list: its
  membership is the union of three sources — the `anchor`'s transitive
  sub-issue chain, the explicit `members`, and a read-side `epic:<key>`
  label — **re-resolved on every poll**. So a plan is an epic with
  `anchor: None` and explicit members, and a TODO item's `link` to an issue
  stays an inert link: it never becomes the anchor. Anchoring is a separate,
  deliberate act ("track this GitHub epic"), and it is what opts a plan into
  the sub-issue sweep — after which every workspace whose parent chain reaches
  that issue is a member, including rows the user never added. Detaching is
  therefore part of anchoring: an anchored plan needs an exclusion list,
  because removing a swept member without one only removes it until the next
  poll.
- The same tree is what agents read and update (below), so the user and
  the agents share one plan.

### Party and the one delivery path

`Party` has exactly three variants, and this is its only definition:

```text
Party = Human
      | Agent { workspace: WorkspaceKey, session: Option<SessionId> }
      | Lazybox
```

An agent is addressed by its **workspace**; `session` rides along only as
provenance — which session was live when this was written. Unassigned is not a
variant: that is `owner: None`. `Lazybox` is a legal `requester` and never an
`owner`, so work lazybox starts by itself (auto-fix, a resume) is owned by the
session it spawns and "who do I chase" always resolves to a workspace.

"lazybox talking to an agent" (a `w w` work prompt, auto-fix, resume) and "an
agent talking to an agent" (ask, handoff, notify) become the same operation:

```text
deliver(to: WorkspaceKey, work: WorkId, kind: Assign | Message | Resume)
  -> DeliveryReceipt { accepted | deferred(reason) | refused(reason) }
```

Addressing the **workspace** rather than a `SessionId` is the one thing today's
bus already gets right, and it has to survive this rewrite: `ask_session`
resolves `running_agent_terminal(&target)` from the workspace key, so it
reaches whichever agent is live *now*. A `SessionId` is a per-session
`Uuid::new_v4()`, and sessions are replaced constantly — `Shift-K` stops every
limit-blocked agent and respawns the same conversation in the same pane, and
auto-fix, `a c` and credit recovery do the same. Had `deliver` taken a
`SessionId`, one `Shift-K` would leave every assigned task addressing a session
that no longer exists: a dead pointer traded for the mutable key this proposal
set out to remove. Ownership therefore survives a respawn by construction, and
the rule for the provenance field is explicit rather than implied — on spawn
the daemon stamps `session` with the new id and appends a `reassigned` history
event.

One owner (the #1688 extraction target) holds the gate: never paste into a
mid-turn agent — `AgentState::Working`, which today is not gated — wait out a
chooser, dedupe per terminal, confirm the submit, and **report back to the
requester as a task event** rather than a TUI toast. The seven current paths
become callers of it. `PromptSource` gains the requesting `Party`, so history
shows who sent what; since `PromptSource` is an IPC wire type
(`crates/ipc/src/lib.rs`) carrying `ts_rs::TS` under `desktop-contract`, that
edit regenerates the desktop contract too — see [Phasing](#phasing).

### Results and the Stop hook

An agent finishes a task by calling `complete_task(id, summary, artifacts,
outcome)`. For agents that won't, lazybox falls back to the **Stop hook's
content and transcript** (no longer discarded) and then to scrollback, and
marks the result's provenance accordingly. `reply_request` becomes one case
of `complete_task`; `answered_by_capture` becomes the last-resort fallback
instead of the mechanism — and the race where a turn already in flight
answered a new question goes away because delivery no longer pastes into an
`AgentState::Working` agent.

Artifacts are the existing `.lazybox/artifacts/` spool (#1822) plus
daemon-stored blobs, addressed by reference so a result costs the requester
a few hundred bytes, not a transcript.

### Subscriptions

`subscribe(scope)` where scope is a task, a plan, a session or a repo. First
event: the current snapshot. Then typed deltas: task lifecycle changes,
results, blockers, and **change notices** — "PR #N merged into `repo`, touching
these paths, base `sha`". A notice goes to a workspace whose declared claim
(below) overlaps the merged paths, plus the workspaces on the same plan — not
to everything in the repo. That scope is the honest reading of the 82% result
above: the notice is worth having because it repairs interference cheaply,
while the frequency that would justify waking every sibling in a repo was never
measured outside a constructed setting. For MCP clients this rides a long-poll
tool (`wait_for_events(cursor, timeout)`) until MCP notifications are usable
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

Today `lazybox_session_context_with_mcp("")` measures 6359 bytes over 30
lines — 4124 for the base briefing plus 2233 for the MCP coordination
paragraph — and the guard in `agents/src/session_context.rs` caps it at 6600
bytes / 37 lines. On top of that every Claude session carries ~1.1 KB of
standing rules and the descriptions of ~25 coordination tools, whether or not
it ever coordinates. Codex, Cursor and generic agents get less and have no MCP
at all.

Proposed tiers:

| Tier | Contents | Budget |
|---|---|---|
| **Always-on** | identity (work id, workspace, repo, role), standing rules, one line: "lazybox context: call `lazybox_guide`" | ~1.5 KB, enforced by *replacing* today's 6600-byte cap, not by a second guard beside it |
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

   **Status.** The user-visible half is #1898: `TodoItem` as a field on
   `Workspace` (schema v15), progress roll-up, and auto-check on merge. That PR
   flags in its own body that a checklist hanging off a workspace cannot be the
   shared plan phases 3–4 subscribe to, and records the decision that the store
   is a follow-up which must land before phase 3 starts.

   The store's **model** is `lazybox_core::work`: `WorkId`/`PlanId` (uuid,
   sanitizer-safe), `Party`, `Link`, `Lifecycle` with terminal states that
   refuse further work, `Task` with provenance history, progress roll-up over
   the tree, auto-completion by link, and `plan_members` for the
   `anchor: None` epic projection.

   The store is now **wired** (`crates/server/src/work_store.rs`): rows persist
   under the `work:` / `plan:` kv prefixes, multi-row writes go through
   `apply_batch` so a roll-up never half-lands, and an undecodable row is
   skipped and counted rather than failing a listing. Four agent-facing verbs
   ship with it — `create_work`, `my_work`, `update_work`, `work_status` —
   named `work` rather than the doc's `task` because `task` is already the tool
   that reads the *tracker record*, which is exactly the confusion the model's
   own naming rule exists to avoid. Phase 3's collapse to ~8 tools must subsume
   these four rather than add to them.

   Three automatic transitions are wired:

   - **Auto-check on merge/close** rides the one call site #1898 already had
     (`workspace::check_todo_items_linked_to`), so the per-workspace checklist
     and the plan rows can never disagree about whether a record landed.
   - **A stranded task fails.** `AgentState::Exited` is deliberately *not* the
     trigger: lazybox replaces agents constantly and on purpose (`Shift-K`,
     auto-fix, `a c`, credit recovery), and each replacement is an exit
     followed by a spawn, so failing on teardown would fail the work of every
     agent lazybox itself restarted. A one-minute sweep fails `Underway` work
     whose owner workspace has had no live agent for `STRANDED_GRACE` (10
     minutes) instead.
   - **A result reaches its requester.** Completing work another agent asked
     for delivers a short notice through the one delivery owner, so the
     requester does not poll.

   Still open from phase 1: #1898's `todo_items` has **not** been folded into
   these rows — that fold is the second migration of the same data #1898
   already called out, and the TUI's TODO list still reads the `Workspace`
   field. The CLI twin for non-MCP agents is phase 3's.
2. **Delivery + results.** The single delivery owner with receipts;
   `complete_task`; keep the Stop hook's content; tasks for `w w`, ask and
   auto-fix; fix the ask race and per-repo rules as part of it. Adding the
   requesting `Party` to `PromptSource` edits `crates/ipc/src`, so this phase
   also regenerates the desktop contract (`make desktop-contract`) and checks
   `apps/desktop/src-tauri` against its own lockfile — that crate is a separate
   cargo workspace, so a root `--workspace` run does not cover it.
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
- Does `claim` gate edits (refuse) or only warn on overlap? Genuinely open —
  the cited work runs both ways and neither result is strong enough to settle
  it. AgentRoom gets its gains from claim + status + broadcast with no refusal,
  while Claim Plane treats concurrent modification as a *pre-write admission*
  problem — declared regions, overlaps serialized, "fails closed on ambiguous
  authority" — on a six-pair evaluation its own author calls "intentionally too
  small for comparative claims". Warn-and-show is the cheaper first cut and the
  one assumed elsewhere in this doc; that is an assumption, not a finding.
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
