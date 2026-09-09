# Orchestration: cross-repo epics with live status

**Status:** investigation + scoping (2026-09-07). No code.
**Thesis fit:** this is *the control surface gaining a map and a memory*,
not lazybox becoming an orchestrator — see [`positioning.md`](positioning.md).
The three earlier orchestration proposals (#1173, #1214, #1323) were closed
as not-planned on 2026-09-03 with that doc as rationale; this one is scoped
to stay on the human-in-command side of the line.

---

## 1. The problem, precisely

> I build large epics that span repos, and it is very difficult to keep track
> of every dependency and the order. I ask "give me status" a lot.

Three distinct pains hide in that sentence:

1. **The graph is nowhere.** The plan (which task blocks which, in which
   repo, in what order) exists only in the planner's head, a prose issue
   body, or a coordinator agent's context — and that context is stale the
   moment the agent turns.
2. **Status is asked, not shown.** "Give me status" today means a coordinator
   agent runs a dozen `gh` / `list_sessions` calls, reads terminals, and
   *summarizes*. It is slow, costs tokens, drops caveats, and is out of date
   as soon as it prints. Yet the daemon already knows every agent state, PR
   CI/review state, and claim in real time.
3. **Roles are implicit.** The coordinator, the workers, the reviewer, and
   whoever integrates are all "a workspace with a Claude in it". Nothing in
   the UI or the prompt says what each is *for*, so nothing can enforce it
   or route by it.

The requirement that reshapes the design: **status must be derived from
daemon state with no model in the loop, live, like the inbox** — and a
coordinator that *is* asked should answer from one structured call with a
"what changed since you last asked" delta, not a re-scan.

## 2. State of the art (September 2026)

### 2a. What people use

| Tool | Dependency model | Roles | Cross-repo | UI | Live status | Source of truth |
|---|---|---|---|---|---|---|
| **Beads** (`bd`) | Full DAG, `blocked-by`; `bd ready` withholds blocked work | none | per-repo ledgers, prefix namespacing | CLI | poll | Dolt DB |
| **Gas Town** (on Beads) | inherits DAG; "convoys" bundle work per agent | Mayor / Polecats / Crew / Deacon / Refinery | "Rigs" wrap repos | `gt feed` TUI + web dashboard | event stream + patrol heartbeats | git "hooks" + Beads |
| **OpenAI Symphony** | delegated to Linear (one issue = one agent) | planner (issue author) / worker / human reviewer | Linear board spans repos | Linear kanban | polls the board, restarts stalled agents | Linear |
| **Devin managed sessions** | in-memory plan, no store | parent Devin / child Devins | not documented | chat | parent reads child trajectories | proprietary |
| **Agent Orchestrator** (aoagents.dev) | plan-based, no documented DAG | orchestrator agent + workers | fleet of workspaces | Electron + CLI + mobile | SSE from Go daemon | local daemon |
| **Taskmaster / Backlog.md / Shrimp** | dependency lists, `next_task`, cycle checks | none | no | CLI/MCP, kanban | pull | JSON / markdown in repo |
| **Conductor, Vibe Kanban, Crystal→Nimbalyst, Claude Squad, Superset, Emdash, Mux** | none (flat list / kanban) | none | one repo per app (Emdash ingests tickets from Linear/GitHub/Jira) | dashboard / kanban / tmux | poll | local app DB + worktrees |
| **Claude Code Agent Teams / subagents, Cursor parallel agents, Codex app** | none — sequencing is the orchestrator prompt's job | lead + peers (implicit) | no | chat / dashboard | dashboard | in-session |

Sources: Beads, Gas Town, Symphony, Devin, Conductor, Vibe Kanban, Emdash,
Taskmaster READMEs and launch posts; the
[awesome-agent-orchestrators](https://github.com/andyrewlee/awesome-agent-orchestrators)
roundup. Terragon has shut down; Vibe Kanban is community-only now.

### 2b. Patterns that recur

- **Worktree isolation is universal** and orthogonal to ordering. lazybox
  already has it.
- **Dependency DAGs live only in agent-native trackers** (Beads, Taskmaster);
  every workspace-manager UI is a flat list or kanban.
- **Roles appear only where an orchestration layer sits above the workspace
  manager** (Gas Town, Devin, Symphony). Each invented its own vocabulary.
- **Source of truth is either a purpose-built local DB or a repurposed PM
  tool.** Symphony's bet — *the tracker is the control plane* — is the one
  most compatible with lazybox's source-agnostic thesis.
- **Live status is polled or agent-summarized almost everywhere.** Gas
  Town's `gt feed` and AO's SSE daemon are the exceptions. lazybox's
  broadcast bus is already the right shape for the exception.
- **Merge order is a separate concern** (Gas Town's Refinery, Symphony's
  review step), per-repo, never coordinated across repos.

### 2c. Gaps nobody fills — the open ground

1. **No tool shows a cross-repo dependency DAG with live execution order.**
2. **No tool coordinates merge order across repos** in one epic.
3. **Status is not a verifiable derived view**; it is a summary.
4. **Role taxonomies are ad hoc**; none is bound to prompts and permissions.
5. **Tracker ↔ agent-tracker reconciliation is one-way or manual.**

lazybox is unusually well placed for 1–3 because the daemon already sees
every repo, every agent, every PR, and every claim in one process.

### 2d. Dependency primitives we can stand on

| System | Primitive | Cross-repo? | Read | Write | Notify |
|---|---|---|---|---|---|
| GitHub | **Issue dependencies** (`blocked_by` / `blocking`, GA 2025-08) | **No** — same repo only | REST `…/issues/{n}/dependencies/blocked_by` | REST POST; GraphQL `addBlockedBy` | `issue_dependencies` webhook |
| GitHub | **Sub-issues** (`parent`, `subIssues`, `subIssuesSummary`) | **Yes**, explicitly | GraphQL / REST | `addSubIssue` | `issues` webhook |
| `gh` CLI ≥ 2.94 (2026-06) | both of the above | as above | `gh issue view --json parent,subIssues,…` | `--blocked-by`, `--blocking`, `--parent`, `--set-parent` | — |
| Linear | `IssueRelation` (`blocks`, `related`, …), parent/child, Project → Initiative (5 levels) | intra-workspace, cross-team | GraphQL | `issueRelationCreate` | webhooks |
| Linear | **Agents API**: `AgentSession` + append-only `AgentActivity` | — | GraphQL | agent posts activities (10 s first-response SLA) | session webhooks |
| Jira | Issue links (`Blocks`), Advanced Roadmaps dependencies | cross-project | REST `issueLinks[]` | `POST /issueLink` | `issuelink_*` webhooks |

Two consequences that decide the design:

- **GitHub cannot express a cross-repo "blocked by" natively.** Sub-issues
  cross repos; dependencies do not. A cross-repo GitHub epic therefore needs
  an overlay for its blocking edges.
- **Linear's `AgentActivity` is the closest existing analog to what we want
  for status**: an agent-writable, append-only feed bound to a task record.
  Ours should be *derived* rather than agent-written, but the shape is
  right, and we can mirror into Linear's feed later.

### 2e. What practitioners converge on

- One agent per repo/worktree, a human or lead deciding *what lands and in
  what order*. Merge sequentially, rebase the rest after each landing.
- A living spec / plan document as shared truth; **contract-first** (agree the
  API shape before the repos move).
- Coordinator / specialist / verifier separation reduces duplicate work.
- Peer-to-peer contract messages (backend agent tells frontend agent the
  shape) beat routing everything through a lead. (Our blackboard, exactly.)
- Reported failure modes: duplicate work on the same task; decisions made on
  stale status; permissive planners spawning too many workers; and "many
  parallel agents without dependency mapping" named as a top failure by
  Cursor's own guidance.

## 3. What lazybox already has (inventory)

| Piece | Where | Reusable as |
|---|---|---|
| `Task.parent` (Linear + Jira fill it; **GitHub hard-codes `None`**) | `core/src/task.rs:631`, `gh-provider/src/graphql.rs:2709,3753` | the hierarchy edge — needs the GitHub producer |
| Collapsible ticket hierarchy (#1189): forest build + indent render | `tui-core/src/inbox/mod.rs:377-400`, `workspace_row.rs:737-754` | tree UI for an epic's members |
| Stacked-PR graph (`parent`/`children`/`depth`, `⇗n/N`) | `core/src/stack.rs:30-108` | a shipped intra-repo dependency render |
| Spaces (#860): cross-repo group tier, header row, collapse, right-pane overview | `config/src/lib.rs:1018`, `inbox/model.rs:225`, `repo_overview.rs` | the Epic tier's shape |
| Group overview pane (#1442): counts, ranked roster, per-repo rollup | `tui/src/components/repo_overview.rs` | host for the epic overview |
| Working claims (15-min heartbeat, 1-h TTL) + watchdog | `core/src/workspace.rs:42-53`, `server/src/working_claims.rs` | mutual exclusion for dispatch |
| SpawnCoordinator (per-workspace mutex, inflight dedup, inject gate) | `server/src/registries.rs:1462-1500` | safe fan-out |
| Label / mention spawn (`lazybox:<agent>` label, `@lazybox`) | `gh-provider/src/mentions.rs:169`, `polling/sources/mod.rs:581` | label-driven bulk start |
| Policy latches (`AutomationPolicies`, `auto_merge_on_green`) | `core/src/policy.rs:362-404` | the autonomy dial's mechanism |
| `designissues` / `carve` snippets: already ask for a tracking issue, a dependency graph and a sequence | `config/src/snippets.rs:1012-1097` | the **planner role prompt** — output is prose today |
| kv store (`set_kv` / `list_kv_prefix`), namespaces like `lazybox:note:`, `terminal-working-claim:` | `store/src/traits.rs:204-230` | `epic:*` records need no schema change |
| MCP bus: `whoami` / `list_sessions` / `read_session` / `post_note` / `read_notes` / `notify_session` | `server/src/mcp.rs` | coordinator ↔ worker channel; **no spawn / create tool yet** |
| JSON gateway `/v1/commands` (`CreateWorkspace`, `Spawn`), `lazybox workspace create --agent` | `api_gateway.rs:1921`, `tui-boot/src/main.rs:686` | the only way an agent can start a sibling today |
| Coordinator-session sketch (`fleet_parent`), #1173 Pillar B (implement → review → fix as fresh agents) | `docs/coordinator-session.md`, #1173 | prior art for roles |

The gap is narrow and specific: **an edge producer, an Epic record, a
derived status, and a role field** — everything else is a re-pointing of
shipped machinery.

## 4. Design

### 4a. Concepts

- **Epic** — a named, cross-repo set of *members* (tasks and/or workspaces)
  with a **dependency DAG** and a **role map**. Anchored to a tracker record
  when one exists (a GitHub parent issue, a Linear project) so the tracker
  stays the source of truth for membership and hierarchy.
- **Edge** — `Blocks(a → b)` (b may not *start* until a is done),
  `MergeAfter(a → b)` (b's PR may not *merge* until a's is merged), and
  `Contract(a → b)` (b consumes an interface a publishes; satisfied when a
  note tagged `epic:<key> contract` exists on the blackboard).
- **Role** — `Coordinator | Planner | Worker | Reviewer | Integrator`, stored
  per workspace. A role changes the spawn prompt preamble, the sidebar badge,
  which MCP tools the session is *told* to use, and (for Coordinator) whether
  it may spawn siblings.
- **Status** — *derived*, never stored: per member
  `Blocked(by …) → Ready → Claimed → InProgress → Asking → PrOpen{ci,review}
  → Mergeable(held: merge-after …) → Merged/Done | Failed`; per epic:
  counts, ready queue, critical path, stalled reason, merge order.

### 4b. Source of truth: tracker where it can be, overlay where it can't

| Fact | Truth | Notes |
|---|---|---|
| Membership + hierarchy | Tracker: GitHub **sub-issues** (cross-repo), Linear parent/project, Jira epic | lazybox reads it; `designissues` already creates it |
| Same-repo blocking | Tracker: GitHub issue dependencies, Linear `blocks`, Jira `Blocks` | lazybox reads it; agents write it with plain `gh issue edit --blocked-by` |
| **Cross-repo blocking on GitHub** | **Overlay**: a `Blocked by: owner/repo#N` marker line in the issue body, parsed like `Closes #N` is today, *plus* the kv record | Portable, agent-writable with `gh`, visible in GitHub; lazybox mirrors it to native deps when both ends are in one repo |
| `MergeAfter`, roles, ordering overrides, epics with no tracker | lazybox kv `epic:<key>` | execution metadata, lazybox-only by nature |

Write-through is one-directional and safe: when lazybox (or a user in the
UI) adds an edge the tracker can hold, lazybox writes it upstream; when it
can't, the body marker is the fallback. An agent never needs a lazybox-
specific write path for the graph — `gh` is enough, which honors the
agent-autonomy principle in `CLAUDE.md`.

### 4c. Creating an epic

1. **From a tracker parent** — `E n` on an issue with sub-issues (or a
   Linear project): lazybox walks `subIssues` transitively across repos,
   reads dependencies and body markers, and materializes the Epic.
2. **From the planner role** — spawn a Planner (`E p`) with the `carve` /
   `designissues` brief plus one new instruction: *use `--parent` and
   `--blocked-by` / `Blocked by:` markers so the graph is machine-readable.*
   Lazybox picks it up on the next poll and shows the DAG. The planner's
   prose stays; the structure becomes real.
3. **Ad hoc** — `E a` adds the cursor row or the multi-select to an epic;
   `E d` adds a blocking edge by picking the blocker from a picker.

Issue→PR fold keeps membership (the existing `closes_issues` /
`linked_tasks` join), so the graph follows the work into its PR.

### 4d. Sidebar

A new top-tier group **`◈ Epic: <name>`** above repo headers (the Space
shape), rows ordered by **topological wave**, each row carrying:

```
◈ Epic: auth-refactor           3/9 done · 2 ready · 1 blocked · 1 asking
  w1 ✓ api      #842 token schema                 ◆ coord  ⇗
  w1 ✓ api      #843 issue JWT                    ⚙ worker
  w2 ▶ web      #311 consume /auth/token           ⚙ worker   ready
  w2 ● mobile   #907 login flow                    ⚙ worker   Working 12m
  w2 ⏸ sdk      #144 client bindings               ⚙ worker   asking!
  w3 ⛔ docs     #622 auth guide           blocked by web#311, sdk#144
  w3 ⛔ infra    #77  rotate secrets       merge-after api#843 (mergeable, held)
```

Glyphs: `⛔` blocked, `▶` ready, `●` in progress, `⏸` asking, `✓` merged,
`✗` failed; `w<N>` wave; role badge. The header line **is** the status.

Keys (all under one bare `E` **epic** leader — `Shift-E` is already the
Error Inbox and bare `E` was verified unbound; which-key popup):
`E n` new · `E a` add · `E d` add dependency · `E r` set role · `E s` status ·
`E m` merge order · `E g` graph view · `E p` spawn planner · `E c` spawn
coordinator · `E x` archive · `E j` jump to the next member that needs you
(asking → failed → ready → blocked), mirroring `!` / `Shift-F`. Filters gain
`epic:<name>`, `ready`, `blocked`.

### 4e. Right pane: the epic overview (the answer to "give me status")

Cursor on the epic header → `OverviewKind::Epic` in the existing overview
pane:

- **Counts strip**: done / in progress / ready / blocked / asking / failing.
- **Ready queue** (ranked: unblocks the most downstream first).
- **Blockers**: what is holding the most work, with the reason.
- **Critical path** and **merge order** (topological order of `MergeAfter`).
- **Per-repo rollup** (reuses `RepoRollupRow`).
- **Recent epic events** (last 10, see 4f).
- An **ASCII DAG** by wave — columns are waves, `─┬─` fan-outs; `E g`
  opens it full-screen, `j/k` moves, `Enter` jumps to the workspace.

Nothing here calls a model. It re-renders on every daemon event.

### 4f. Real-time: status is an event stream, not a question

- The daemon keeps an `EpicResolver` that recomputes status on every
  relevant event — agent state change, PR poll (CI / review / merge), claim
  change, edge change — and emits `Event::EpicStatus { key, snapshot, delta }`
  on the broadcast bus. TUI, desktop, and the JSON gateway's `/v1/events`
  all get it for free.
- **Epic events enter the reactive inbox** as activity rows with
  read/unread: *"web#311 unblocked (api#843 merged)"*, *"epic stalled: all
  remaining members blocked on external"*, *"merge-order: infra#77 is
  mergeable but held behind api#843"*, *"sdk#144 asking"*. This is the
  fleet-scoped inbox the coordinator sketch called the differentiator.
- **MCP `epic_status(epic?, since?)`** returns the same snapshot plus the
  delta since a cursor. The coordinator's session briefing says: *answer
  "give me status" with one `epic_status` call, never by crawling `gh`.* Its
  reply becomes a diff — what changed, what's ready, what's stuck — in one
  turn.
- **MCP `epic_ready(epic)`** lists ready members so a coordinator (or a
  worker finishing) can pull the next task Beads-style (`bd ready`).
- Desktop notification hooks: "ready queue non-empty and an agent is idle",
  "epic complete", "merge-order violation".

### 4g. Roles

| Role | Prompt preamble (`core/src/prompts.rs`) | May | Badge |
|---|---|---|---|
| Planner | the `carve` / `designissues` brief + machine-readable graph instruction | write issues + edges via `gh` | `✎ plan` |
| Coordinator | "you own epic X; use `epic_status` / `epic_ready` / `notify_session`; do not code; spawn workers with `spawn_worker`" | **`spawn_worker`** (new MCP tool → `CreateWorkspace` + `Spawn`, the gateway path made tool-shaped) | `◆ coord` |
| Worker | task brief + *your blockers are done: …* + *contracts for this epic are on the blackboard, tag `epic:X`* | post notes, notify siblings | `⚙ worker` |
| Reviewer | diff + the issue's DoD checklist; emits findings as a note tagged `review` | `read_session`, notes | `👁 review` |
| Integrator | merge order + `MergeAfter` gates; "land in this order, rebase the rest" | `g m` per member in order | `⇅ integ` |

Roles are a field on `Workspace` (`role: Option<Role>`, next to `hopper` /
`remote`, the shape those took), set at spawn from the epic action or later
with `E r`. The #1173 Pillar-B pipeline (implement → review → fix as fresh
agents) becomes "a member whose DoD includes a Reviewer pass", not a new
engine.

### 4h. Execution and the autonomy dial

Earn it in three notches, each an existing latch shape:

1. **Manual** (ship first): lazybox surfaces *ready*; you start the next ready member yourself (`w w` / `a c` on it; `E s` shows the status view).
2. **Assisted**: an `AUTO` pill on the epic: when a member becomes ready and
   the concurrency cap allows, spawn a Worker (first time confirmed). Uses
   working claims for exclusion and the SpawnCoordinator for safety. This
   is `lazybox:<agent>` label-spawn re-pointed at "ready", not new code.
3. **Auto**: plus **merge-order gating on merge-on-green** — a mergeable PR
   with an unmerged `MergeAfter` predecessor is *held*, shown as such, and
   merged when the predecessor lands; plus an automatic Reviewer stage. This
   is the one genuinely new gate, and it closes the "no one coordinates
   merge order across repos" gap.

Guardrails carried over: the working-claim TTL prevents double-spawn, the
"start N agents?" confirm bounds fan-out (the cost blow-up practitioners
report), and the `no-auto-fix` / `do-not-lazybox` labels keep their meaning.

### 4i. Contracts across repos

Contract-first is what practitioners converge on and what the blackboard
already supports. A `Contract(a → b)` edge is satisfied by a note on the
blackboard tagged `epic:<key>` + `contract` from a's session; b's Worker
brief quotes it. The Reviewer checks the implementation against it. No new
storage — `post_note` / `read_notes` with tags, which exist.

### 4j. Labels: the visible projection

lazybox already treats GitHub labels as live coordination state (`working`,
`lazybox:w:*`, `lazybox:<agent>`), which is what makes the fleet legible to
anyone looking at GitHub rather than at lazybox. The epic gets the same
treatment, so the plan is visible from GitHub, from a Linear/Jira mirror, or
from any other tool:

| Label | Direction | Meaning |
|---|---|---|
| `epic:<key>` | read **and** written | membership — a third membership source next to the anchor's sub-issue chain and explicit assignment, so a planner or a human can add a member from GitHub alone; written on assign, removed on unassign |
| `lazybox:ready` / `lazybox:blocked` / `lazybox:done` | written only | the derived member status, mutually exclusive, written **only when it changes** and only when the epic opts in (`publish_status_labels`) — zero API calls on a quiet poll |
| `role:<role>` | read and written | the workspace role (P2); the persisted field wins when both exist |
| `wave:<n>` | deferred | waves shift whenever an edge changes and would churn labels |

Two rules keep this honest. Labels are **a projection and a membership
hint, never an input to the status resolver** — otherwise a stale label from
a dead daemon would freeze status. And label writes go through add/remove,
never replace, because the `working` claim labels live on the same issues.

### 4k. Blockers are first-class, not just edges

"Give me status" is usually "what is blocked, on what, on whom, since when".
The structural sources above (an open dependency, an external task, an
agent asking, a held merge, a cycle) cover only the blockers lazybox can
infer. The one it cannot infer is the **declared** blocker — "blocked on a
decision about token expiry", "needs the Stripe key" — which is exactly the
one a human must act on. So a blocker is its own record on every member:

```
Blocker { kind: Dependency | External | Decision | Credential | Review |
                MergeOrder | Contract | Cycle | Other,
          reason: String, owner: Operator | Agent(key) | External(name),
          since: unix-ms (stable across recomputes and restarts),
          holds: u32 (transitive dependents it holds) }
```

Three ways to declare one, all cheap: an MCP `report_blocker(reason, kind?)`
/ `clear_blocker` for agents (identity from the bearer, so a worker reports
its own wall instead of idling — the session briefing says so); a
`Blocked on: <text>` body-marker line for humans and planners, parsed by the
same module as `Blocked by:`; and a `blocked:<kind>` label as the visible
projection (§4j rules apply).

Blockers lead everywhere: first in the epic header line (`⛔ 2 blocked!`
when the operator owns one), the first section of the overview (sorted by
how much work each holds, then by age, operator-owned rows bold), first in
the `E j` sweep, `BlockerAdded` / `BlockerCleared` in the status delta and
the activity feed, and a one-shot desktop notification when an
operator-owned blocker is older than `epics.blocker_alert_after` (default
4 h). Linear's native Blocked workflow state and GitHub's label are the
mirrors.

## 5. Tooling we can use with lazybox

| Tool | Use |
|---|---|
| **GitHub sub-issues + issue dependencies**, via `gh` ≥ 2.94 and GraphQL | primary graph truth for GitHub epics; agents write with `gh issue edit --parent / --blocked-by` |
| **Linear relations + projects/initiatives**; **Linear Agents API** | graph truth for Linear epics; later, mirror our derived status into `AgentActivity` so Linear shows what lazybox knows |
| **Jira issue links** | graph truth for the Jira provider (already reads epic link) |
| **Beads (`bd`)** | an optional **provider**: read a repo's `.beads` ledger as tasks + edges for teams already on it; `bd ready` semantics are exactly our Ready state |
| **Taskmaster `tasks.json` / Backlog.md** | importers → an Epic with edges (cheap, file-based) |
| **Gas Town** | reference architecture for roles and the event-feed dashboard, not a dependency |
| **Symphony's `SPEC.md`** | reference for "tracker as control plane" + stalled-agent restart policy |
| Graph rendering | TUI: hand-rolled layered (wave) layout, consistent with the hand-rolled markdown; desktop: `dagre`/`elk` |

## 6. Phasing (each slice dogfoodable)

Tracked as epic #1517 with sub-issues #1521 (P0) → #1522 (P1) → #1523 (P2)
→ #1524 (P3) → #1525 (P4), chained with GitHub sub-issue + blocked-by
relations so the epic renders as an epic in lazybox once P0 ships.

| Phase | Delivers | New state | Answers |
|---|---|---|---|
| **P0 — edges in the inbox** | GitHub provider fills `Task.parent` from sub-issues and reads dependencies + `Blocked by:` markers; Linear reads `blocks`; `Blocked on:` declared blockers; rows get `⛔ blocked by N` / `▶ ready`; `ready` / `blocked` filters; `E j` jump | none (Task fields) | "what can I start right now" |
| **P1 — Epic + live status** | Epic record (kv), sidebar tier from a tracker parent, header status line, overview pane, `EpicResolver` + `Event::EpicStatus`, epic events in the inbox, MCP `epic_status` / `epic_ready`, coordinator briefing, blockers as records + `report_blocker` (§4k), `epic:*` + status labels (§4j) | `epic:<key>` | **"give me status"** without a model |
| **P2 — roles** | `Workspace.role`, badges, role prompt preambles, `E r`, `spawn_worker` MCP tool for Coordinators, Planner spawn with machine-readable-graph instruction, `role:*` labels | `role` field | who does what, enforced |
| **P3 — graph + merge order** | full-screen DAG, `MergeAfter` edges, merge-order readout, merge-on-green hold | edge kinds | landing order across repos |
| **P4 — autonomy dial** | `AUTO` / `REVIEW` / `ORDER` latches (assisted dispatch on ready, Reviewer stage, epic-wide merge-on-green), `Contract` edges | policy latch on the record | the fleet runs the plan; you triage |

P0 has value with no new concept at all and de-risks the providers. P1 is the
one the "give me status" habit needs; it should be the first thing dogfooded.

**P1 delivery (#1522) split at the daemon/client boundary.** The first PR
ships the *daemon foundation* — the part that has to exist before any client
can render an epic: the `EpicRecord`/`EpicKey` kv record (`core/src/epic.rs`),
the `UpsertEpic` / `AssignEpic` / `ArchiveEpic` commands and the
`EpicResolver` that derives an `EpicSnapshot` + `EpicDelta`s pushed as
`Event::EpicStatus` (`server/src/epics.rs`), blocker records with
`report_blocker` / `clear_blocker` (§4k), the `epic_status` / `epic_ready` /
`report_blocker` / `clear_blocker` MCP tools (`server/src/mcp.rs`), epic
events landing in the workspace activity feed, and the coordinator briefing
(`agents/src/session_context.rs`). The TUI ignores `Event::EpicStatus` for
now. Deferred to a follow-up PR (the step-5 client boundary): the sidebar
epic tier, the header status line and overview pane, the `collapsed_epics`
config, and writing the `epic:*` + status projection labels back to the
tracker (§4j). The desktop protocol version stays at 4 — the desktop DTOs do
not yet consume `EpicStatus`.

**P2 shipped (#1523) — roles.** `Workspace.role: Option<Role>` is a
serde-defaulted, OR-merge-safe field (`core/src/workspace.rs`);
`effective_role()` lets the persisted field win and falls back to the
`role:<planner|coordinator|worker|reviewer|integrator>` project label so a
role set on the tracker is adopted when the field is unset. `E r` sets or
clears it through a Choice modal + `SetWorkspaceRole` command; each role
carries a sidebar badge (`✎ plan`, `◆ coord`, `⚙ worker`, `👁 review`,
`⇅ integ`). A role-stamped spawn gets a **prompt preamble** injected ahead of
its work prompt (`core/src/prompts.rs`, applied in `server/src/spawn_handler.rs`
whenever the spawn resolves a role and the prompt is non-empty). `E p` / `E c`
spawn a Planner / Coordinator on the cursor workspace, carrying the role
*in-band* on the `Spawn` command: `SetWorkspaceRole` and `Spawn` both dispatch
on the daemon's detached lane as independent concurrent tasks, so the preamble
must not depend on the persist landing first — the in-band copy governs the
framing while the separate command still persists the role for the badge and
label projection. The MCP `spawn_worker` tool (`server/src/mcp.rs`) is
Coordinator-only: it creates a workspace, assigns it to the caller's epic as a
Worker, and spawns an agent on a brief, refusing off-role or past the epic's
worker cap (`agents.max_epic_workers`, default 6). Labels are written on
set/clear via `sync_role_label_target` (single `role:*` label converged, never
wholesale-replaced). Roles are advisory except the `spawn_worker` gate — merge
gating/ordering is P3 and automatic dispatch is P4. The epic-header
`<epic>-coordinator` creation path is deferred to where client-side epic-row
rendering lands (the TUI still ignores `Event::EpicStatus`), so the reachable
`E c` target is the cursor workspace.

**P3 delivery (#1524) shipped.** `MergeAfter` edges land on
`Task.merge_after` (`Vec<TaskId>`), parsed from `Merge after: owner/repo#N`
body markers and *implied* by every `Blocks` edge unless
`EpicRecord.implied_merge_after` (default true) opts out — so the common case
("a blocks b" ⇒ b lands after a) needs no extra marker. The `EpicResolver`
topologically orders the epic's PRs and marks each one that is mergeable but
sits behind an unmerged predecessor as **held**; the overview pane and the new
`E m` merge-order modal render that order with held rows flagged. Merge-on-green
respects the hold — an armed workspace whose PR is ready but held is not merged
until its predecessor lands (`epics::held_by`, checked in `merge_pr_task`) —
and a manual `g m` on a held PR surfaces a force-confirm (`Y` merges out of
order, sending `Command::MergePr { force: true }`); the daemon signals the
refusal with the shared `MERGE_HELD_REASON_PREFIX` sentinel on
`Event::PrMergeFailed`'s `reason` so the client can offer the override without a
new wire field. `E g` opens the full-screen DAG (waves as columns, `j/k`·`h/l`
nav, `Enter` jumps, `Esc` closes) over a pure `layout()` in tui-core
(`epic_graph.rs`) so the layering stays testable and ratatui-free. Wire
additions (`Task.merge_after`, held merge status, `Command::MergePr { force }`)
bump the desktop contract; the sentinel const is not a schema change.

**P4 delivery (#1525) shipped — the autonomy dial.** Three latches ride the
epic record as `EpicRecord.policies: EpicPolicies` (`core/src/policy.rs`), each
a `PolicyArm` in the shape of the existing `ARM` / `FIX` policies and **off
until explicitly armed**. Unlike auto-fix, `Default` follows no global switch —
there is no "autonomy" setting to follow — so `Default` and `Disarm` both read
as off and the toggle is a two-state `Arm ⇄ Default` cycle
(`EpicPolicies::toggled`); `Disarm` survives as the decisive off that
`absorb_from` keeps.

| Pill | Latch | What it does |
|---|---|---|
| `AUTO` | `auto_dispatch` | A member becomes `Ready` → spawn a Worker on it (P2 preamble), ranked so the member unblocking the most others goes first, capped at `agent.max_epic_workers` |
| `REVIEW` | `auto_review` | A member's PR turns green with no review → spawn a Reviewer; a `blocking` verdict shows the member `ReviewBlocked` and holds its merge |
| `ORDER` | `merge_in_order` | Every member gets `auto_merge_on_green` armed as its PR opens, so the epic lands itself — P3's merge-after hold supplies the sequence |

Every decision is a pure function over already-loaded data — `plan_dispatch`,
`plan_reviews`, `plan_merge_arming` in `server/src/epics.rs` — and
`run_latches` is the thin async shell that gathers the inputs, calls them, and
performs the effects after the snapshot has been broadcast. Latches read the
snapshot's **standing state**, deliberately *not* the accompanying
`EpicDelta`s: gating on the delta reads as the safer "act only on a
transition" rule and is in fact a dead latch, because `diff` yields no member
deltas either for a first-sight snapshot (every epic after a daemon restart)
or for a policy-only change (arming moves no member's status) — the two
moments a latch most needs to act. Re-entry is safe because each latch carries
its own idempotency (the dispatch marker, the review row, the already-armed
check), and `recompute_all` updates its `last` snapshot under the `EpicMemory`
lock before calling in, so one transition reaches the latches once. Dispatch
runs through the same
`ProviderAction::AutoSpawnAgent` path the `@lazybox` / label spawns use — one
`epic_role` field carries the role in-band and tags the run
`AutonomousTrigger::EpicAuto` — so the SpawnCoordinator collapse, the
working-claim exclusion, and the store-backed `autospawn-epic:<epic>:<member>`
dedup marker all apply unchanged. **The `no-auto-fix` / `do-not-lazybox`
labels are honored as "do not auto-dispatch / auto-review" too**: they have
always meant "lazybox, keep your hands off this row", and unattended dispatch
is exactly that.

**Arming `AUTO` is the confirmation.** The issue asked for the *first spawn*
per epic to confirm; the client-side confirm on the arm delivers the same
promise — you are asked once, with the epic and the cap named, before any
automatic spawn can happen — without needing a daemon-initiated modal (a
mechanism lazybox does not have) that could fire while you are away. Standing
a latch down never asks.

The **Reviewer stage** keys off a persisted `epic-review:<workspace>` row
rather than a PR head sha (which `Task` does not carry): the row opens when the
PR turns green, and is dropped the moment the member stops being green — which
is what makes a re-green after fixes review exactly once more. The row also
records the epic that opened the run, and a verdict is accepted only if it
carries that epic's tag — otherwise the `epic:<key>` tag would be decorative
and any live epic's tag could flip a hold a different epic raised. Because
`review_blocks_merge` is keyed on the workspace alone, `recompute_all` prunes
every row whose workspace no longer belongs to a live epic: without that a
member unassigned, removed, or archived out of its epic would strand a
`blocking: true` row and hold that PR's merge forever, citing an epic that no
longer exists. The Reviewer's
brief instructs it to end with `post_note(tags=["review", "epic:<key>",
"blocking"|"clean"])`; `epics::on_note_posted`, hooked into `post_note` itself,
records the verdict, so the latch reacts at the write instead of polling the
blackboard. A blocking verdict holds the merge on the same terms as an unlanded
merge-after predecessor (`epics::review_blocks_merge`, checked beside
`held_by` in both `auto_merge::on_workspace_committed` and `merge_pr_task`),
and `g m --force` overrides it the same way.

**Contracts** (§4i) land as `EdgeKind::Contract`, parsed from a
`Contract: owner/repo#N` body marker onto `Task.contracts`. Unlike every other
edge, it is satisfied by the producer *publishing the interface* — a blackboard
note tagged `contract` + `epic:<key>` from its session — not by its task
closing, so a consumer starts as soon as the interface is agreed and a producer
that merged without ever publishing still gates. An unsatisfied contract makes
the consumer `Blocked` with `blocked_reason: Some("contract")`, no
`blocked_by` and no `external_blockers`; the satisfied set is read once per
recompute into `LatchInputs` so `resolve` stays a pure function of plain data.
That read is a **single** blackboard scan bucketed by epic tag — scanning per
epic would re-parse every note in the store once per record, and notes are
capped per scope while scopes are not (one per session), so a fleet-sized
blackboard would turn an N-epic recompute into N full scans on the 300 ms
debounce path. The buckets are keyed by epic because a contract published for
one epic says nothing about another epic's interface.

That scan does not *decide* satisfaction, it only **latches** it (#1577). The
blackboard is a rolling buffer — `post_note` prunes each scope to its newest
`NOTES_PER_SCOPE` entries — so a producer that keeps posting evicts its own
contract note, and `global` fills faster still. Re-deriving satisfaction from
the notes alone therefore un-satisfies an interface that was genuinely agreed:
the consumer flips back to `Blocked` long after the fact, and with `AUTO` armed
a Worker already dispatched on it sits behind a blocker nobody raised. So the
first sighting writes an `epic-contract:<epic>:<producer>` row
(`PublishedContract`) and satisfaction reads the rows; the note is only how a
contract arrives. A row is **never dropped because its epic went away** — the
difference between it and the review rows next door, which are a cache the next
green run rebuilds. Once the note is evicted the row holds the only copy of an
interface another agent wrote, so an epic-lifecycle prune destroyed it
irrecoverably. Latching is likewise blind to archival: an interface published
while its epic happens to be archived still has to leave a row, or retention
takes the only other copy and the consumer blocks on `contract` forever with
nothing left to republish it. Liveness gates *satisfaction*, never the record.

Two comparisons decide what a row holds. `published_at` is a **watermark**: the
row never moves backwards, because retention prunes per scope while the scan
reads every scope, so the newest note can be evicted while an older one
survives elsewhere — without the watermark the row silently reverted to the
superseded interface and bumped its revision as though the producer had
republished. Among notes that pass the watermark the **interface text** decides,
not the clock: notes are ordered by `(ts, seq)`, a row carries no `seq`, so a
pure `ts` test dropped a correction posted in the same millisecond as its first
draft.

The one row that *is* dropped is one first seen before its epic record existed.
An epic key is `slugify(name)`, so a later, unrelated epic created under the
same name is the same key and used to inherit its predecessor's interfaces —
satisfying a consumer's edge and quoting another project's spec into its
Worker. `since` against the record's `created_at` tells the two apart (notes
older than the record are ignored for the same reason, or the dropped row would
simply re-latch), and dropping the loser is also the only bound on the rows.
The cost is that re-creating an epic under a name it held before asks each
producer to republish — one `post_note`, and a visible `contract` blocker until
they do, which is the cheap failure next to a Worker silently building against
another project's interface.

Posting a contract latches and recomputes at the write, the recompute gated on
the note naming a live epic — `post_note` holds the blackboard's process-wide
write lock across that call and `recompute_all` spawns agents and calls GitHub,
so an ungated tag let one agent stall every other agent's `post_note`.
The consumer's Worker preamble quotes each producer's latest contract inside an
`<untrusted-content source="agent-authored contract">` fence — a specification
to satisfy, never instructions to follow. It reads the rows too, so a Worker
dispatched after the note aged out is still briefed with the interface rather
than with nothing.

**Surfaces.** `E A` / `E R` / `E M` toggle the latches on the cursor
workspace's epic, and the `g p` policies menu grows an epic section (three
rows, each naming the epic it governs) when the cursor sits inside one. Armed
pills append to the epic readouts' frame titles (`Auth refactor · AUTO ORDER`)
— the sidebar epic tier is still deferred, so those are the epic header rows
this build has. Three desktop notifications fire, each debounced on a state
change: ready work with `AUTO` off and nothing currently Working (latched per
epic, since that one reads a standing state — and keyed on the *absence* of a
running agent rather than the presence of a parked one, so it still fires on a
fresh install where no agent has ever reported a state), an epic completing,
and a review finding blocking findings.

Wire additions: `EpicPolicies` on the record and mirrored onto `EpicSnapshot`,
`Command::SetEpicPolicies`, `EpicMemberStatus::ReviewBlocked`,
`EpicDelta::Reviewed`, `EdgeKind::Contract`, `EpicMember.blocked_reason`,
`Task.contracts`, `AutonomousTrigger::EpicAuto`. **Not shipped** (explicit
non-goals): no account pool / spillover (#1173 Pillar A), and no remote-box
dispatch — `spawn_worker` across boxes still needs the remote-spawn path.

## 7. Open questions

1. **Epic anchor on GitHub without a parent issue** — require one (the
   `designissues` snippet already makes it) or allow kv-only epics? Lean:
   allow kv-only, nudge toward a tracking issue.
2. **`Blocked by:` marker syntax** — reuse a line the `designissues` prompt
   already emits, and accept `Depends on:`; decide whether lazybox also
   posts it as a comment for visibility.
3. ~~**Concurrency cap for assisted dispatch** — per epic, global, or per
   account?~~ **Decided (P4): per epic**, reusing `agent.max_epic_workers`
   (default 6) — the same cap the `spawn_worker` MCP tool enforces, so a
   Coordinator fanning out by hand and the `AUTO` latch fanning out on its own
   are bounded by one number. `0` disables both. Per-account spillover stays
   with the #1173 account-pool idea.
4. **Status write-back** — mirror derived status to Linear `AgentActivity`
   / a GitHub tracking-issue comment on a cadence, or leave lazybox as the
   only view? Lean: opt-in, later.
5. **Remote / multi-user** — an epic spanning boxes (#965 sandboxes) is fine
   for status (the daemon sees all sessions) but `spawn_worker` across boxes
   needs the remote-spawn path; still deferred after P4, which dispatches
   locally only.
6. **Contract granularity** — a `Contract` edge is satisfied by the *existence*
   of a published contract, not by its content matching anything. A producer
   that posts a placeholder unblocks its consumer. **Partly decided (#1577):**
   the persisted `PublishedContract` row carries a `revision` (bumped whenever
   the producer publishes a *changed* interface) and the interface text, so a
   re-published contract is visible in the log instead of changing under its
   consumers in silence, and a later content-aware gate has a version to read.
   The gate itself stays loose — satisfaction still turns on the row existing —
   until the loose form has been dogfooded.
