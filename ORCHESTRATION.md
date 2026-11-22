# Orchestration: missions, events and a configurable process

How Lazybox should let an agent manage other agents on one machine: keep the
mission, split it into goals and tasks, start and watch workers, move work
through phases, and leave a record a person can trust. It is a design,
written 2026-11-16, with its [decisions](#decisions-for-you) taken. Slice 0
(the process file: §4) is implemented in
[`internal/process`](services/runner/code/internal/process/doc.go) and
[slice 1](#slice-1-as-built) (the mission log and the derived board, solo
pair) in [`internal/work`](services/runner/code/internal/work/doc.go);
[the Orchestrator grant](#the-orchestrator-grant-and-lineage-slice-2a-as-built)
(one conversation runs a mission's agents, under a ceiling the person sets)
in `internal/work/grant.go` and `internal/runner/orchestrator.go`; the rest is
not yet.
[Software factory](SOFTWARE_FACTORY.md) describes the end state (contract,
holdouts, verifier, merge queue); this page is the model and configuration
underneath it.

Constraints it respects: **D14** (task, assignment, attempt, evidence and
integration are separate records; one writer per mutable checkout), **D15**
(role policies and evidence-backed completion before automatic routing or
workflow plugins), **C17** (the shared Hub lives in a separate platform; local missions must
not become a competing Hub, so the event envelope is designed to be exported
to it).

## Short answers

| Question | Answer |
| --- | --- |
| Do we have entry history like V1? | **Yes, since slice 1.** V1 kept a per-terminal prompt history (text, time, typed or snippet; 128 KiB per session) with a picker. V2 recalls sent messages with ↑/↓ in the composer, and the Runner keeps every prompt with its source in its prompt log (§6), which recall reads, so it follows the person across windows and devices. |
| Do we track snippets specially? | **Partly.** V2 keeps five recent keys and favourites in client preferences (per device). V1's per-workspace memory (`]N`), `next:` chains (`]]n`) and `action:` contracts (review → fixer artifacts) are not in V2. [§6](#6-prompt-history-and-snippets) makes them events. |
| An agent for transitions? | **Yes, but most transitions are rules.** Mechanical moves (report → verify, verdict pass → integrate) are deterministic rules in the process file; a *transition agent* handles judgment: status reports, stalls, escalations, phase proposals. [§3](#3-roles-are-configuration) |

## 1. What practitioners do

| System | Shape | State and config | Lesson for us |
| --- | --- | --- | --- |
| [Claude Code agent teams](https://code.claude.com/docs/en/agent-teams), [Projects](https://code.claude.com/docs/en/claude-projects) | Lead + teammates sharing a task list with dependencies and file-lock claims; Projects: coordinator + cloud threads, one branch and PR each, grouped *Waiting on you / Working / Landing / Resolved* | Roles from `.claude/agents/*.md`; `TaskCompleted` hooks can block (exit 2); project `MEMORY.md` | Teammates are not isolated ("editing the same file leads to overwrites"); status lags; the lead declares victory early. 3–5 teammates. |
| [Devin managing Devins](https://cognition.com/blog/devin-can-now-manage-devins), [dynamic workflows](https://docs.devin.ai/work-with-devin/dynamic-workflows) | Coordinator spawns children in their own VMs with a compute limit, wakes when one finishes, messages or stops them | Workflows as deterministic scripts; each agent call keyed by a hash and replayed on resume | Children don't report back unless told; managers assume shared state ([Cognition, 2026](https://cognition.com/blog/multi-agents-working)) |
| [Factory Missions](https://factory.com/news/missions-architecture) | Orchestrator, workers, validators (scrutiny + black-box user testing); milestones run **serially** | `validation-contract.md` (assertion ids, tool, evidence) written **before** features; `features.json` claims assertions; model per role | 37% of time validating, 2–4 rounds per milestone, fix work = 34% of implementation |
| [Augment Intent](https://augmentcode.com/blog/intent-a-workspace-for-agent-orchestration) | Coordinator → implementors in waves → verifier; human approves spec first | Living spec agents read and update; worktree per workspace | The spec gate is the human's main lever |
| [OpenAI Symphony](https://github.com/openai/symphony) | One agent per tracker issue; dispatch by state, labels, priority | `WORKFLOW.md`: YAML front matter (tracker, concurrency, hooks, agent) + prompt template; live reload keeps the last good config | Scheduler state is **in memory**: restart re-polls and re-dispatches |
| [Gas Town](https://github.com/gastownhall/gastown), [Beads](https://github.com/steveyegge/beads) | Mayor (coordinator), Polecats (workers), Witness (stall monitor), Refinery (merge queue that bisects) | Beads: hash ids, typed dependencies, `bd ready`, atomic claim; now Dolt, JSONL is an export | 4–10 agents are chaotic without coordination; "polecats never push to main" |
| [Linear agent sessions](https://linear.app/developers/agent-interaction) | An issue delegated to an agent opens a session | Typed activities (`thought`, `action`, `elicitation`, `response`, `error`); session state **derived from the last activity**; stale after 30 min | Typed, append-only activity is enough for a human to follow |
| [Copilot custom agents](https://docs.github.com/en/copilot/reference/custom-agents-configuration), [Mission Control](https://github.blog/ai-and-ml/github-copilot/how-to-orchestrate-agents-using-mission-control/) | Tasks assigned to agents across repos | `.github/agents/*.md`; a profile is pinned to the commit SHA at assignment | Partition by files; run dependent tasks in sequence; watch for drift |
| [Conductor](https://www.conductor.build/docs/reference/settings/reference) | Workspace and branch per task, PR, merge, archive | `.conductor/settings.toml`; managed > repo > user; scripts repo-only, approvals user-only | Layered config with per-layer authority |
| [herdr](https://herdr.dev/docs/socket-api/) | Panes of agents; `agent.wait --until done\|blocked` | Socket API; event history not durable (`events_lost` → snapshot) | `wait` must survive a missed event |
| [Attractor](https://github.com/strongdm/attractor) | DOT pipeline: node shape = handler (LLM task, human wait, condition, fan-out, manager loop) | `goal_gate`, `max_retries`, edge conditions, checkpoint per node, typed stage events, model "stylesheet" | A graph of phases and gates is a readable process format |
| [BMAD](https://docs.bmad-method.org/reference/ticket-tree/), [Spec Kit](https://github.github.io/spec-kit/quickstart.html), [Kiro](https://kiro.dev/docs/specs/) | Spec → plan → tasks files; Kiro runs tasks in dependency waves; Spec Kit `converge` loops until done | BMAD `tickets.toml` with `verify`, `after`, `hitl`; halts after 5 review loops | "Read status from files, not chat output"; the agent never sets `done` |
| [gh-aw](https://github.github.com/gh-aw/introduction/overview/) | Markdown workflows triggered by repo events | Agent read-only; writes only through sanitized **safe outputs** | Scopes, not trust |
| [LangGraph](https://docs.langchain.com/oss/python/langgraph/interrupts), [Agent Framework](https://learn.microsoft.com/en-us/agent-framework/workflows/checkpoints), [Agents SDK](https://openai.github.io/openai-agents-python/handoffs/), [Restate](https://docs.restate.dev/ai) | Durable graphs, checkpoints, handoffs as tools | A resumed node re-executes; checkpoints need stable agent ids | Actions need idempotency keys; no framework replaces our adapters (see [architecture review](ARCHITECTURE.md#critical-coordination-review--2026-11-03)) |

What the evidence says about multi-agent work:

- **Cost.** Multi-agent research used ~15× the tokens of chat; token use
  explained 80% of quality ([Anthropic](https://www.anthropic.com/engineering/multi-agent-research-system)).
  Vague briefs cause duplicated work; subagents should return references, not
  text.
- **Writers in parallel fail; readers in parallel work.** Cognition keeps
  writes single-threaded and adds a clean-context reviewer;
  [Cursor](https://cursor.com/blog/scaling-agents) found flat peers with locks
  slowed 20 agents to the throughput of 2–3, while planners + independent
  workers + a judge worked; a separate integrator became a bottleneck.
- **Visible scorers get gamed.** [METR](https://metr.org/blog/2025-06-05-recent-reward-hacking)
  saw evaluator monkey-patching; "please don't cheat" barely helped. Keep the
  verifier out of what the builder can edit.

## 2. The model

### Hierarchy: Mission → Goal → Task → Attempt

| Level | What it is | Owns | Ends when | Peers |
| --- | --- | --- | --- | --- |
| **Mission** | An outcome a person asked for, across repos | Brief, process pin, budget, autonomy level | Accept gate passes, or canceled | Factory mission, Projects project, Gas Town convoy, Linear project |
| **Goal** | A verifiable checkpoint | **Assertions** (id, check, required evidence) | All its assertions verified and its gate passed | Factory milestone, BMAD epic, Kiro wave |
| **Task** | One assignment: one owner, one branch per repo it touches | Brief (objective, done-criteria, boundaries, output shape — V1's four lines, ≤ 4 KB), dependencies, claimed assertions | Verified and integrated, or failed/canceled | Bead, Linear issue, Symphony issue, BMAD story |
| **Attempt** | One execution of a task by one session | Session, worktree, evidence, cost | Session ends or hands off | V1 execution; D14 "attempt" |

Why this depth and not another:

- **Goals exist because they carry assertions.** A goal without assertions is
  a label; the UI then shows tasks directly under the mission. Goals are
  optional (a one-task mission has an implicit goal).
- **Task ≠ attempt.** A worker crashing, rotating accounts or being replaced
  by a fresh session is a new attempt on the same task. V1 learnt this the
  hard way (`WorkId` is a UUID, not the tracker key, because the issue→PR fold
  changed keys and stranded rows).
- **No arbitrary nesting.** A worker may keep a checklist inside its task;
  that is not a graph node. Deep trees are where Linear-style boards and
  agent planners both lose people.
- **Dependencies are typed edges between tasks**, reusing V1 epics:
  `blocks` (cannot start) and `merge_after` (can build, lands later).

### Events: the only stored state

Every change is an event appended to the mission's log. Nothing else about a
mission is written.

```yaml
id: 01JD8Z…            # ULID, unique
seq: 412                # per-mission, gap-free
at: 2026-11-16T14:03:22Z
mission: m_7f3a
subject: task:t_19       # mission | goal:… | task:… | gate:… | decision:…
type: task.reported
actor:                   # who did it, never inferred from text
  kind: agent            # human | agent | rule | check | lazybox
  role: worker
  conversation: c_88
  execution: e_412
caused_by: 01JD8Y…       # the event this reacts to (trigger chain)
idempotency: e_412/report/1
evidence:
  - run:e_412/r_7        # lazybox run: command, exit 0, commit, output file
  - commit:api@3f9c2e1
  - check:api@3f9c2e1/ci/test
payload: {summary: "…", claims: [A3, A4]}
```

Evidence references point at records Lazybox already keeps: `run:` (a
`lazybox run` record — command, exit status, compact view, full-output file),
`commit:`, `pr:repo#n@sha`, `check:`, `review:` (V1 review artifact),
`file:path@sha`, `turn:conversation#cursor` (a point in a transcript),
`decision:`. Each is pinned to a revision so freshness can be computed later
(V1 `review.rs`: *current / head moved / unknown*).

| Family | Types |
| --- | --- |
| Mission | `mission.created`, `mission.brief_changed`, `mission.process_pinned`, `mission.canceled` |
| Plan | `goal.created`, `assertion.added`, `assertion.changed`, `task.created`, `task.edge_added`, `task.brief_changed`, `task.canceled` |
| Assignment | `task.assigned`, `attempt.started`, `attempt.ended`, `task.handoff` |
| Progress | `task.reported` (a claim), `task.question`, `task.answered`, `task.blocked`, `evidence.recorded` |
| Verification | `verdict.recorded` (assertion × verifier → pass/fail/cannot-check), `finding.filed`, `finding.disposed` |
| Decisions | `decision.requested`, `decision.recorded`, `decision.superseded` |
| Flow | `phase.proposed`, `phase.entered`, `gate.requested`, `gate.passed`, `gate.rejected`, `escalation.raised` |
| Integration | `integration.queued`, `integration.merged`, `integration.failed`, `integration.reverted`, `check.completed`, `check.failed` |
| Resources | `budget.warning`, `budget.exhausted`, `wip.held`, `status.reported` |
| Conversation | `prompt.submitted`, `snippet.sent` (§6) |

Triggers may also use two synthetic sources that are not stored: `timer`
(every N) and `stall` (no event on a live task for N); what they cause is
stored.

Rules for the log: types are versioned (`task.reported@1`); unknown types are
kept and shown raw; an event is refused (with the reason, like V1
`TransitionRefused`) rather than silently dropped; terminal tasks reject late
reports from dead agents; a human event is never attributed to an agent.

### Derived state

State is a fold of the log plus live observations (agent liveness, PR state,
check state), recomputed on each relevant event, exactly like V1's epic
resolver: debounced, a pure function, snapshot cached and rebuildable from
the log.

**Task status, first match wins** (adapted from V1 `member_status`):

| # | Status | Derived from |
| --- | --- | --- |
| 1 | canceled | `task.canceled` |
| 2 | done | accept gate passed, or `integration.merged` when autonomy ≥ 4 |
| 3 | failed | retry budget spent, or an attempt ended without a report (V1 `fail_from_agent_exit`) |
| 4 | needs you | open human gate, question or escalation on the task |
| 5 | verifying | a verifier attempt is live |
| 6 | changes requested | last verdict failed or a blocking finding is open, retries left |
| 7 | working | an attempt is live (observed, not declared) |
| 8 | claimed done | `task.reported`, no verdict yet |
| 9 | blocked | a `blocks` predecessor not done, or held |
| 10 | ready | otherwise |

Flags beside the status: **stale** (evidence pinned to an older head),
**over budget**, **unclaimed assertions**.

Every status shows its **trust level**: *claimed* (agent said so), *observed*
(Runner saw the exit status or process), *verified* (independent verdict or CI
on the same commit), *accepted* (gate). This is the audit's "verified status"
thesis applied to every row.

**Goal** status: all assertions verified → met; any failed → at risk.
**Mission phase**: the last `phase.entered`. The Runner appends it only when
the previous phase's exit gate has passed, or a person overrides (recorded as
such). **Critical path** and **merge order** come from the edges (V1
`critical_path`, `MergeAfter`).

### Decisions, findings, handoffs

- **Decision**: question, options, choice, rationale, who decided, the tasks
  it affects, what it supersedes. Agents fetch the decisions affecting their
  task instead of rereading transcripts (PRODUCT step 5).
- **Finding**: V1 `review.rs` unchanged in spirit — severity, `file:line`,
  evidence, remediation, scope pinned to base/head/dirty digest; a fixer
  answers each with *fixed / already resolved / blocked / refuted*; silence is
  not a disposition; the report is never mutated by the fixer.
- **Handoff**: objective, done so far, decisions, open questions, evidence
  references, receiver. Built from the log, not from scrollback, so a fresh
  session or the other provider can continue (the `checkpoints` package
  already holds objectives, criteria and source evidence).

## 3. Roles are configuration

A role is a named binding in the process file. Code knows only the tool
scopes a role may hold, never the role names.

| Role | Does | Default binding | Never |
| --- | --- | --- | --- |
| **coordinator** | Keeps the mission; creates goals and tasks; assigns; answers worker questions from decisions; asks a person when a decision is missing | strongest model, read-only, no write tools | Edits code; passes human gates |
| **planner** | Turns a brief into spec, assertions and a task graph | same as coordinator, or a cheaper model | Starts workers |
| **worker** | Builds one task in its own worktree branch; runs tests through `lazybox run`; reports with evidence | either provider; YOLO only inside a task worktree | Reads holdouts; merges |
| **verifier** | Fresh context on the *other* provider; gets diff + assertions, not the transcript; records verdicts | read-only in its own worktree; holdout read | Fixes |
| **reviewer** | Files findings against a pinned head | other provider, read-only | Fixes; blocks without evidence |
| **integrator** | Lands verified branches in dependency order; post-merge checks; bisects | usually a **rule**; workers rebase their own branches; an agent only for conflicts | Merges anything unverified |
| **transition** | Gas Town's Witness plus a status writer. Watches the flow: proposes phase changes, writes status reports, detects stalls and loops, escalates | small model, read-only + `phase.propose`, `status.report`, `escalate` | Passes gates; creates work beyond fix tasks |

Each role has: `provider`, `model`, `effort`, `permission` (Ask / Read only /
YOLO), `scopes` (Lazybox tools it may call), `instructions` (inline, a file,
a snippet, or an existing agent file such as `.claude/agents/reviewer.md`,
`.github/agents/*.md` or `.factory/droids/*.md`), `context` (what it is
handed: brief, decisions, diff, assertions, holdouts), `budget` (cost, turns, wall time, retries), `triggers` (event
types plus a condition), `concurrency` (live instances) and `wip` (tasks in
hand).

**Rules before agents.** A trigger's action is either a fixed verb
(`start`, `send`, `request_gate`, `enter_phase`, `enter_next_phase`,
`create_task`, `notify`, `escalate`, `stop`, `exec`, `integration.queue`) or
`wake: <role>` with this event. The defaults
use rules for every mechanical move and agents only for judgment, because
agent-driven transitions are slow, cost tokens on every event and can loop.
A rule may carry `limit: N` (fires at most N times per subject); a
`create_task: fix` is charged to the task's `budgets.task.retries`.

The scopes a role may hold, and the CEL variables each event type offers, are
catalogs in code (`lazybox process events`); there is deliberately no scope
that passes a gate.

## 4. The process file

### Where it lives and how it combines

| Layer | File | Edited by | Can |
| --- | --- | --- | --- |
| Built-in presets | in the binary | releases | everything |
| User | `process.yaml` in the Runner's state directory (`processFile` in the Runner config), so each environment has its own | Settings → Process | set defaults and the **ceiling** |
| Repository | `<repo>/.lazybox/process.yaml` (committed) | PR | choose a preset (`extends:`), add roles, gates, checks; only **within** the ceiling |
| Workspace | Runner state | Settings or the workspace menu | pick which repo's file wins in a multi-repo workspace; override |
| Mission | `mission.process_pinned` event | at mission start | snapshot of the resolved process and its digest |

- **Pinning.** A mission runs the process it started with. Editing a file
  never changes a live mission; "adopt the new process" is an explicit,
  recorded event.
- **Combining.** The preset is the repository's `extends`, else the user's,
  else solo pair (reviewed team once a mission has more than one task). Each
  layer changes only what it sets: scalars, nested mappings field by field,
  lists replaced whole, roles and gates merged by name (`null` removes one).
  The user's role defaults apply to roles the chosen preset has.
- **The ceiling.** A cloned repository is untrusted input. Its process file
  cannot raise autonomy, budgets, permissions or providers above the user's
  ceiling (reviewed team's limits until the person sets one): going above it
  is an error at the line that asked, never a silent clamp, and an unset
  budget takes the ceiling's value. Only the user's file may set a ceiling.
  The first use of a new file digest asks the person (like `direnv allow`).
- **Conditions** use [CEL](https://cel.dev) — non-Turing-complete, bounded,
  already the policy language of Kubernetes admission — over the derived
  state: `task.paths.exists(p, p.startsWith("migrations/"))`. Evaluation is
  bounded again here (1 KiB of source, two nested comprehensions, a cost
  limit and a deadline per evaluation); an evaluation error means "did not
  fire" and is recorded.
- **Escape hatch**: `exec` runs a repository script through `lazybox run`
  with the event in `LAZYBOX_EVENT` (a file path); its output is evidence, it
  holds no Runner credential.

### Validation

1. **Schema**: a JSON Schema generated from the Go types, published with the
   binary (`lazybox process schema`; committed as
   `internal/process/schema/process.v1.json`); editors get completion through
   a `# yaml-language-server: $schema=process.schema.json` line naming the
   schema beside the file (`lazybox schema process --write .lazybox` puts it
   there; nothing is fetched from the web). Every file is checked against it
   before it is decoded, so a wrong key or value is reported at its line.
2. **Semantics** (`lazybox process validate`): roles referenced exist; event
   types are known; every phase is reachable and has an exit; no role holds a
   forbidden scope; budgets fit the ceiling; autonomy 5 without container
   isolation is an error, not a warning; triggers cannot form a cycle without a
   budget or count limit (a rule's `limit`, fix tasks under
   `budgets.task.retries`, an agent's budget, or a human gate or manual
   trigger, which waits for a person). Conditions are compiled per event:
   a rule on `task.reported` cannot read `gate`.
3. **Explain** (`lazybox process explain task.reported`): which rules and
   roles fire, in order (rules in file order, then role triggers, then
   notifications), with what scopes and what they may cause.
4. **Replay** (slice 4, it needs a mission log): run a changed process
   against a recorded mission log and show where it would have diverged
   (which gates, which extra agents, what cost).

`lazybox process validate|show|explain` read files (no Runner needed, for a
PR); with `--workspace` they ask the Runner, whose owner-only
`GetOrchestrationProcess` and `ExplainOrchestrationProcess` also back the
read-only Settings → Process view.

### How people edit it

Settings → Process, not only YAML:

- **Pipeline view**: phases left to right, gates as diamonds (human, agent,
  check), rules as arrows; click to edit.
- **Role cards**: provider and model pickers from configured accounts,
  permission, budgets, triggers as "when … if … do …" rows with the CEL
  condition behind an *Advanced* toggle.
- **Preset switcher** with a diff against the current file; **ceiling**
  section for the user layer.
- Every edit shows the YAML diff and writes back preserving comments. For a
  repository layer it writes the file in the worktree, for review in a PR.
- **Ask Lazybox** edits it through the settings MCP (extended with
  `get_process` / `propose_process_change`), confirmed per change as today.

### Preset: solo pair (level 3)

You coordinate; one worker builds; the other provider reviews on demand.

```yaml
# yaml-language-server: $schema=process.schema.json
version: 1
name: solo-pair
autonomy: 3                      # you review every PR
budgets:
  mission: {usd: 20}
  task:    {usd: 10, retries: 1}
  on_exhausted: escalate         # stop | escalate | warn
wip: {workers: 1, writers_per_repo: 1}
phases:
  - {id: build,  exit: reported}
  - {id: review, exit: reviewed}
  - {id: merge,  exit: merged}
gates:
  reported: {kind: rule, when: "task.claims_evidence"}
  reviewed: {kind: human, show: [diff, findings, evidence]}
  merged:   {kind: check, when: "pr.checks_green && pr.merged"}
roles:
  worker:
    provider: claude
    model: default
    permission: ask
    scopes: [task.report, task.question, evidence.read]
    instructions: {file: .lazybox/roles/worker.md}
    budget: {turns: 80}
    concurrency: 1
  reviewer:
    provider: codex
    model: default
    permission: read_only
    context: [brief, diff, decisions]
    scopes: [finding.file, evidence.read]
    instructions: {snippet: deepreview}
    triggers: [{on: gate.requested, if: "gate.id == 'reviewed'", mode: manual}]
rules:
  - {on: task.reported, do: {request_gate: reviewed}}
  - {on: finding.filed, if: "finding.severity == 'blocker'", do: {send: worker, snippet: fixall}}
  - {on: gate.passed, do: enter_next_phase}
notifications:
  - {on: [gate.requested, task.question, escalation.raised], to: [inbox, desktop]}
snippets:
  suggest: {build: [test, tdd], review: [deepreview], merge: [pr]}
```

### Preset: reviewed team (level 4, default)

A coordinator plans and assigns; workers build in parallel; an independent
verifier checks a fixed contract; you approve the contract and accept the
result.

```yaml
version: 1
name: reviewed-team
autonomy: 4
budgets:
  mission: {usd: 60, wall: 10h}
  task:    {usd: 12, turns: 120, retries: 2}
  on_exhausted: escalate
wip: {workers: 3, writers_per_repo: 1, overlapping_paths: serial}
phases:
  - {id: plan,      exit: contract}
  - {id: build,     exit: all_verified}
  - {id: integrate, exit: merged}
  - {id: accept,    exit: accepted}
gates:
  contract:     {kind: human, show: [brief, assertions, task_graph, budget]}
  all_verified: {kind: rule,  when: "mission.assertions.all(a, a.verdict == 'pass')"}
  merged:       {kind: check, when: "mission.prs.all(p, p.merged && p.checks_green)"}
  accepted:     {kind: human, show: [contract_table, evidence, cost, diffstat]}
roles:
  coordinator:
    provider: claude
    model: strongest
    effort: high
    permission: read_only
    scopes: [goal.*, task.create, task.assign, session.spawn:worker,
             decision.record, decision.request, gate.request, evidence.read]
    context: [brief, decisions, board]
    instructions: {file: .lazybox/roles/coordinator.md}
    triggers:
      - {on: mission.created}
      - {on: [task.question, task.blocked, verdict.recorded, escalation.raised]}
    concurrency: 1
    budget: {usd: 8}
  worker:
    provider: [claude, codex]    # round-robin across configured accounts
    permission: yolo             # only inside its task worktree
    scopes: [task.report, task.question, evidence.read, decision.read]
    context: [task_brief, decisions, assertions_claimed]
    instructions: {file: .lazybox/roles/worker.md}
    concurrency: 3
  verifier:
    provider: other_than_worker
    permission: read_only
    scopes: [verdict.record, evidence.read]
    context: [diff, assertions_claimed]   # never the worker transcript
    instructions: {file: .lazybox/roles/verifier.md}
    budget: {usd: 3, turns: 40}
  reviewer:
    provider: other_than_worker
    permission: read_only
    scopes: [finding.file]
    instructions: {snippet: deepreview}
  transition:
    provider: claude
    model: small
    permission: read_only
    scopes: [phase.propose, status.report, escalate, task.create:fix]
    triggers:
      - {on: timer, every: 30m}
      - {on: attempt.ended}
      - {on: budget.warning}
      - {on: stall, after: 20m}       # no event for a live task
rules:
  - {on: task.reported, if: "task.evidence.exists(e, e.kind == 'run' && e.exit == 0)",
     do: {start: verifier}}
  - {on: task.reported, if: "!task.evidence.exists(e, e.kind == 'run')",
     do: {send: worker, text: "Report rejected: run the tests with lazybox run."}}
  - {on: verdict.recorded, if: "verdict.failed && task.retries_left > 0",
     do: {create_task: fix, assign: worker}}
  - {on: verdict.recorded, if: "task.assertions.all(a, a.verdict == 'pass')",
     do: {start: reviewer}}
  - {on: gate.passed, do: enter_next_phase}
integration:
  strategy: queue                # queue | pr_per_task | manual
  order: dependency              # from merge_after edges
  method: squash
  after_each_merge: {exec: .lazybox/integration-check.sh}
  on_failure: bisect
notifications:
  - {on: [gate.requested, decision.requested], to: [inbox, desktop], priority: high}
  - {on: [escalation.raised, budget.exhausted], to: [inbox, desktop]}
  - {on: status.reported, to: [inbox]}
snippets:
  suggest: {build: [test], integrate: [freshen, push]}
```

### Preset: factory (level 5, guarded)

Scenarios are a holdout; humans write scenarios and watch metrics; merges are
automatic when the holdout passes within budget. Refused unless builders are
isolated.

```yaml
version: 1
name: factory
autonomy: 5
requires: {isolation: container}   # otherwise validation fails
budgets:
  mission: {usd: 400, wall: 48h}
  task:    {usd: 25, turns: 200, retries: 3}
  on_exhausted: stop
wip: {workers: 8, writers_per_repo: 2, overlapping_paths: serial}
holdout: {store: runner, readers: [verifier]}
phases:
  - {id: spec,      exit: contract}
  - {id: plan,      exit: graph_ok}
  - {id: build,     exit: holdout_pass}
  - {id: integrate, exit: merged}
gates:
  contract:     {kind: human, show: [brief, assertions, scenarios]}   # the one human gate
  graph_ok:     {kind: agent, role: transition, when: "mission.tasks.all(t, t.claims.size() > 0)"}
  holdout_pass: {kind: agent, role: verifier,
                 when: "mission.scenarios.all(s, s.verdict == 'pass')"}
  merged:       {kind: check, when: "mission.prs.all(p, p.merged && p.post_merge_green)"}
roles:
  planner:
    provider: claude
    model: strongest
    permission: read_only
    scopes: [goal.*, assertion.*, task.create, task.edge]
    triggers: [{on: gate.passed, if: "gate.id == 'contract'"}]
  coordinator:
    provider: claude
    model: strongest
    permission: read_only
    scopes: [task.assign, session.spawn:worker, decision.record, escalate, evidence.read]
    triggers: [{on: [phase.entered, task.question, verdict.recorded, integration.failed]}]
  worker:
    provider: [claude, codex]
    permission: yolo
    scopes: [task.report, task.question, evidence.read, decision.read]
    concurrency: 8
  verifier:
    provider: other_than_worker
    permission: read_only
    scopes: [verdict.record, holdout.read, evidence.read]
    context: [diff, assertions_claimed, scenarios]
  integrator:
    provider: codex
    permission: yolo                # only in the integration worktree
    scopes: [integration.*, task.create:fix]
    triggers: [{on: integration.failed, if: "failure.kind == 'conflict'"}]
  transition:
    provider: claude
    model: small
    scopes: [phase.propose, status.report, escalate, task.create:fix]
    triggers: [{on: timer, every: 1h}, {on: stall, after: 30m}, {on: budget.warning}]
rules:
  - {on: task.reported, do: {start: verifier}}
  - {on: verdict.recorded, if: "verdict.failed", do: {create_task: fix, assign: worker}}
  - {on: verdict.recorded, if: "verdict.passed", do: {integration.queue: task}}
  - {on: check.failed, if: "check.branch == 'main'", do: {create_task: fix}}
  - {on: gate.passed, do: enter_next_phase}
integration: {strategy: queue, order: dependency, method: squash, auto_merge: true, on_failure: bisect}
notifications:
  - {on: [escalation.raised, budget.exhausted, integration.reverted], to: [inbox, desktop]}
  - {on: status.reported, to: [inbox], digest: daily}
```

## 5. On Lazybox V2

### What exists and what it becomes

| Today | Role in orchestration |
| --- | --- |
| Session hosts; agents outlive the Runner | Workers and the coordinator survive Runner restarts; the mission log resumes from its last sequence |
| `journal` (append-only, fsynced, cursors) | The mission log's storage, with no size eviction (missions are compacted to a snapshot, never truncated) |
| Multi-repo workspaces of worktrees | One **task workspace** per task (all sessions in a workspace share files, so parallel writers need separate workspaces); the mission links them |
| Attention inbox | Gates, questions, escalations and decisions become inbox rows with the mission attached |
| PR loop (commit, push, open, checks, merge) | The integrator's operations; check results become `check:` evidence |
| Help settings MCP | Grows into the Lazybox MCP; the same confirm-per-change model edits the process |
| `lazybox run` records and telemetry `commands` | `run:` evidence with exit status, commit, compact and full output |
| Per-execution HMAC credential (F1) | Extended into role scopes |
| Metering gateway, telemetry | Budgets per mission, task, role; cost per accepted task |
| Live instruction delivery, live permission change | Role changes and decisions reach running sessions |
| `checkpoints` | Handoff packets |
| `internal/work` | The event types, the mission log, the fold and the derived status (slice 1) |

### Lazybox MCP for orchestrating roles

Also exposed as `lazybox mission …` for agents without MCP (V1 `work_calls`
pattern: one implementation, two surfaces).

| Tool | Purpose | Scope |
| --- | --- | --- |
| `mission_get` | Brief, phase, goals, assertions, board, budget left | any role in the mission |
| `task_create` / `task_edge` | Add a task or dependency | `task.create`, `task.edge` |
| `task_assign` | Create the task workspace (new branches) and start a session with a role binding | `session.spawn:<role>` |
| `session_send` | Send a message or snippet to a session in the mission | `session.send` |
| `wait` | Block until events match a filter or a timeout; returns the events | any |
| `evidence_list` / `evidence_read` | Runs, commits, checks, findings for a task; compact view by default | `evidence.read` |
| `task_report` | Claim done with summary and evidence refs (refused without a ref when the process requires one) | `task.report` |
| `task_question` | Ask the coordinator or a person; becomes an inbox row if routed to a person | `task.question` |
| `decision_record` / `decision_request` | Record or ask for a decision | `decision.*` |
| `verdict_record` / `finding_file` | Verification results | `verdict.record`, `finding.file` |
| `gate_request` | Ask for a gate; humans pass it in the app | `gate.request` |
| `phase_propose`, `status_report`, `escalate` | Transition agent outputs | as named |
| `integration_queue` | Put a verified task in the merge queue | `integration.*` |

### Security model

- **Credentials.** Each execution keeps its HMAC credential. The Runner adds a
  grant table: execution → mission, task, role, scopes, taken from the pinned
  process. Every tool call checks the grant; an agent cannot widen its own
  scopes, and process edits by an agent are proposals a person confirms.
- **Human gates are owner-only.** No configuration can give a role
  `gate.pass` for a human gate; the schema has no such scope. (This is the
  self-approval hole F1 closed, kept closed by construction.)
- **Spawn bounds.** A spawned session carries its parent's depth plus one (V1
  `start_depth`), with a maximum per process, children per role and sessions
  per mission; ask chains keep V1's `MAX_ASK_DEPTH` loop guard.
- **Git and GitHub.** Workers push only their task branch; force-push and
  merge to the base are integrator or rule operations. The GitHub token stays
  in the Runner; agents use PR tools, never the token.
- **Holdouts** are readable only with `holdout.read` (verifier); builders are
  denied the path. Soft until container isolation, as the factory page says.
- **Budgets** are enforced in the metering gateway per execution, so
  orchestrated sessions are always metered.
- **Repository process files** stay under the user's ceiling (above).

## 6. Prompt history and snippets

| | V1 | V2 today | Proposed |
| --- | --- | --- | --- |
| Prompt history | Per terminal: text, time, `typed` or `snippet`; Runner keeps 128 KiB, picker of 200 | In the conversation journal, no recall | `prompt.submitted` with source; ↑ / Ctrl-R in the composer; search per conversation, workspace, all |
| Recent snippets | Global five, in `state.db` | Five in client preferences, per device | Per person, in the Runner, so it follows devices; plus *frequent* (frecency) |
| Per-workspace memory | Last 12 distinct keys, `]N` badge | none | Derived per workspace and per task: "`deepreview` sent at 3f9c2e1, head has moved" |
| Chains | `next:` resolved from the last snippet sent to that agent; `]]n`; refuses if the last one was not acknowledged | none | Same, from `snippet.sent` in the log; the inbox can offer "next: `fixall`" |
| Artifact contracts | `action: deep_review / fix_all` with review artifacts | none | `expects:` on a snippet names the events it must produce (`finding.filed` or an empty review); missing → flagged "delivered, no result" |

The source of a prompt is recorded: `typed`, `snippet{key, catalog digest}`,
`console{run}` (Send to agent), `inbox` (quick reply), `role{mission, rule}`
(sent by a rule or the coordinator) or `handoff`. That answers "who told the
agent to do this" without reading transcripts. Snippets plug into the process:
role instructions can be a snippet, rules can send one, and each phase can
suggest some. Prompt text stays in Runner state; mission exports leave it out
unless asked.

## 7. Plan

### Slices

| # | Slice | Value | Size |
| --- | --- | --- | --- |
| 0 | **Process schema, presets, `validate` / `explain`.** No agents. | Settles the configuration contract before code depends on it | S |
| 1 | **Mission log and derived board, solo pair.** Mission, tasks, events in `internal/work`; task workspaces; `task_report` with evidence; rule-started reviewer on the other provider with V1 findings; gates in the inbox; prompt history and snippet events (§6). | First honest *claimed vs verified* column on real work; V1 parity for history and chains | M |
| 2 | **Coordinator and scoped MCP.** Grant table, the MCP tools above, `wait`, decisions, reviewed-team preset without the merge queue (ends in PRs). | One agent manages others, under limits a person can read | M |
| 3 | **Integration.** Merge queue in `merge_after` order on the PR loop, post-merge checks, bisect, CI failure → fix task. | Parallel work lands without semantic breakage | M |
| 4 | **Transition agent and process editor.** Status reports, stall detection, pipeline and role-card UI, Ask Lazybox edits, replay. | Configurable by people who will not write YAML | M |
| 5 | **Factory.** Holdout store, container isolation, level 5, scheduled triggers. | Only after 1–4 show measured verified-done rates | L |
| — | **Hub export.** The event envelope is the contract offered to the external Hub (C17); missions stay local until the Hub accepts them. | Collaboration without a second Hub | — |

Slice 1 is the smallest valuable one: it needs no coordinator agent and
already answers "what is going on, who claimed what, and what actually ran".

### Slice 1 as built

The decisions below were taken as recommended (optional goals; a committed
per-repository process file; CEL; solo pair by default, reviewed team once a
mission has more than one task; the transition agent in slice 4; existing role
files read as instructions; prompt history in the Runner; missions local).

**Where it lives.** `services/runner/code/internal/work` is the domain:
`event.go` (types, versions, payloads), `fold.go` (the one definition of what
a log accepts, and the fold), `derive.go` (status and trust, pure),
`store.go` and `ops.go` (one append-only, fsynced JSONL log per mission under
`<state>/work/missions/`, idempotency keys, rules), `prompts.go` (the
Runner-wide prompt log, `<state>/work/prompts.jsonl`). The Runner feeds it in
`internal/runner/work.go`; the owner-only RPCs are `ListMissions`,
`GetMission`, `CreateMission`, `UpdateMission` (one typed change) and
`ListPromptHistory` (`internal/transport/missions.go`). None is on the agent or
terminal allowlist.

**The process.** `work.Policy` (`internal/work/policy.go`) is what slice 1
asks of a process: preset, digest, autonomy, the human gate a report
requests, the gate that makes a task done. `work.PolicyFor` resolves the
built-in preset through slice 0's `internal/process` (`process.Resolve`) and
takes the unconditional `request_gate` of its `task.reported` rule, which must
be a human gate (`reviewed` in solo pair); passing it makes a task done. A
mission pins the digest and those gates in `mission.process_pinned`, so its
log means the same after a release changes a preset. Every new mission pins
solo pair, since presets whose reports go to a verifier (reviewed team) are
refused until slice 2 runs one; the user's and the repository's process files
are not read yet.

**What is recorded, automatically.** A `lazybox run` (or `lazybox log`) that
finishes in a conversation which is a live attempt becomes `evidence.recorded`
on its task, with the exit status (none for a pipe), the full-output path,
what the agent saw, and each workspace repository's HEAD. A workspace
terminal's run attaches to the attempts in its workspace, as do commits made
through the PR loop and pull requests the PR loop reads (`GetPullRequest`,
create, merge): one record per head and check state. While a board is open,
the Runner rereads its attempts' pull requests at most once a minute each. A
deleted conversation ends its attempts (`attempt.ended`, deleted) and its
prompts are forgotten. Every message accepted by an agent is a
`prompt.submitted` with its source (`typed`, `snippet`, `mission`, …); a
message that is exactly a catalog snippet's body, or is labeled as one, also
records `snippet.sent` with the chain it offers.

**Trust, as derived.** *claimed*: reported, with no passing evidence (a failing
last run is flagged). *observed*: the last real `lazybox run` exited 0, or the
attempt's agent is running. *verified*: checks passed on a pull request whose
head is the newest the Runner saw for that repository (a later run or commit
on another head makes it *stale*). *accepted*: the person passed the gate. A
report always requests the gate; the status says how much backs it.

**On screen.** The *Missions* panel (page menu → Missions; dockable like any
panel): missions, a board in four columns (Ready, Working, Needs you, Done)
with status, trust badge, flags, evidence chips (a run opens its output in
the console, a pull request opens on GitHub), the conversation working on the
task and its liveness, the snippet chain (*After review, next: fixall*), and
the log as History. Assign opens a new conversation in a chosen workspace (or
takes one you have) and can send the brief. The inbox lists *Awaiting
acceptance* first, counted in its badge, with Accept and Ask for changes.

**Not yet.** An agent cannot report its own task (slice 2's `task_report`
needs the grant table): the person reports. No reviewer is started by a rule,
so `verifying`, verdicts and findings are not produced. Goals and dependencies
exist in the log and the RPC; the panel creates dependencies, not goals.
Assigning does not create a task workspace; the person picks one. Pull
requests are reread only while a board is open. A log is never compacted
(64 MiB per mission). Mission exports for the Hub are not built.

### The reviewer, person-triggered (first step of slice 2)

The *reviewer* role of §3 exists today as one click ([GITHUB.md, "Agent
review"](GITHUB.md#agent-review)): a person starts it on a conversation's
changes, one turn or its pull request; the Runner opens a read-only
conversation on the other provider (the role's `other_than_worker`), gives it
`context: [brief, diff]` (the goal, the scope's diff and files, the run
evidence), and its findings are V1's — severity, `file:line`, quoted lines,
pinned to the head they were read on, with freshness — filed through a scoped
verb (`lazybox review add`, the `finding.file` scope in all but name). The
person triages them (accept, dismiss with a reason) and sends accepted ones to
the author; silence is not a disposition. On a mission task the review is
`review:` evidence: pending while it runs, failing while blockers are open,
which keeps the task from *verified*.

Kept within slice 1's APIs: no grant table (the agent allowlist admits
`AddReviewFinding` and `ListReviewFindings` for the caller's own execution,
and the Runner admits only the reviewer of a review in its own workspace), no
`finding.filed` event (findings live in the workspace's review store; the
mission log records the review's outcome as evidence), and the reviewer is
started by a person, not by a rule.

The loop after it is person-steered too ([GITHUB.md, "The review
loop"](GITHUB.md#the-review-loop)): findings reach the author as filed, the
author's fixing turn starts a re-review of that turn only, the reviewer
resolves or reopens each finding, until no blocker is open, a round limit, or
the same blocker still open after two fixes (then the person decides). The
two agents may ask each other through `lazybox ask` — the `session.send`
of §5 narrowed to one review's pair. Its rounds and timeline are kept on the
review, not yet as mission events.

How slice 2 automates it, with the process file unchanged: the reviewed-team
rule `{on: verdict.recorded, if: "task.assertions.all(…)", do: {start: reviewer}}`
and solo pair's `triggers: [{on: gate.requested, if: "gate.id == 'reviewed'", mode: manual}]`
call the same start (provider `other_than_worker`, permission `read_only`,
`instructions: {snippet: deepreview}` as the brief's template); the grant
table gives the reviewer execution `finding.file`, which `lazybox review add`
becomes; each finding is also a `finding.filed` event on the task, and the
person's accept or dismiss a `finding.disposed`; a rule on `finding.filed`
with `finding.severity == 'blocker'` sends the author `fixall`. Nothing in the
person-triggered path is replaced: a rule starts what the click starts.

### The Orchestrator grant and lineage (slice 2a as built)

The coordination design of [sota-agents.md §6](audits/2026-11-16/sota-agents.md#6-a-coordination-design-for-lazybox)
starts here: a person lets one conversation run a mission's agents, and that
conversation drives only the agents it started. Guide:
[Let an agent run agents](docs/guides/orchestrator.md).

**The grant is a mission event.** `grant.issued` (subject `grant:<id>`)
names the holder conversation, the scopes (`session.spawn`, `session.send`,
`session.wait`, `session.stop`, `task.create`; there is deliberately no scope
that approves, answers, passes a gate or accepts) and a **ceiling**:
permission (read-only, ask or yolo), the subscriptions children may run on,
how many may be live at once (1–16, default 3), depth (1–3, default 1) and a
budget for the tree. `grant.revoked` ends it. Only a person issues a root
grant (`UpdateMission` `grant`, owner only); the fold
(`internal/work/grant.go`) refuses one from anyone else, a second live grant
for the same conversation, and (Store) the same conversation orchestrating two
missions. The person's ceiling cannot exceed the conversation's own permission
(`inherit` counts as ask); the subscriptions default to its own and its
rotation, and the person may name others. A grant an agent narrows for its
own child (depth > 1) must be a subset in every field, and falls with the
grant it came from. Deleting the holder revokes its grants (`lazybox` actor).

**Lineage is the attempt.** A spawn appends `task.created` (actor agent),
`task.assigned` and `attempt.started` in one batch; the assignment carries
`parent` (the orchestrator's conversation), `grant`, `role`, `permission` and
`profile`. The fold checks the grant is in force, the actor is its holder,
there is room for one more live child (a child counts until its task is
accepted or canceled, or its attempt ends), and the child's permission and
subscription are within the ceiling. The Runner (`runner/orchestrator.go`)
narrows further before it opens anything: the child's permission is the
narrower of the request, the ceiling and the orchestrator's permission *now*;
it never rotates subscriptions; a parent in plan mode gets a child in plan
mode; it uses the parent's gateway. `--workspace new` creates a workspace
with the parent's repositories on new branches from the parent's branches.

**An agent writes a short list.** `agentMay` in `fold.go` is every event an
agent may be the actor of: `task.created` (holder), `task.assigned` (holder,
lineage-checked), `attempt.ended` (its own child), `task.reported` (its own
task), `grant.*` (narrowed for its own child) and `message.sent`. Plus one
exception: a reviewer's verdict (`evidence.recorded` of kind `review`), which
Lazybox records from the review store. Gate decisions stay `human` only.

**Messages.** `message.sent` (subject `message:<id>`) goes only between a
parent and its child (`State.Related`), from the sending agent itself, at
most 4 KB, at most six a minute in each direction of a pair (counted from the
log, so a restart does not reset it). The receiver gets it as a turn whose
journal record carries `data.origin` (`agent.Origin`, kinds `agent_brief`,
`agent_note` and `agent_report`, with the sender's conversation so clients
can open it): clients show it as an event of the conversation from the
sender, opening to the exact text, never as "You". The prompt log records it with source `agent`. The
sender's transcript gets an `agent_message` line; spawns, stops and grants are
`agent_spawned`, `agent_stopped`, `orchestration_granted|revoked` lines.

**The verbs** are six RPCs on the agent allowlist (`transport/auth.go`), each
for the caller's own execution only: `SpawnAgent` (also needs a grant in
force), `ListAgentLineage`, `SendAgentMessage` and `WaitAgents` (every target
in the caller's lineage: its children under a live grant, or `parent`),
`StopChildAgent` (a child, never `parent`) and `ReportAgentTask` (a child,
its own task: the mission's report rule opens the person's gate, and the
parent is told). `TestOrchestratorGrantWalksEveryRPC` walks every other RPC
and stream for the orchestrator's and the child's credentials (denied), and
names the forbidden ones: approve, answer and permission change on the child's
pending request, accept, reject, grant and revoke. CLI: `lazybox agents
spawn|ls|send|wait|watch|stop|report`; `wait` and `watch` long-poll in 55 s
steps and ride over a Runner restart.

**Briefings.** The orchestrator's (what it may do, its ceiling) goes with the
person's next message as instructions, outside the transcript; a revoke does
the same. A child's is its session's own instructions (who started it, how to
report and reply; approvals go to the person). Both are constants in
`runner/orchestrator.go`, apart from `commandBriefing`.

**On screen.** `/orchestrate` (⌘K *Let this conversation run agents…*, or the
board's *Let a conversation run agents…*) opens the grant sheet. The header
shows **Orchestrator n/m**; its card lists the ceiling and the agents with
their state, and revokes. A child's header leads to its orchestrator
(`Session.lineage`, which the Agents panel can nest by). The board shows the
orchestrator above the columns and, on each card, who started it as what.

**Not yet** (in this slice). Plan approval and budgets, the Missions page
with its Timeline, Messages and Leases tabs, live overlap and review
capacity are below; the merge train is designed there, not built. A child's
approvals reach the person's Agents list and Inbox labelled *started by
<orchestrator> · <mission>*, and the Agents list nests an orchestrator's
agents under it (`agents-model.ts` `nestByLineage`); plans to approve and
tasks to accept sit at its top and in the Inbox count.
`ask` (a blocking question to a child) is `send` plus `wait --until idle`.
Not verified with real Claude or Codex agents: the fake provider and the
fake Runner drive every test.

### A conversation an agent asks the person to start (as built)

A grant is too much ceremony when the person just tells an agent, in its
chat, to start a second conversation. In the field, an agent asked to do
that ran `lazybox agents` (exit 2), then `spawn` (exit 4 and a paragraph of
what agent credentials may do), and got no further. Now:

- **One request, one click.** `lazybox conversation new` (and `lazybox
  agents spawn` without a grant) calls `RequestConversationStart`, which
  records a request on the asking conversation
  (`runner/conversation_starts.go`, persisted in `conversation-starts.json`).
  The request carries the title, the brief (at most 8 KB) or an artifact of
  the workspace, the reason, the workspace (`same` or `new`), the agent
  (provider, subscription, model) and the asking conversation's permission
  and gateway.
- **Shown like an approval.** It is a pending entry of type
  `conversation_request` on the session view, so the chat card, the Inbox,
  `lazybox inbox` and a paired phone's notifications show it. The card shows
  how many conversations already work in the workspace (`shared_with`) and
  offers a new workspace in one click. `AnswerConversationStart` is the
  owner's. A paired phone with `respond` may also answer, but only as asked:
  start or decline, never another agent, model or workspace.
- **A peer, not a child.** Starting the conversation opens it at the asking
  conversation's permission, in its workspace or in a new one branched from
  it (`childWorkspace`). The conversation gets no lineage. Its first message
  is the brief, plus a line naming who asked and why (origin
  `conversation_brief`). A brief artifact is named by its id, and a new
  workspace gets a copy of it.
- **The answer.** The asking agent collects the answer from
  `WaitConversationStart` (exit 0 started, 4 declined, 5 no answer yet). If
  it no longer waits, the answer reaches it as a note. Either way, the
  asking conversation shows one line (`conversation_started`, with
  **Open**, or `conversation_declined`).
- **Guards.** A conversation may have at most 3 requests waiting and make 10
  an hour. A request with the same title is the same request. A request
  expires after 24 hours, and there is no auto-approval: no setting, hook or
  extension can answer one.
- **Under a grant, unchanged.** A conversation that holds a grant in force
  is not asked. Its request becomes `Spawn` under the grant, a child on the
  mission, exactly as `lazybox agents spawn` does.

Every coding agent's briefing has one line about it (`conversationBriefing`,
kept apart from `commandBriefing`). An agent's refusals are one line naming
the next step (`agentDenial` in `transport/auth.go`), never the list of
everything an agent credential may do. Guide:
[Ask an agent to start another conversation](docs/guides/orchestrator.md#ask-an-agent-to-start-another-conversation).

### Plan approval and budgets (slices 2b and 2d as built)

**The plan** (`internal/work/plan.go`) is `plan.proposed` (subject
`plan:<id>`, actor the grant's holder): up to 16 tasks, each with a key,
title, brief, role, permission and subscription (within the ceiling),
workspace (`same`/`new`), paths (relative; a trailing `/` is a folder), an
estimate and `after` keys (no cycles). A new proposal supersedes one still
waiting. `plan.approved` and `plan.rejected` are a person's only; approval
defaults to no budget cap. The reviewer can select **Set a budget cap** before approving. The decision carries the optional cap and the fan-out, which become the grant's, and the same
batch creates the plan's tasks with their `blocks` edges (actor `rule`,
role `plan`). `Plan.Overlaps` lists paths more than one task names. The
orchestrator is told the decision as a turn (`agent.Origin` kind
`plan_decided`) and in its transcript.

**Plan first.** A grant's ceiling may require a plan (`plan_required`; the
grant sheet's default). Then `SpawnAgent` starts only tasks of an approved
plan of that grant, only once every task it waits for is accepted, and
takes the plan's role, permission, subscription and workspace as defaults
(still narrowed as in 2a). A narrowed grant keeps the requirement.

**Budget.** A tree's spend is the telemetry cost of the orchestrator's and
every child's conversation (API-equivalent prices, unpriced usage counted
as nothing; cached 5 s). A live child reserves what it has not spent of its
plan estimate. `SpawnAgent` refuses a spawn whose estimate would take spent
+ reserved past the cap and records `budget.exhausted` once (actor
`lazybox`); past 80% it records `budget.warning`. `budget.raised` (the
person's) sets a new cap and clears it. The board shows each task's cost
against its estimate and each grant's spent, reserved and cap.

**Surfaces.** `ProposePlan` is on the agent allowlist for a holder only;
`UpdateMission` `approve_plan`, `reject_plan` and `raise_budget` are the
owner's. CLI: `lazybox mission plan <file|->` (agent), `mission
approve|reject|budget` (person); `lazybox agents ls` shows the plan's tasks
with their ids and the budget. The plan sheet (`PlanSheet.tsx`) opens from
the orchestrator's transcript, the Inbox (**Plans to approve**), the
board and the badge. Tests: `plan_test.go`, `plans_test.go`,
`mission_test.go`, `test:orchestration-plans`.

**Not yet.** Agents already running are not paused at the cap (the design's
turn-end pause in the metering gateway); an estimate is the orchestrator's
own, not computed from model prices and history. Paths are declared, not
leased: overlap is flagged in the plan only.

### The missions board, leases, live overlap and review capacity (slices 2c, 3a as built)

**The board is a page.** ⌘K *Open the missions board* (or the expand
button on the Missions panel, or a badge card's mission link) opens it as a
page of its own (`showMissionsPage`, `navigate-requests.ts`), where the four
columns stand side by side. Tabs (←/→, Home/End): **Board**, **Timeline**,
**Messages**, **Leases**.

- **Timeline** (`MissionTimeline`): every event of the log oldest first, one
  lane per actor (you, each orchestrator, each agent, Lazybox for rules,
  checks and what it observed), a dot where each acted, and *after #n* on an
  event whose `caused_by` is set, which highlights the event it reacts to.
  `MissionEvent` now carries `id` and `caused_by`.
- **Messages** (`MissionMessages`): `message.sent` as one thread per
  parent–child pair, reports marked.
- **Leases** (`MissionLeases`): leases in force and the overlaps observed.

**Leases** (`internal/work/lease.go`) are advisory: `lease.claimed`
(subject `lease:<id>`) names a task, a relative path (a trailing `/` is a
folder), `exclusive` or `shared`, and a TTL (default 2 h, at most 24 h); it
lapses by itself. An agent leases only for its own task, an orchestrator
also for its agents' tasks. A claim that meets another task's live lease
where either is exclusive is refused, naming the holder. CLI:
`lazybox lease claim <path> [--exclusive] [--ttl] [--task] | release | ls`
(`ClaimLease`, `ReleaseLease`, `ListLeases` on the agent allowlist, for the
caller's own execution).

**Live overlap.** After every turn's code checkpoint
(`code_checkpoints.go` → `overlapTurnEnded`), the files the turn changed
join what that conversation has changed (read back from its checkpoints
after a restart, at most the last 40 turns). If the conversation is a live
attempt, its files are intersected with every other live attempt of the
same mission and with other tasks' exclusive leases; each hit is
`overlap.detected` (actor `lazybox`, once per pair, kind and paths: `files`
or `lease`). The orchestrators of the tasks are told (an `overlap_detected`
event in their transcript and a turn carrying the same origin); the board
flags both cards. Nothing is blocked.

**Review capacity.** A grant's `review_limit` (default 3) holds new agents
while that many tasks of the mission wait at the person's gate:
`SpawnAgent` refuses with the count, records `wip.held` once per count, and
the board says "N tasks wait for your review; new agents are held".

**Not yet: the merge train** (sota-agents.md §6.5 step 3, ORCHESTRATION
slice 3). Design, for the next slice: `integration.queued` when the person
accepts a task whose evidence includes a PR (or by `lazybox mission merge
<task>`); a Runner rule pops the queue in `merge_after` then acceptance
order, rebases the task branch on the base through the PR loop, waits for
its checks, merges with the process's method, runs `after_each_merge`, and
records `integration.merged`; a red post-merge check bisects the merges
since the last green base with the same loop, reverts the culprit
(`integration.reverted`) and opens a fix task for it, assigned to its
orchestrator. Workers never push the base; only the train merges. The
board's Landing column and the timeline's lane for Lazybox show it. Not
built here: it needs the PR loop to take a queue lock per repository, and a
real repository with checks to verify, neither of which the fake Runner has.

### Risks and failure modes

| Failure (source) | Design answer |
| --- | --- |
| Coordinator declares victory early or does the work itself (Claude teams) | Coordinator has no write tools; *done* is derived from verdicts and gates, never from its words |
| Children don't report; managers assume shared state (Cognition) | Report is a tool call with evidence refs; an attempt ending without one fails the task (V1); handoffs carry decisions |
| Parallel writers overwrite each other (Claude teams, Cursor) | One task workspace per task; `writers_per_repo`; `overlapping_paths: serial` |
| Semantic conflicts after clean merges | Merge in `merge_after` order, checks after each merge, bisect (Gas Town Refinery) |
| Integrator bottleneck (Cursor) | Integration is a rule; workers rebase their own branches; the integrator agent only for conflicts |
| Gamed tests (METR, ImpossibleBench) | Verifier on the other provider, fresh context, holdouts; tests read-only in its worktree |
| Review loops that never converge (BMAD halts after 5) | Retry budget per task, then escalate |
| Token cost ×15 (Anthropic) | Budgets in the gateway; rules instead of agents for transitions; cost per accepted task shown |
| Lost state on restart (Symphony keeps it in memory) | Event log; agents outlive the Runner; `wait` resumes from a cursor |
| Missed events (herdr `events_lost`) | `wait` takes a sequence cursor; on gap it returns the derived snapshot |
| Repeated side effects on resume (LangGraph) | Idempotency key on every action event; Runner refuses duplicates |
| Process drift mid-run | Process pinned per mission, like Copilot pinning an agent profile to a SHA |
| A cloned repo raises its own autonomy | User ceiling; first-use approval of each file digest |
| Configurability nobody can predict | `explain` and replay; presets first; D15 still holds: no plugin runtime |
| Drift over long runs (Cursor) | Transition agent proposes fresh attempts from a handoff packet when context fills |

### Decisions for you

**Decided 2026-11-16: every recommendation below was accepted** (optional
goals; a committed per-repository file, the workspace choosing which wins,
under the user's ceiling; CEL; solo pair then reviewed team; the transition
agent in slice 4; existing agent files read as instructions; prompt history
in the Runner; missions local, the event envelope offered to the Hub).

1. **Hierarchy.** Mission → Goal → Task → Attempt with optional goals, or
   drop goals and hang assertions on the mission.
2. **Where the process lives.** Committed `.lazybox/process.yaml` per repo
   (recommended, reviewed like code) or Runner state only. And which file wins
   in a multi-repo workspace: the workspace's choice (recommended) or a merge.
3. **Condition language.** CEL (recommended) or a fixed set of named
   conditions only (less configurable, easier to show in the UI).
4. **Default preset.** Solo pair for new users and reviewed team once a
   mission has more than one task (recommended), or reviewed team always.
5. **Transition agent.** Ship it in slice 4 (recommended; rules cover slice
   1–3), or earlier for status reports only.
6. **Role files.** Also read existing `.claude/agents/*.md`,
   `.github/agents/*.md` and `.factory/droids/*.md` as role instructions
   (recommended), or only Lazybox's own.
7. **Prompt history scope.** Per person across devices in the Runner
   (recommended) or per device as snippets are today.
8. **Hub boundary.** Confirm that missions stay local and the event envelope
   is what Lazybox offers the external Hub (C17).
