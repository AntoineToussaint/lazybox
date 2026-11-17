# Agent UX and multi-agent coordination: state of the art, and what Lazybox should build — 2026-11-16

This page surveys how the leading tools run and coordinate many coding
agents. The survey covers orchestration, conflict avoidance, attention,
review, observability, cost, the phone, and the permissions of an agent that
starts other agents. The survey ends with a concrete coordination design for
Lazybox. That design builds on what already exists:

- missions ([ORCHESTRATION.md](../../ORCHESTRATION.md));
- the review loop and `lazybox ask` ([GITHUB.md](../../GITHUB.md#the-review-loop));
- the scoped agent credentials ([RESOURCE_OWNERSHIP.md](../../RESOURCE_OWNERSHIP.md#agent-credentials));
- hooks and grants ([POWER.md](../../POWER.md)).

The companion [herdr re-audit](herdr-gui.md#re-audit-evening) lists the UI
fix briefs. Brief R2 there is the design on this page.

**Method.** The survey reads official documentation, changelogs and
engineering blogs, as of 2026-11-16, and links every claim. **[UV]** marks a
claim that rests only on secondary sources, or where sources disagree.
None of these products was run for this page.

## 1. The market moved

- **Conductor** was acquired by Meta on Nov 4. It announced a Cloud shutdown for Nov 20, then on Nov 13 posted "Conductor is back… isn't shutting down" ([0.93.0](https://www.conductor.build/changelog/0.93.0-conductor-is-joining-meta), [0.94.2](https://www.conductor.build/changelog/0.94.2-conductor-is-back), [Axios](https://www.axios.com/2026/11/04/meta-acquires-ai-coding-startup-conductor)).
- **Vibe Kanban**: its company shut down on Apr 10. The board and projects were retired, and only local workspaces survive, as a community project ([shutdown](https://vibekanban.com/blog/shutdown)).
- **Crystal** was deprecated in favour of **Nimbalyst** **[UV]** ([crystal](https://github.com/stravu/crystal), [nimbalyst](https://nimbalyst.com)).
- **Jules** has stopped taking sessions (Nov 11) and winds down on Dec 31 ([changelog](https://jules.google/docs/changelog)).
- **Windsurf** became **Devin Desktop** (Jun 2). Its Agent Command Center is the default surface, and third-party agents connect over ACP ([Cognition](https://cognition.com/blog/introducing-devin-desktop)).
- **ACP (Agent Client Protocol)** became the de facto way to drive coding agents from a GUI. 51 agents are listed. Natively: Gemini CLI, Goose, OpenCode, Kimi, Qwen, Cursor CLI, Copilot, Droid, Cline, Kiro and others. Claude Code and Codex connect through adapters ([agents](https://agentclientprotocol.com/overview/agents), [registry](https://zed.dev/blog/acp-registry)).

The lesson for a local-first tool: **users have no portable record of their
agents' work.** A tool whose missions, events and evidence are open files on
the person's machine survives the next acquisition. Lazybox already has
this, through the mission log and the journal.

## 2. Who does what

| Product | Isolation, parallelism | Orchestration shape | Conflicts | Attention | Review / verification | Spawn permissions |
|---|---|---|---|---|---|---|
| **Claude Code** ([agents](https://code.claude.com/docs/en/agents.md), [teams](https://code.claude.com/docs/en/agent-teams), [projects](https://code.claude.com/docs/en/claude-projects.md)) | Worktree per background agent; subagents capped at 20 per session, depth 3; workflows run 16 at once | Agent teams: a lead, a shared task list with dependencies, file-locked claims, mailboxes. Projects: a coordinator with cloud threads | Teammates are **not** isolated ("split work by file") | Agent view: Working / Needs input / Idle / Completed / Failed. Projects: *Ready for review / Waiting on you / Working / Landing / Resolved*. Push to phone | Inline diff comments go back to the agent; PR auto-fix on CI and review comments; `ultrareview` reproduces findings | Teammates inherit the lead's mode. **A message from another agent never grants consent.** Background subagents' prompts surface in the main session, labelled |
| **Codex** ([app](https://developers.openai.com/codex/app/features), [subagents](https://learn.chatgpt.com/docs/agent-configuration/subagents)) | Local, worktree or cloud container; Handoff moves a thread between them | Subagents with roles defined in TOML | Branch per thread | Thread list; OSC 777 notifications (Nov) | Review pane with inline comments, stage or revert by hunk; `@codex review` posts P0/P1 only | Subagents inherit the sandbox and approval mode and **cannot override them**; an approval request in a headless run fails back to the parent |
| **Cursor 3** ([3.0](https://cursor.com/changelog/3-0), [projects](https://cursor.com/blog/projects)) | Local, worktree, cloud VM or SSH; best-of-N | Projects: a coordinator that never writes code, workers, a task board, a shared file store, triggers | Branch per worker; "split into PRs" | Agents Window; iOS remote control, voice | Bugbot (78 % resolution); screenshots and videos on PRs | Asks before reaching outside the Project |
| **Copilot / Agent HQ** ([coding agent](https://docs.github.com/en/copilot/concepts/coding-agent/coding-agent), [Agent HQ](https://github.blog/news-insights/product-news/welcome-home-agents/)) | Actions runner (59-minute limit); `/fleet` up to 32 agents | One issue goes to Copilot, Claude, Codex or all three, which compete | PR per agent; one-click conflict resolution | Agents tab per repository; mission control across surfaces | Self-review before asking for yours, plus security scans | Admin allowlists |
| **Antigravity 2.0** ([2.0](https://antigravity.google/blog/introducing-google-antigravity-2/), [docs](https://antigravity.google/docs/agent/)) | Automatic worktree per subagent; depth up to 10 | Agent Manager; asynchronous subagents | Worktrees | Inbox **[UV]** | **Artifacts** (plans, screenshots, walkthroughs, recordings) that you comment on like a document; review policy per artifact | Subagents inherit the parent's permissions |
| **Devin** ([managing Devins](https://old.cognition.ai/blog/devin-can-now-manage-devins)) | A VM per child | A coordinator messages its children mid-task, tracks their usage, pauses or kills them | VM and branch per child | Agent Command Center | Devin Review; autofix of other bots' comments | Not documented |
| **Factory Missions** ([architecture](https://factory.com/news/missions-architecture)) | Fresh worker per feature; **mostly serial** | Orchestrator, workers, validators; a **validation contract** of assertions written before the features | Serial milestones | — | Validators: code scrutiny plus black-box user testing; 37 % of the time is spent validating | A model per role |
| **Warp Oz** ([orchestration](https://docs.warp.dev/agent-platform/cloud-agents/orchestration/multi-agent-runs.md)) | Cloud runs | `/orchestrate` proposes the breakdown and **children start only after you approve it**; durable messaging between agents | — | Runs page: Working / Blocked / Failed / Success; an unattended run that needs input ends *Blocked* instead of hanging | — | Children inherit auth; credit use rolls up by ancestor |
| **Amp** ([agent-to-agent](https://ampcode.com/docs/orbs/agent-to-agent), [Puck](https://ampcode.com/news/meet-puck)) | An "orb" (remote machine) per thread | Threads start threads and report back; Puck is a meta-agent | "Keep work in one thread if it edits the same files" | App notifications | Oracle second opinion | Not documented |
| **OpenHands Agent Canvas** ([canvas](https://www.openhands.dev/product/agent-canvas), [Aug update](https://www.openhands.dev/blog/new-in-agent-canvas-august-2026)) | Worktree per agent; local, VM or cloud | Parent tasks launch children; **drives Claude Code, Codex and Gemini over ACP** | — | Automation dashboard | Commits and PR drawer | `max_children` |
| **Gas Town / Beads** ([gastown](https://github.com/steveyegge/gastown)) | A crash-surviving worktree per worker | Mayor (coordinator), Polecats (workers), Witness (stall monitor), Refinery | **Workers never push to main; Refinery is a Bors-style bisecting merge queue with gates** | `gt feed` with a "problems" view | Gates in the queue | A concurrency cap |
| **OpenAI Symphony** ([spec](https://github.com/openai/symphony/blob/main/SPEC.md)) | Workspace per tracker issue | Tracker as control plane; claim states | Claims | State endpoint | A handoff state such as "Human Review" | **Tracker credentials stay host-side**; the agent gets an orchestrator tool |
| **Zed** ([parallel agents](https://zed.dev/docs/ai/parallel-agents)) | Optional worktree per thread, detached HEAD | Threads sidebar mixing native, ACP and terminal agents | — | Thread status | git | — |
| **herdr** ([core commands](https://raw.githubusercontent.com/herdrdev/herdr-core/master/docs/commands.md)) | Terminals on a resident core; no isolation | `hc run --bg` children; `send --wait`, `wait --children`; reports arrive as messages in the parent's input | None | Status from OSC 7501; an agents list sorted by urgency | None | **None.** Only nested launches of herdr itself are blocked |
| **Linear agent sessions** ([agent interaction](https://linear.app/developers/agent-interaction)) | — | An issue delegated to an agent opens a session; typed activities | — | **State derived from the agent's last activity**; stale after 30 min; the agent must acknowledge within 10 s | — | The delegate is the agent; the human stays assignee |

## 3. State of the art, by dimension

- **(a) Isolation.**
  - A worktree per local agent is universal, and a container or VM per cloud task is standard.
  - Concurrency caps run from 6 to 32, and nesting caps from depth 1 to 10.
  - Zed archives and restores a thread's worktree with the thread.
- **(b) Orchestration.** Three shapes have converged:
  1. **A coordinator that never writes code**, delegating to fresh-context workers that each return one result: Cursor Projects, Claude Projects and teams, Devin, Factory.
  2. **The tracker as control plane**: Symphony, Linear, Gas Town convoys.
  3. **A conversational meta-agent**: Amp Puck, Warp `/orchestrate`.

  The best gate is Warp's: **the person approves the breakdown before anything spawns.**
- **(c) Conflicts.** Prevention is structural: a branch per task, partitioned ownership, serial writes.
  - **Nobody ships file claims on code.** Claude's locks cover task claims only.
  - Cursor measured that locks slowed 20 agents to the throughput of 2–3; optimistic concurrency worked better ([Cursor](https://cursor.com/blog/scaling-agents)).
  - The strongest merge story is Gas Town's bisecting queue with gates.
  - Advisory leases exist only in side projects ([MCP Agent Mail](https://github.com/Dicklesworthstone/mcp_agent_mail)).
- **(d) Attention.**
  - The agent *reports* its state; nobody guesses it from the screen. Examples: OSC 7501, Linear activities, Warp *Blocked*, and ACP v2's `state_update: running | idle | requires_action` ([v2 RFD](https://agentclientprotocol.com/rfds/v2/overview)).
  - The most complete grouping is Claude Projects': *Ready for review / Waiting on you / Working / Landing*.
  - "Done" means *not yet seen* (herdr).
  - Claude, Codex and Cursor push to the phone.
- **(e) Review.** Four patterns stand out:
  - Verify before the human looks: Copilot self-review, `ultrareview` reproducing each finding, Factory validators.
  - Evidence artifacts: videos, screenshots and walkthroughs (Cursor, Antigravity).
  - Inline comments routed back to the agent (Codex, Claude, Antigravity, Conductor).
  - Bot-to-agent autofix loops (Devin, Bugbot).
- **(f) Observability.**
  - Per-agent drill-down exists: Claude `/workflows`, Copilot session logs, Warp's run tree, Gas Town's feed, OpenTelemetry from Claude and Factory.
  - **No product shows a causal timeline across parent, children and messages.**
- **(g) Cost.**
  - Cost is shown per thread or run (OpenHands, Warp rollups) and estimated before some runs (`ultrareview`, Claude workflow warnings).
  - **Budgets enforced per task or per agent tree: nobody.**
- **(h) Remote.**
  - Native phone apps over Tailscale/SSH (herdr) or vendor clouds; QR pairing (Codex, Conductor); voice (Cursor).
  - herdr has no push ("banner while the app is open"; [mobile](https://herdr.dev/mobile)).
- **(i) An agent that starts agents.**
  - Children inherit the parent's sandbox and permissions and cannot raise them (Codex, Antigravity).
  - Prompts surface to the person, labelled with their source (Claude).
  - An agent's message cannot carry consent (Claude).
  - Credentials stay host-side (Symphony).
  - **Nobody attenuates capabilities**, meaning a child getting a *subset* of tools, paths, budget and depth, auditable.

## 4. What the evidence says

- **Writers in parallel fail; readers in parallel work.** Cognition keeps writes single-threaded and adds review loops, which find about 2 bugs per PR, 58 % severe ([Cognition, Apr 2026](https://cognition.com/blog/multi-agents-working)). Cursor: locking failed, a planner–subplanner–worker tree worked, and the integrator and judge roles became bottlenecks and were removed ([Cursor](https://cursor.com/blog/self-driving-codebases)).
- **Review is the binding constraint.**
  - Practitioners cap out at 3–5 parallel agents ([Osmani](https://addyosmani.com/blog/cognitive-parallel-agents/); Claude's docs recommend 3–5 teammates).
  - Going from 2 to 12 agents moved the bottleneck to review ([Mergify](https://mergify.com/blog/how-agents-work-with-parallel-agents)).
  - METR could not measure a reliable speed-up, partly because concurrent agents broke its time logging ([METR, Feb 2026](https://metr.org/blog/2026-02-24-uplift-update/)).
- **Validation dominates.** Factory spends 37 % of mission time validating and 2 % orchestrating. Fix work is about a third of implementation ([Factory](https://factory.com/news/missions-architecture)).
- **Tokens.** Multi-agent research used about 15× the tokens of chat ([Anthropic](https://www.anthropic.com/engineering/multi-agent-research-system)). Claude teams use about 7× ([costs](https://code.claude.com/docs/en/costs.md)).
- **No controlled study** shows parallel coding agents beating one agent plus a review loop. Everything beyond Factory's case study is vendor metrics.

## 5. Gaps nobody fills, and where Lazybox can lead

| Gap | Why nobody fills it | Lazybox's head start |
|---|---|---|
| **Trusted "done"**: status backed by what actually ran | Products report the agent's claim | Missions already derive *claimed / observed / verified / accepted* from `lazybox run` evidence, checks and gates |
| **Capability attenuation** for agents that spawn agents | Everyone inherits the parent's permissions or does nothing | Per-execution HMAC credentials, agent and hook allowlists walked by tests, and owner-only human gates |
| **Work sized to review capacity** | Products launch work blindly | The Inbox already counts what waits for acceptance; a WIP limit can be tied to it |
| **Live overlap detection** between running agents | Worktrees hide overlap until merge | Per-turn checkpoints know every attempt's changed files and hunks as they happen |
| **A reviewable record of agent-to-agent messages** | Messages are opaque (Amp, Warp) or racy files (Claude mailboxes) | `lazybox ask` is already structured, rate-limited and labelled in both transcripts |
| **Budgets enforced per task or agent tree** | Cost is only displayed | The metering gateway already sits in every request |
| **A causal timeline across agents** | Per-session logs only | Mission events already carry `caused_by` and actor |
| **Durable, portable state** | Cloud state; shutdowns | An append-only JSONL mission log on the person's machine |
| **One inbox across agent vendors** | Each speaks its own dialect | One Inbox for Claude and Codex today; ACP adds the long tail |

## 6. A coordination design for Lazybox

### 6.1 Principles

1. **Rules before agents, agents before people, people at the gates.** This is ORCHESTRATION §3, kept.
2. **An orchestrator coordinates; it never writes code, never consents, and never judges its own work.** Write, consent and accept are three separate powers, held by the worker, the person and the person respectively.
3. **Capabilities only shrink down the tree.**
   - A child's grant is a subset of its parent's: tools, paths, permission level, budget, depth, fan-out.
   - The Runner enforces this.
   - No message, file or prompt can widen it.
4. **Parallel readers, serial writers per path.** Leases are advisory. Overlap is *detected live* and *serialised at merge*. Locks are never taken on edit, because Cursor measured the collapse.
5. **Throughput is capped by review.** The system never starts more work than the person can accept.
6. **Everything is an event** in the mission log, attributed to its actor. The Timeline is a fold of that log, so are the board and the costs.

### 6.2 Data model

These records extend the slice-1 mission log (`internal/work`) and add no
new store. New event types are shown in `code`.

| Record | Fields | Events |
|---|---|---|
| **Grant** | `id`, `mission`, `holder` (execution), `parent_grant`, `scopes[]`, `workspaces[]` (the ones it may create inside), `paths` (globs it may lease), `permission_ceiling` (ask, read-only or yolo, never above the parent's), `budget` ($, turns, wall time), `depth`, `fan_out`, `expires` | `grant.issued`, `grant.narrowed`, `grant.revoked` |
| **Lineage** | `parent_execution`, `child_conversation`, `grant`, `task`, `started_by` (rule, orchestrator or person) | `attempt.started` gains `parent` and `grant` |
| **Plan proposal** | `tasks[]` (brief, role, provider and model, paths, `blocks` / `merge_after` edges, estimate in $ and minutes), `fan_out`, `total_estimate` | `plan.proposed`, `plan.approved` (owner only, with a $ cap and a WIP limit), `plan.rejected`, `plan.amended` |
| **Lease** | `path_glob`, `repo`, `holder` (task), `mode` (`exclusive` or `shared`), `ttl`, `reason` | `lease.claimed`, `lease.released`, `lease.expired`, `overlap.detected` (two attempts' changed hunks intersect, or a hunk falls inside another task's exclusive lease) |
| **Message** | `from` (execution), `to` (execution), `kind` (`ask`, `answer`, `note`, `report`), `text` (≤ 4 KB), `refs[]` (evidence ids), `in_reply_to`, `delivered_at`, `answered_at` | `message.sent`, `message.delivered`, `message.answered`, `message.refused` (with the reason) |
| **Budget** | per mission, task and grant: `limit`, `spent`, `reserved` | `budget.reserved` (at spawn, from the estimate), `budget.warning` (80 %), `budget.exhausted` (spawning refused; running agents pause at turn end) |
| **Merge train** | ordered `entries[]` (task, PR, head), `state` per entry, `bisect` | `integration.queued`, `integration.merged`, `integration.failed`, `integration.reverted` (ORCHESTRATION §2, unchanged) |

Derived:

- **Review load**: tasks *claimed done* plus tasks *awaiting acceptance* plus open findings assigned to the person.
- **Tree cost**: the sum over lineage.
- **Overlap map**: per repository, which tasks touched which paths and hunks.

### 6.3 The Orchestrator permission

An Orchestrator is a conversation started with a **grant** whose scopes
include `session.spawn`. Its caller gives it a mission and a ceiling. What
the grant allows:

| May | Never |
|---|---|
| `plan.propose` (a proposal waits for the person) | Spawn before `plan.approved`, or beyond the approved fan-out or $ cap |
| Create task workspaces (new branches) inside the mission's repositories | Add a repository; it may only *request* one, as `lazybox workspace request-repo` does today |
| Start conversations for approved tasks, with a role binding and a grant ⊆ its own | Give a child a wider permission, more paths, more budget or more depth than itself |
| `send` / `ask` / `wait` / `read` / `interrupt` / `stop` **only on conversations in its own lineage** | Touch any conversation it did not start, or the person's |
| `lease.claim` / `release` for its tasks' paths | Take an exclusive lease that conflicts with another mission's |
| `task.create`, `task.edge`, `decision.record`, `decision.request`, `gate.request`, `evidence.read` | `gate.pass`, `task.accept`, `Respond*` (approvals and questions), permission changes, settings, files outside its workspaces, merges |
| Read its children's evidence: compact output, diffs, verdicts | Read holdouts (factory preset) |

Enforcement:

- The grant table sits beside the agent allowlist (`internal/transport/auth.go`). Every RPC checks the caller's grant *and* the target's lineage.
- A test walks every RPC, as `TestHookCredentialReachesOnlyItsGrantsWhileItRuns` does.
- **Approvals from children route to the person, never to the orchestrator.** They appear in the Inbox labelled "from *worker 2* · started by *Orchestrator* · mission *X*" (Claude's labelling).
- A child's question routes to the orchestrator only when it is a `task.question`. Permission requests always go to the person.
- An agent's message carries no authority. The Runner never interprets text as consent; only the person's RPCs answer requests.

### 6.4 Agent-to-agent messaging

This generalizes `lazybox ask`, which today works only between a review's
author and reviewer.

- **Verbs:** `lazybox agents send|ask|wait|read`. `ask` blocks up to `--timeout`, as `lazybox ask` does now, and answers that arrive later come back as a labelled message.
- **Addressing:** by lineage only.
  - A parent reaches its children.
  - A child reaches its parent.
  - Siblings reach each other only through the parent, with one exception: a reviewer ↔ author pair inside one review.
- **Delivery:**
  - The message enters the receiver's conversation as a *labelled* prompt with source `role{mission, from}`, and is recorded as `message.sent`. ORCHESTRATION §6 already records prompt sources.
  - It is held while the person has an unsent draft in that conversation, as herdr's `draft_wait` does.
- **Limits:**
  - 4 KB per message, with references instead of pasted output (Anthropic's lesson).
  - At most N messages per minute per pair.
  - One open `ask` per direction.
  - Ask chains keep V1's `MAX_ASK_DEPTH`.
- **Visible:** each mission has a **Messages** tab, a thread view of every message with its evidence links, so a person can audit "who told whom what".

### 6.5 Conflict avoidance: lease, detect, serialise

1. **Plan time.** Each proposed task names its `paths`. The plan sheet shows overlaps between tasks before approval, and the process's `overlapping_paths: serial` turns an overlap into a `blocks` edge.
2. **Run time.**
   - Per-turn checkpoints already record each attempt's changed files and hunks. The Runner intersects them across the mission's live attempts after every turn.
   - An intersection, or an edit inside another task's exclusive lease, raises `overlap.detected`. That shows as a chip on both board cards and as a note to the orchestrator, which decides whether to sequence, merge or hand off.
   - Nothing is blocked mid-edit.
3. **Merge time.**
   - A merge train in `merge_after` order on the PR loop: rebase, checks, merge, post-merge checks.
   - A red post-merge check bisects and opens a fix task (Gas Town's Refinery).
   - Workers never push to the base branch; only the train merges.

### 6.6 Attention and review capacity

- One Inbox, ordered:
  1. awaiting acceptance;
  2. needs input (including children's approvals, labelled with their lineage);
  3. plan approval;
  4. failed;
  5. overlap;
  6. ready for review;
  7. working.
- **Review capacity.**
  - The process sets `wip.review` (default 3). When review load reaches it, the orchestrator's spawns are held (`wip.held`) and the Inbox says "3 tasks wait for your review; new work is paused".
  - Starting more work becomes a decision the person makes, not something that happens to them. No product does this today.
- **Phone.** Plan approval, accept or ask for changes, and approvals are one tap; push carries the kind, never the transcript text (herdr-gui R7p).

### 6.7 Review and verification

This is ORCHESTRATION §3, made concrete with what shipped:

- A worker reports with evidence: `lazybox run` exit codes, the commit, and checks.
- A rule starts a **verifier on the other provider**, with a fresh context and the diff plus the assertions, never the transcript.
- The **review loop** runs: findings, fixes, re-review of the fixing turn, with a round limit.
- **Acceptance stays with the person.** Antigravity-style *artifacts* (a walkthrough, screenshots from the agent browser) attach to the task as evidence, so that acceptance is a read, not an investigation.

### 6.8 Observability

- **Timeline:** one mission view showing every event across every agent, with lanes per agent and arrows for `caused_by` (spawned, messaged, verdict, overlap). Click any event to open its turn, run or diff. Nobody has this; the event envelope already supports it.
- **Tree:** the Agents panel nests by lineage (orchestrator > workers > sub-agents), each row with state, age and tree cost.
- **Export:** the event log as OpenTelemetry spans (`mission → task → attempt → turn`), opt-in, for people who already run a tracing backend.

### 6.9 Cost

- **Estimate first.** The plan proposal estimates each task, from the role's model price and its history of turns. The person approves with a cap.
- **Reserve at spawn.** The gateway refuses a request that would cross a grant's remaining budget. At 80 % it warns; at 100 % running agents finish their turn and pause (`budget.exhausted`), and the Inbox asks for more or a stop.
- **Show the outcome.** Each accepted task shows its cost, and each mission its cost per accepted task (the audit README's "cost per outcome").

### 6.10 Any CLI agent

Workers can be ACP agents (herdr-gui R3), so a mission can mix Claude,
Codex, Gemini CLI, Goose or OpenCode workers. ACP's
`session/request_permission` maps onto the same approval routing, and
`usage_update` onto the same budgets. The verifier rule
`provider: other_than_worker` then has more than two choices.

### 6.11 UI

- **Missions as a main-area page, not a side panel.**
  - Board columns: Ready, Working, Verifying, Needs you, Landing, Done.
  - Cards show trust badges, overlap chips and cost.
  - Tabs: **Board**, **Tree**, **Timeline**, **Messages**, **Leases** (a path-by-task map), **Budget**.
- **Plan approval sheet:**
  - the proposed task graph, with paths and overlaps highlighted;
  - the estimate;
  - fan-out and WIP sliders, a $ cap;
  - Approve / Edit / Reject.

  Editing a task's brief or paths is direct. Approving records `plan.approved` with the person as actor.
- **Orchestrator card** in the chat header: its grant (scopes, remaining budget, depth, children n/m), with "Narrow…" and "Revoke".
- **Inbox rows from children** carry a lineage crumb: *Orchestrator › Worker 2*.
- **Keyboard:**
  - `]] m` opens Missions.
  - In the board, `a` accepts, `r` asks for changes, `p` approves the plan.
  - ⌘J still means "next thing that needs you".

### 6.12 CLI

```text
lazybox mission plan <mission> --file plan.yaml      # orchestrator: propose
lazybox mission approve <mission> --cap 20 --wip 3   # person: approve (owner only)
lazybox agents spawn --task T --role worker [--provider P] [--model M]
lazybox agents ls [--mine]                           # orchestrator: its lineage
lazybox agents send|ask <agent> "text" [--timeout 90s]
lazybox agents wait <agent|--children all|any> --until idle|needs-input|reported
lazybox agents read <agent> [--last N]               # compact transcript + evidence refs
lazybox agents stop <agent>
lazybox lease claim <path-glob> [--exclusive] [--ttl 2h] · release · ls
lazybox mission timeline <mission> [--json]
lazybox budget [<mission>]
```

The same verbs are MCP tools for agents that prefer MCP (ORCHESTRATION §5).
`wait` takes a cursor and returns a snapshot on a gap, as herdr's
`events_lost` lesson teaches.

### 6.13 Permissions matrix

| Verb | Person | Orchestrator | Worker | Verifier / reviewer | Hook (grants) | Phone (grants) |
|---|---|---|---|---|---|---|
| Approve, answer, permission change | ✓ | ✗ | ✗ | ✗ | `respond` (approve/answer) | `respond` |
| Pass gate, accept task | ✓ | ✗ | ✗ | ✗ | ✗ | ✓ (owner device) |
| Approve plan, raise cap | ✓ | ✗ | ✗ | ✗ | ✗ | ✓ |
| Propose plan, create task | ✓ | ✓ | ✗ | ✗ | ✗ | ✗ |
| Spawn conversation | ✓ | ✓ approved tasks, grant ⊆ own | ✗ (provider sub-agents only) | ✗ | `start` | `start` |
| Send / ask / wait / read | ✓ any | ✓ own lineage | parent only (`ask`) | its review pair only | `respond` / `read` | `respond` / `read` |
| Interrupt / stop | ✓ | ✓ own lineage | own commands | own commands | `respond` | `respond` |
| Lease claim | ✓ | ✓ its tasks' paths | ✓ its task's paths, shared only | ✗ | ✗ | ✗ |
| Report, evidence | — | ✗ | ✓ own task | verdict / finding | ✗ | ✗ |
| Merge | ✓ | ✗ | ✗ | ✗ | ✗ | ✗ (the train merges) |

### 6.14 Slices

| # | Slice | Acceptance | Size |
|---|---|---|---|
| 2a | **Grant table and lineage**, with `agents send/ask/wait/read/stop` on own lineage, and `task_report` for workers (ORCHESTRATION slice 2) | The RPC walk test; a worker reports with `run:` evidence and its task becomes *claimed* with no human step | M |
| 2b | **Plan proposal, approval sheet, spawn under the cap** | Playwright: propose → approve with a $ cap → three workers in their own workspaces; a fourth spawn is refused over the fan-out | M |
| 2c | **Missions page:** Board, Tree, Messages; lineage in the Agents panel and the Inbox | Screenshots at 1440 and 1024, dark and light; a child's approval is labelled and lands with the person | M |
| 2d | **Budgets** reserved at spawn and enforced in the gateway; review-capacity WIP | A grant at 100 % pauses its workers at turn end; `wip.review = 2` holds the third spawn with an Inbox row | M |
| 3a | **Leases and live overlap** from checkpoints | Two workers edit the same hunk, and `overlap.detected` appears on both cards within one turn | M |
| 3b | **Merge train** in `merge_after` order, with post-merge checks and bisect (ORCHESTRATION slice 3) | Three PRs land in order; a breaking one is bisected out and a fix task opens | L |
| 4 | **Timeline** and OpenTelemetry export | Every event of a test mission appears in order, with `caused_by` arrows | M |

Slices 2a–2c close the herdr re-audit's row 39 ("agent drives agents") with
a safety model herdr does not have. Slices 2d–3a are what nobody ships.

## Sources

Product docs and changelogs are linked inline. Research and evidence:
[Anthropic multi-agent research system](https://www.anthropic.com/engineering/multi-agent-research-system) ·
[Cognition, "Multi-agents: what's actually working"](https://cognition.com/blog/multi-agents-working) ·
[Cursor, "Scaling agents"](https://cursor.com/blog/scaling-agents) ·
[Cursor, "Self-driving codebases"](https://cursor.com/blog/self-driving-codebases) ·
[METR uplift update, Feb 2026](https://metr.org/blog/2026-02-24-uplift-update/) ·
[Factory missions architecture](https://factory.com/news/missions-architecture) ·
[Addy Osmani on parallel agents](https://addyosmani.com/blog/cognitive-parallel-agents/) ·
[Mergify on parallel agents](https://mergify.com/blog/how-agents-work-with-parallel-agents) ·
[ACP overview](https://agentclientprotocol.com/protocol/overview) ·
[ACP v2 RFD](https://agentclientprotocol.com/rfds/v2/overview) ·
[ACP updates](https://agentclientprotocol.com/updates.md) ·
[awesome-agent-orchestrators](https://github.com/andyrewlee/awesome-agent-orchestrators).

**Not verified:**

- The Antigravity inbox and its 5-agent limit.
- Codex `max_threads` and `max_depth` defaults, and the CLI `codex agents` / `codex queue`.
- The Devin Desktop kanban states.
- Whether Sculptor's Pairing Mode still exists.
- That the Claude ACP adapter bundles its own CLI.
- The Crystal deprecation date.
- The press coverage of Conductor's reversal (only its changelog says so).
