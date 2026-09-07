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

1. **From a tracker parent** — `x E n` on an issue with sub-issues (or a
   Linear project): lazybox walks `subIssues` transitively across repos,
   reads dependencies and body markers, and materializes the Epic.
2. **From the planner role** — spawn a Planner (`x E p`) with the `carve` /
   `designissues` brief plus one new instruction: *use `--parent` and
   `--blocked-by` / `Blocked by:` markers so the graph is machine-readable.*
   Lazybox picks it up on the next poll and shows the DAG. The planner's
   prose stays; the structure becomes real.
3. **Ad hoc** — `x E a` adds the cursor row or the multi-select to an epic;
   `x E d` adds a blocking edge by picking the blocker from a picker.

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

Keys (all under one `x E` leader; which-key popup):
`n` new · `a` add · `d` add dependency · `r` set role · `s` start next
ready (spawn workers on every ready member, behind the usual "start N
agents?" confirm) · `g` graph view · `m` merge order · `p` spawn planner ·
`x` archive. Plus one global jump, `Shift-E`: next epic member that needs
you (asking → failed → ready), mirroring `!` / `Shift-F`. Filters gain
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
- An **ASCII DAG** by wave — columns are waves, `─┬─` fan-outs; `x E g`
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
with `x E r`. The #1173 Pillar-B pipeline (implement → review → fix as fresh
agents) becomes "a member whose DoD includes a Reviewer pass", not a new
engine.

### 4h. Execution and the autonomy dial

Earn it in three notches, each an existing latch shape:

1. **Manual** (ship first): lazybox surfaces *ready*; you press `x E s`.
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

| Phase | Delivers | New state | Answers |
|---|---|---|---|
| **P0 — edges in the inbox** | GitHub provider fills `Task.parent` from sub-issues and reads dependencies + `Blocked by:` markers; Linear reads `blocks`; rows get `⛔ blocked by N` / `▶ ready`; `ready` / `blocked` filters; `Shift-E` jump | none (Task fields) | "what can I start right now" |
| **P1 — Epic + live status** | Epic record (kv), sidebar tier from a tracker parent, header status line, overview pane, `EpicResolver` + `Event::EpicStatus`, epic events in the inbox, MCP `epic_status` / `epic_ready`, coordinator briefing | `epic:<key>` | **"give me status"** without a model |
| **P2 — roles** | `Workspace.role`, badges, role prompt preambles, `x E r`, `spawn_worker` MCP tool for Coordinators, Planner spawn with machine-readable-graph instruction | `role` field | who does what, enforced |
| **P3 — graph + merge order** | full-screen DAG, `MergeAfter` edges, merge-order readout, merge-on-green hold | edge kinds | landing order across repos |
| **P4 — autonomy dial** | `AUTO` latch (assisted dispatch on ready), Reviewer stage, held-merge auto-release | policy latch | the fleet runs the plan; you triage |

P0 has value with no new concept at all and de-risks the providers. P1 is the
one the "give me status" habit needs; it should be the first thing dogfooded.

## 7. Open questions

1. **Epic anchor on GitHub without a parent issue** — require one (the
   `designissues` snippet already makes it) or allow kv-only epics? Lean:
   allow kv-only, nudge toward a tracking issue.
2. **`Blocked by:` marker syntax** — reuse a line the `designissues` prompt
   already emits, and accept `Depends on:`; decide whether lazybox also
   posts it as a comment for visibility.
3. **Concurrency cap for assisted dispatch** — per epic, global, or per
   account (ties to the #1173 account-pool idea)?
4. **Status write-back** — mirror derived status to Linear `AgentActivity`
   / a GitHub tracking-issue comment on a cadence, or leave lazybox as the
   only view? Lean: opt-in, later.
5. **Remote / multi-user** — an epic spanning boxes (#965 sandboxes) is fine
   for status (the daemon sees all sessions) but `spawn_worker` across boxes
   needs the remote-spawn path; defer.
