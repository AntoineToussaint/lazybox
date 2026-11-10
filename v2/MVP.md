# Lazybox V2 MVP specification

**Scope update, 2026-11-09:** the first release supports development from a phone,
an available remote Runner and continuity across iPad/desktop. The immediate
gate is the [device acceptance journey](DEVICE_EXPERIENCE.md#acceptance-journey);
[Roadmap](ROADMAP.md) defines delivery order. The Hub belongs in Obin platform
V2: integration is required, implementation here is excluded. The two-human,
multi-Runner scenarios below remain the broader collaborative target. They do
not defer mobile or all need to pass before the one-human phone journey ships.

The MVP proves that two humans and two agents can complete one shared software change across devices, using independent desktop windows, accessible visual evidence, and persistent remote execution. It includes the Desktop, Hub, and Runner boundaries from the first working build.

The MVP is complete when the reference journey works end to end and the failure scenarios below have recorded evidence. Existing V1 feature count is not a release criterion.

The [selected stack](STACK.md) shares TypeScript contracts/application logic,
React desktop/web surfaces and selects Expo/React Native for mobile. Device
composition and real mobile lifecycle tests are required; sharing desktop markup
is not a mobile delivery strategy. V2 805's older sequencing is superseded.

## Minimum topology

| Element | Minimum requirement |
| --- | --- |
| Humans | Two individually authenticated people with distinct authorship and personal view state. |
| Desktops | Two macOS computers; a person can leave one and sign in on another. |
| Windows | At least three independent windows for work, application preview, and review or execution. |
| Hub | One durable service, exercised both locally and on a remote Linux host. |
| Runners | Two enrolled Runners, exercised on developer and remote hosts; each successfully performs an execution. |
| Agents | Two simultaneous agents with separate assignments; proposed adapters are Claude Code and Codex. |
| Work | One project, two repositories, linked existing tracker records, and a dependency graph. |
| Evidence | A running application preview, annotated screenshot, code diff, and recorded test result. |

The continuity scenario places the required continuing work on the remote Runner. Sleeping a developer machine cannot preserve processes on that machine; the Hub must report that distinction accurately.

## The reference project

Use a small frontend and backend application maintained for integration testing. A change adds a backend contract and a corresponding visible frontend behavior. The project has a deterministic startup command, test commands, discovered service endpoints, and isolated test data for each execution.

Both agents receive bounded assignments. They exchange a contract and at least one question through shared work records. A second human supplies visual feedback, which an agent resolves. A human reviews code and evidence before explicit source control integration.

Use existing tracker records in the manual dogfood project. The Hub links them to its internal work model; tracker state and execution state remain distinguishable. Automated tests use fixtures or controlled test repositories rather than creating records in a contributor's normal projects.

## Required capabilities

### Shared work and identity

- Create and edit objectives, acceptance criteria, work items, assignments, and directed blocking dependencies.
- Represent distinct humans, agents, and execution attempts with stable identities.
- Enroll a second human and Runner, revoke access, and enforce the small maintainer/contributor role model.
- Record the actor, revision, cause, and outcome of shared mutations.
- Show ready, active, held, awaiting input, awaiting review, accepted, failed, and canceled work through defined transitions.
- Reject dependency cycles and stale conflicting edits with actionable feedback.
- Link the two repositories and existing GitHub work records without duplicating their workspaces.

### Hub and Runner execution

- Run the Hub independently of all Desktop windows.
- Register Runners with authenticated identities, capabilities, availability, and capacity.
- Select a Runner explicitly, reserve capacity, queue work, and start an execution with a durable identity.
- Provision the repository/worktree and launch an audited structured agent adapter.
- Observe progress, ask and answer questions, make permission decisions, interrupt, and inspect failure evidence.
- Distinguish turn completion, process exit, result submission, acceptance, and integration.
- Deduplicate repeated start delivery and reconcile Runner state after disconnection or Hub restart.
- Preserve completed outputs until their durable artifact transfer succeeds.
- Preserve the [log-pipe workflow](EXECUTION.md): an agent can publish shell output to a human console while retaining direct or queryable access to that output. Register one command stream; never rerun a command to supply another viewer.
- Support managed command wait/read/cancel and actual process exit reporting. A pipe-only capture reports EOF separately from unknown producer exit status.
- Open live consoles in separate windows and on another authorized computer; retain output and report capture gaps or publication failures explicitly.

### Agent and human collaboration

- Let both supported agent adapters read their assignment, dependencies, relevant decisions, and bounded context.
- Publish a finding or interface contract and retrieve another agent's contribution.
- Ask a decision question, resolve it as an authorized human, and notify the affected assignments.
- Hand work or feedback to another participant with the required evidence references.
- Submit a structured result and expose pending or failed notification delivery.
- Keep important state changes visible to both humans without manual status summaries.

### Smart Desktop and multiple windows

- Open work graph, work detail/execution, preview, and review as panes or independent native windows.
- Maintain independent navigation, selection, scroll position, and focus for each window.
- Update shared records across windows without redirecting another window's navigation.
- Show ownership, dependencies, blockers, decisions, review status, and Runner uncertainty visually.
- Restore personal open surfaces after restart and on another computer, adapting to the available displays.
- Show changes since the person's last acknowledged visit and direct attention to unresolved questions or failures.
- Preserve local drafts through a disconnect and make their unsent state visible.
- Provide usable keyboard navigation and readable focus behavior in the required surfaces.

### UI development and visual evidence

- Open a real application running on a Runner, including a remote Runner.
- Keep navigation, assets, interactive requests, and development WebSocket traffic working through the preview route.
- Capture a screenshot with source revision and preview context.
- Add a pinned or rectangular annotation and a discussion linked to a work item.
- Assign that feedback to an agent, receive a revision, and resolve or reopen the finding.
- Keep saved screenshots accessible after the producing Runner disconnects.
- Show a live preview as unavailable when its service is unavailable, independently of saved evidence.

### Review and integration

- Inspect diffs and test evidence tied to a source revision.
- Record findings, responses, and human acceptance.
- Mark affected review evidence stale when source changes.
- Show what prevents integration, including upstream dependencies, unresolved findings, and provider failures.
- Support an explicit authorized integration action for the initial GitHub path and record its outcome.
- Reconcile an uncertain provider response before offering a retry that might repeat a side effect.

## Deferred capabilities

These remain on the roadmap after the MVP:

| Capability | MVP boundary |
| --- | --- |
| Federation of Hubs | One authoritative Hub per collaboration space. |
| Automatic execution migration | Reconcile or explicitly restart an execution; preserve attempt history. |
| Offline shared editing | Cached reads and durable local drafts; shared writes require connection. |
| Rich simultaneous document editing | Versioned edits and explicit conflicts; no general CRDT editor. |
| Remote control of another person's layout | Independent views; optional follow mode later. |
| Autonomous organization management | Bounded agent collaboration and explicit integration authority. |
| Full provider parity | Initial GitHub path; Linear, Jira, and Slack follow audited seams. |
| Every V1 feature and data migration | Selective reuse and a later migration strategy. |
| Broad mobile IDE parity | Phone development and iPad adaptation are required now; complete desktop IDE parity is not. |
| Windows Desktop | Proposed first desktop platform is macOS. |
| Arbitrary untrusted workloads | Trusted team code on explicitly authorized Runner hosts; broader sandboxing follows. |
| Full spatial canvas and design editor | Required graph, preview, screenshot annotation, and review surfaces first. |
| Frame-accurate shared browser sessions | Independent access to the preview; screen sharing and joint browsing later. |

## Acceptance scenarios

Every scenario records the build, topology, actor identities, source revisions, result, and evidence. A simulator-only result cannot satisfy a real-machine or real-agent requirement.

### A01 Complete shared work

Two humans create and inspect the same project. Two agents perform related work across the two repositories, exchange a contract and a question, and submit evidence. Visual feedback from the second human becomes assigned work. Review resolves the findings and an authorized human integrates the result in dependency order.

Pass: the work, authors, decisions, evidence, and integration outcome can be reconstructed from the product without asking the participants to reconstruct the history.

### A02 Independent windows and screens

Open the graph, preview, and review in separate windows. Select different work in each. Modify a shared item in one window, close another, move a window between displays, then relaunch with fewer displays available.

Pass: shared changes appear consistently; navigation stays independent; closing windows stops no execution; useful surfaces restore on visible displays.

### A03 Continue from another computer

Human A starts work on computer A while Human B contributes. A closes the Desktop and signs in on computer B. Remote agents continue during the absence.

Pass: A can access the same shared work and saved evidence, recover personal surfaces, identify B's intervening actions, and answer an outstanding question. No local path on computer A is needed.

### A04 Collaborate during conflicting edits

Two humans edit the same work revision and attempt conflicting ownership or decision changes. Retry an accepted command after simulating a dropped response.

Pass: no accepted edit disappears silently; the stale edit is actionable; the retry returns the original result without a second mutation; authorship is correct.

### A05 Runner connection loss and reassignment

Disconnect a Runner while its agent remains alive. Let its observation become stale. Attempt a second dispatch, reconnect the Runner, and reconcile a completion produced during the outage.

Pass: the Hub shows uncertainty, prevents a duplicate writer, and reconciles the original attempt. Old assignment authority cannot integrate after explicit reassignment.

### A06 Hub restart and duplicate starts

Restart the Hub during active execution and replay a start request whose acknowledgement was lost. Reconnect both Desktops and Runners.

Pass: durable work and decisions survive; each execution starts at most once for the accepted attempt identity; clients recover consistent state without losing an acknowledged mutation.

### A07 Process failure and user decisions

Exercise a startup failure, a mid-run crash, a blocked permission request, an interrupted execution, and a task whose turn ends without acceptance.

Pass: each case has an accurate state and actionable evidence. Another human can answer the permitted question. A finished turn never marks unaccepted work as accepted.

### A08 Portable preview and evidence

Open a remote preview from both computers. Exercise navigation and development updates. Capture and annotate a screenshot, upload it, then stop the preview service and disconnect its Runner.

Pass: saved evidence remains accessible; preview availability changes accurately; feedback retains its work and revision anchors; application content cannot invoke privileged Desktop commands.

### A09 Revision aware review and integration

Review a revision, change the code, and attempt acceptance or integration using stale evidence. Exercise a provider failure and a dropped integration response.

Pass: the changed revision is visible, stale review cannot silently authorize a new revision, dependencies hold integration, and ambiguous provider outcomes are reconciled before retry.

### A10 Membership and scope

Have a contributor attempt a maintainer-only action; revoke a human device and a Runner; attempt cross-project access using an agent's scoped credential.

Pass: operations enforce the defined authority, revoked credentials stop working within the documented bound, and records retain the actual authenticated author.

### A11 Slow clients and recovery

Throttle a Desktop while a Runner emits sustained output and the other Desktop changes shared work. Expire the slow client's replay cursor and reconnect it.

Pass: process output does not block shared work; buffering remains bounded; the slow client resynchronizes to authoritative state; another client's interaction remains usable.

### A12 Local deployment

Run the reference application with Desktop, Hub, and Runner on one computer. Close the Desktop, reopen it, and restart the services separately.

Pass: local deployment uses the same contracts, preserves independent service lifetimes, and keeps V1 data isolated.

### A13 Shared shell output

An agent starts a real command emitting incremental output and a known failure. Publish it through the log-pipe interface while the agent reads its output. Both humans open the same stream in independent console windows. Close and reopen a console, disconnect a viewer, and simulate a publication interruption.

Pass: the producer runs once; the agent retains useful output access; viewers receive the retained stream and can resume from offsets; closing a view does not stop the command. Capture gaps and publication failures are visible. Managed execution reports the producer's actual failure; pipe-only capture does not invent an exit status.

### A14 Bidirectional agent communication

For each supported agent family, exchange follow-ups and an image, answer a real question and approval through their correlated protocol requests, queue another message, interrupt work, and exercise restart/resume. Have both humans attempt to answer the same pending request.

Pass: the provider receives the intended messages and one authorized answer; queued, delivered, interrupted, and resolved states reflect actual acknowledgements. Resume and pending-request recovery limits are recorded. An ordinary chat message cannot masquerade as a protocol approval.

## Scale and responsiveness

The first product journey uses two real agents. The architecture must also be exercised at a larger simulated fleet size before MVP release.

Proposed initial benchmark: one project with 1,000 work items, 20 active simulated executions across two Runners, two connected Desktops, and three windows per Desktop. Include artifact transfers and sustained execution output. Record event rates and payload sizes alongside latency and memory; agent count alone is insufficient.

Proposed targets, to calibrate and freeze in M0:

- Local window navigation and selection respond within 100 ms at p95 under the benchmark workload.
- An acknowledged shared mutation appears on another healthy Desktop within one second at p95, on the declared test network.
- Reconnect restores an actionable shared view within five seconds at p95 on that network and dataset.
- Control queues, process buffers, and artifact transfer buffers have explicit bounds; a 30-minute steady workload does not produce continuing unbounded growth.

Measure actual runs, hardware, network conditions, repetitions, and exclusions. Simulated execution tests establish control-plane behavior; two real-agent dogfood runs establish adapter and workflow behavior. Neither measurement substitutes for the other.

## MVP release gates

1. A01 through A14 have passing evidence, including real computers and agents where specified.
2. Every reused V1 component and its admitted dependency boundary has an approved [audit record](REUSE_AUDIT.md).
3. Core state transitions, idempotency, conflict handling, Runner fencing, replay, and recovery have automated regression coverage.
4. Native multi-window behavior, display restoration, preview interaction, and two-person usability have a recorded manual pass.
5. The benchmark has a reproducible profile, measurements, and an explicit decision on any missed target.
6. Installation, enrollment, revocation, logs, backup/restore, and service restart have been exercised on the supported topology.
7. Open defects are triaged; failures that corrupt work, lose accepted decisions, bypass authority, or duplicate writers block the MVP.
8. Limitations and unsupported cases appear in product behavior and release notes where they affect use.

## What the first demo must show

A person opens an existing objective, sees two agents collaborating, inspects a live application, assigns annotated feedback, closes the app, and continues from another computer while a second person contributes. They can review and finish the work from that shared context.

A fixture-driven visual prototype is useful during M0 and M1. It is labeled as a prototype and does not satisfy this demo or the MVP acceptance gates.
