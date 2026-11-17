# A software factory on Lazybox

A software factory turns stated intent into merged software through agent
loops, and proves correctness with checks rather than with a person reading
every line. This page says how one would work on Lazybox V2: which parts
exist, which the [roadmap](audits/2026-11-16/README.md) already plans, and
what is new. It is a design for discussion, not an implemented feature.
The underlying model and its configuration are in
[Orchestration](ORCHESTRATION.md).

## What the practitioners do (November 2026)

Sources and the claims behind them are in the research notes; the important
ones:

- **Two working models.** A *dark factory* (StrongDM) where no human writes
  or reviews code and quality is judged against hidden end-to-end
  *scenarios*; and a *lit factory* (Factory Missions, Augment Intent, Devin,
  Claude Projects, Gas Town) where a coordinator splits the work, workers
  build in isolation, and **independent validators** check it, with humans at
  a few gates. Dan Shapiro's levels put the line between them at level 4
  ("you collaborate on specs and plans, the agents do the work") and level 5
  ("a black box that turns specs into software").
- **The contract comes first.** Factory Missions writes a *validation
  contract* — a finite list of testable assertions, each with an id, how to
  check it and the evidence required — before any code. StrongDM keeps its
  scenarios outside the codebase, like a holdout set.
- **Validators are not builders.** They start with fresh context, never see
  the builder's reasoning, and report rather than fix. Self-evaluation bias
  is the failure they exist to prevent.
- **Everyone built their own control plane.** Attractor (StrongDM, a DOT
  pipeline with checkpoints and human gates), Symphony (OpenAI), Gas Town
  (Yegge, with a merge queue that bisects). Workers and spec tools are off
  the shelf; the orchestration, holdouts, merge queue and ledger are not.
- **What breaks.** Agents game visible tests (ImpossibleBench: frequent
  test exploitation; hiding tests cuts it but costs performance; read-only
  tests sit between). Parallel agents conflict semantically — Factory runs
  features serially, Gas Town queues merges, Projects suggests a merge order.
  Review agents that never converge. Specs that agents don't follow. Code that
  nobody understands any more. Cost (StrongDM's "$1,000 of tokens per
  engineer per day" is a stated target, not a measured necessity).

## Why Lazybox is a good host

A factory needs long-running, isolated, observable, metered agent work across
repositories. V2 already has most of the substrate:

| Factory need | Lazybox V2 |
| --- | --- |
| Workers that survive restarts during long runs | Session hosts; agents outlive the Runner |
| Isolation per unit of work, across repos | Multi-repo workspaces of worktrees |
| Validators with different blind spots | Two providers: build with one, verify with the other |
| Evidence a validator can trust | `lazybox run`: real exit status, full output kept, compact view for the agent |
| Human sees everything as it happens | Live consoles, detached windows |
| Cost control | Metering gateway, account rotation, per-session telemetry |
| Changing rules mid-run | Live instruction delivery, live permission change |

Missing: a run and task model, holdout storage, a verifier role, a merge
queue, an attention inbox, checkpoints and the PR loop — all on the roadmap
already. The factory is mostly a *composition* of roadmap items 1–9, plus
three factory-specific pieces: the contract, the holdout store and the
integrator.

## How a run works

```
 brief ──► spec + contract + scenarios ──► [gate 1: approve]
                                               │
                         plan: task graph over repos, budget, WIP limit
                                               │
        ┌──────────── build: workers, one worktree branch each ────────────┐
        │   evidence from lazybox run, checkpoint per turn, ledger events  │
        └──────────────────────────────┬────────────────────────────────────┘
                                       ▼
             verify: fresh verifier on the other provider, holdout scenarios
               pass ─► integrate          fail ─► fix task back to build
                                       ▼
             integrate: merge queue per repo in dependency order,
             post-merge integration checks, bisect on failure
                                       ▼
             [gate 2: accept] contract table, evidence, cost, diff ─► PRs merged
```

1. **Intent.** A person writes a brief. A spec agent drafts three things into
   the run: the spec, the **validation contract** (assertions with ids,
   check method and required evidence) and **scenarios** (end-to-end stories
   used as a holdout). Gate 1: the person edits and approves the contract.
   This is where human judgment moves; it is not removed.
2. **Plan.** A coordinator turns the spec into a task graph across the
   workspace's repositories. Each task claims assertions, names its repo and
   branch, and has a budget. A WIP limit bounds parallel workers; tasks that
   touch the same module run in order, not in parallel.
3. **Build.** Each task runs in its own worktree branch as an ordinary
   Lazybox session. Workers run tests through `lazybox run`, so every claim of
   "tests pass" has a recorded exit status, commit and full output behind it.
   Each turn is checkpointed so a bad turn can be reverted.
4. **Verify.** When a task says it is done, a **verifier** session starts with
   fresh context on the *other* provider. It gets the diff and the contract,
   not the builder's transcript, and runs the scenarios from the holdout store
   in its own worktree. Each assertion gets a verdict with evidence. A failure
   becomes a fix task for a builder; the verifier never fixes.
5. **Integrate.** A merge queue per repository lands verified branches in
   dependency order, runs integration checks after each merge (semantic
   conflicts merge cleanly and only show up here) and bisects when a batch
   fails.
6. **Accept.** Gate 2 shows the contract as a table — assertion, evidence,
   verdict — with cost and diff size. Depending on the run's autonomy level,
   the person accepts, or policy merges automatically.
7. **Learn.** CI failures, review comments and production signals become new
   tasks in the same run or a new one. The ledger keeps every decision.

## The autonomy dial

One run setting, not a separate product:

| Level | Human does | Lazybox enforces |
| --- | --- | --- |
| 3 — reviewed | Reviews every PR | Contract and evidence shown on each PR |
| 4 — gated (default) | Approves the contract, accepts the result | Verifier must pass; merge queue lands it |
| 5 — dark | Writes scenarios, watches metrics | Auto-merge on holdout satisfaction and budget |

## Data model

The mission, goal, task and attempt records, the typed event log they are
derived from, roles as configuration and the `.lazybox/process.yaml` presets
(including one per autonomy level) are specified in
[Orchestration](ORCHESTRATION.md). Only the factory-specific records stay
here:

- **Spec** and **Contract:** versioned documents; an **Assertion** has an
  id, a check, required evidence and the tasks that claim it.
- **Scenario:** holdout story, stored with the run outside every worktree,
  readable only by verifiers.

## Holdouts: honest about enforcement

Keeping scenarios away from builders is the main defence against gamed tests,
and it is the hardest part to enforce locally:

- **Soft (first version):** scenarios live in Runner state, never in a
  worktree, and are given only to verifier sessions. Builder sessions are
  denied reads of that path. A builder in YOLO mode can still read any file
  the user can, so this deters accidents, not a determined agent.
- **Strong (later):** builders run in containers or under a separate OS user
  that cannot see Runner state (roadmap: remote and container execution).

Until the strong version exists, a dark (level 5) run is not trustworthy and
Lazybox should say so.

## Failure modes and the design's answer

| Failure | Answer |
| --- | --- |
| Agents game visible tests | Holdout scenarios; verifier on another provider; read-only tests in the verifier's worktree |
| Verifier shares builder's blind spots | Fresh context, no builder transcript, different model family |
| Parallel agents conflict semantically | WIP limit; serial tasks on shared modules; integration checks after each merge; bisecting queue |
| Review loops never converge | Verifier reports verdicts on a fixed contract, not open-ended opinions; a task has a retry budget, then escalates |
| Specs not followed | Assertions are the spec that matters; unclaimed assertions block acceptance |
| Nobody understands the code | Gate 2 shows a walkthrough per task; level 4 is the default |
| Runaway cost | Per-run and per-task budgets on the metering gateway; stop and escalate at the limit |
| Long runs interrupted | Ledger plus agents that outlive the Runner; tasks resume, not restart |

## First slice

The smallest factory worth running on real work, at level 4, in one
workspace:

1. **Run and contract.** A run page: brief, contract (editable table of
   assertions), scenarios, approve. Stored in Runner state with a ledger.
2. **Coordinator tools.** The Lazybox MCP and its scoped credentials, as in
   [Orchestration §5](ORCHESTRATION.md#5-on-lazybox-v2).
3. **Evidence capture.** Record each `lazybox run` per session with commit and
   exit status (the console work in flight records most of this).
4. **Verifier role.** On submit, start a read-only session on the other
   provider with the contract and scenarios; record verdicts.
5. **Board.** Tasks with state, assertions with evidence and verdict, cost so
   far, and the two gates in the attention inbox.

This needs, from the roadmap: the attention inbox (1), verified status (2) and
the work model (7). The merge queue and PR loop (3) come right after; until
then gate 2 ends in branches ready for a pull request.

## Decisions for you

1. Default autonomy level — level 4 recommended.
2. Holdout enforcement: accept soft enforcement for the first version, or wait
   for container isolation.
3. Budget model: per run, per task, or both; hard stop or escalate.
4. Verifier provider: always the other provider, or configurable per run.
5. Where specs and contracts live: Runner state only, or committed to a
   repository (`.lazybox/factory/`) so they are reviewed like code.
