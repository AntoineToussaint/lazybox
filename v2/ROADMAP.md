# Lazybox V2 delivery roadmap

The roadmap builds one complete collaborative workflow, validates it, and then expands its scale and reach.

V2 008 now has a runnable Codefly Runner implementation. [Runtime evidence](RUNTIME_EVIDENCE.md) records passing checks and remaining live Claude/multi-profile gates; the package is not marked complete merely because its code runs. The [MVP specification](MVP.md) defines the first release boundary. Every reused V1 component passes the [reuse audit](REUSE_AUDIT.md) before admission.

## Current delivery order — development from a phone, 2026-11-09

The [implementation package plan](IMPLEMENTATION_ROADMAP.md) now defines owners,
Codefly/Obin reuse, dependencies, acceptance gates and implementation status for
P00–P12. P01 starts with a Codefly-managed Working Runner and independent client
attachment; this does not complete crash recovery or remote/mobile admission.

The goal is **complete real development work from a phone**, with appropriate
iPad, laptop and multi-display experiences over the same durable work. Mobile
interaction and remote continuity are requirements now. The Hub is implemented
in **Obin platform V2**; Lazybox owns its clients and Runner and must validate
their integration with it.

The [November 9 audit](audits/2026-11-09/README.md) distinguishes main source,
the unapplied coordination candidate, passing checks and reproduced defects.
[Device experience](DEVICE_EXPERIENCE.md) defines the proposed shared model,
device-specific flows and complete acceptance journey.

| Order | Deliverable | Owning boundary | Admission evidence |
| --- | --- | --- | --- |
| R0 Portable contract and UX baseline | Phone Inbox/Work/review, adaptive iPad, desktop panels; work/assignment/result/attention identities and a two-repository fixture. | Shared client core and platform hosts; contract agreed with Obin. | Same scenario at phone/tablet/desktop sizes, explicit stale/offline/failure states, measured single-agent baseline. |
| R1 Safe remote execution | Distinct human/Runner/attempt principals, scoped tools, enrolled always-on Runner, durable receipts, owned resources and bounded admission. | Obin authority/control; Runner admission/lifecycle. | Fix audit F1/F2/F4: no agent approval bypass, no dispatch against missing accepted dependencies, no duplicate starts; crash cleanup or explicit unresolved ownership; no owner credential on phone or in agent context. |
| R2 Complete phone development | Start work, answer durable questions, direct one writer, independently review commits/tests/preview, request revisions and integrate. | Phone, shared core, Runner artifacts, Obin work/attention/integration. | Real phone and remote Runner with laptop closed; two-device answer conflict, app loss/reconnect, stale result and partial/lost integration acknowledgement without repeated effects. |
| R3 Device depth and handoff | iPad conversation beside evidence, desktop multi-display restoration, focused file edits and scoped interactive shell, useful context/cost detail. | Device-specific compositions over common commands/models. | Real iPad multitasking, touch/keyboard/large text, phone-to-desktop handoff with local layouts preserved, editor/shell ownership and cancellation. |
| R4 Measured parallel coordination | Per-assignment working copies, durable delivery, configured role/model/effort/budgets, bounded dependency-ready dispatch. | Runner resource/scheduling owners and Obin assignments. | Compare outcome/cost against sequential baseline; conflicting edits, changed decisions, exhaustion and one crashed worker do not lose work or duplicate publication. |

R0 UX and contract work starts alongside R1 engineering. Exercise tablet/desktop
views throughout R2; R3 deepens them. Role policies and durable communication
needed for the sequential R2 flow belong in R2, not R4. A mock Hub can support
development but cannot pass the remote release gate.

**First release topology:** one person on two devices, one always-on remote
Runner, two repositories, Claude/Codex support, sequential writing and independent
review. Execution does not depend on a client window. A sleeping laptop Runner
does not silently migrate native conversations.

**Implementation order:** qualify existing Obin contracts and start independent
Runner lifecycle through Codefly; make release gates reproducible; fix authority,
dependency and resource lifetime; connect the Hub's durable agents and assignments;
deliver phone Inbox, Work and review/integration. Keep desktop behavior covered
while extracting portable state. Do not admit all 44 staged files wholesale or
turn that local board into a competing Hub.

**Scorecard:** phone-completed/integrated outcomes, manual context copying,
unexplained stalls, stale decisions accepted, duplicate starts/publications,
orphan resources, recovery, time/API-equivalent cost per accepted result,
unpriced usage and touch/keyboard/screen-space usability.

**Deferred:** plugin marketplace, autonomous model routing, unbounded agent
chatter, elaborate DAG editing and fleet-scale parallelism. Phone/iPad UX,
remote authority, artifact access and durable attention are not deferred.

The M0–M8 catalog remains historical scope inventory. M1 does not authorize a
second Hub here, and V2 805 no longer postpones mobile until after a desktop MVP.
No R0–R4 outcome is marked complete by this audit.

## November 3 delivery review (historical)

<a id="current-delivery-order--critical-review-2026-11-03"></a>

The following records the earlier local-first sequence. The November 9 direction
above supersedes its mobile deferral and delivery order.

The immediate goal is **one person reliably delivering a coordinated change
across repositories with Claude and Codex**. Agent count, number of panels and
number of generated PRs are not success measures. The future shared Hub belongs
in **Obin platform V2**, not this repository. The M0–M8 catalog below is retained
as a long-term dependency inventory; it is not the current implementation order.
Its older requirement to implement a local Hub before dogfooding is superseded.

The running implementation provides a substantial local execution workbench:
structured adapters, retained conversations, worktrees, account profiles,
command capture, telemetry, repository discovery, and workspace surfaces.
It does **not** yet provide a complete coordination or delivery system.
Multiple conversations currently share the workspace's worktrees. Attachment
tracking is not exclusive task-level write ownership.

A workflow experiment exists in an isolated source snapshot. It demonstrates
an objective, task/dependency board, explicit coordinator startup, provider tools,
review states, launch deduplication and restart handling. It is **not applied to
the running Dev app**, and is not admitted as the coordination architecture.
The later preserved candidate has SQLite receipts/change records, single-writer
admission and captured commits. It still lacks a notification consumer, test
provenance and integration receipts; the November 9 audit also reproduces authority
and dependency-freshness defects. See the
[critical architecture review](ARCHITECTURE.md#critical-coordination-review--2026-11-03).

| Order | Outcome | Smallest implementation boundary | Admission evidence |
| --- | --- | --- | --- |
| L0 Contract and baseline | Make current ownership and failure behavior explicit. | Transition table; capability evidence; distinguish workspace, task, assignment, attempt, result and integration; freeze a two-repository fixture and single-agent baseline. | Code-backed inventory; baseline elapsed time, interventions, accepted outcome and cost; no fabricated completion claims. |
| L1 Durable coordinated work | A coordinator proposes bounded work; one worker executes; an independent reviewer checks it. | Workspace coordination module, transactional state/command receipts/change log, scoped tools, versioned context packet, persisted role/model policy; one writer per mutable checkout. | Restart at each start/ack boundary; duplicate starts; changed decisions; interrupted worker; deleted chat; no lost task or silent duplicate writer. |
| L2 Verified delivery | The objective reaches the intended repository state. | Result bound to repository commit vector and captured tests; separate review, acceptance and integration states; explicit base selection in diff UI. | Change a reviewed commit and observe invalidation. Complete dependent changes in two controlled repositories; record partial integration and a lost merge response without duplicate publication. |
| L3 Bounded parallelism | Parallel work improves outcomes without corrupting shared state. | Per-assignment working copies for independent writers; dependency-ready queue, aggregate resource/cost limits, durable notifications, explicit delegation depth and cancellation ownership. | Two independent writers plus review, conflicting edits, resource contention and one crashed worker; compare against L1 baseline before raising concurrency. |
| L4 Obin Hub integration | Share canonical work and coordinate across workspaces/people. | External work references, scoped commands/events, artifact availability, identity and assignment authority supplied by Obin platform V2. | Contract tests against the real Hub; duplicate tracker links, disconnected Runner, stale assignment, device handoff and one authoritative owner per record. |

L1 and L2 form the first useful local coordination release. L3 is not a
prerequisite for it. Ordinary single-agent work stays direct; coordination is
an explicit workspace action and can also be requested through a scoped tool.
A Kanban board is a view of durable work. A graphical DAG editor is optional.

Configuration should describe roles (coordinator, implementer, reviewer),
provider/model/effort where supported, account pool, tool authority and budgets.
Record requested and observed configuration on each attempt. Model switching
is not equivalent to account rotation or portable native resume. Begin with
an explicit configuration and measured comparisons, not an automatic router.

**Deferred from the next slice:** another Hub implementation, mobile delivery,
general workflow/plugin marketplace, unconstrained agent-to-agent chatter,
automatic model routing, fleet-scale scheduling, and elaborate DAG editing.
Do not drop basic chat continuity, clear task/repository identity, source review,
error recovery or measured UX quality to make room for those features.

**Release scorecard:** accepted and integrated outcomes; number of manual context
copies and unexplained stalls; duplicate assignments/publications; stale evidence
accepted; elapsed and critical-path time; tokens/API-equivalent cost per accepted
result; unpriced usage; recovery after interruption. Subscription allowances and
invoiced spend remain separate from estimated token cost.

Research informs these proposed gates, rather than proving that any particular
agent count or framework is optimal. The [architecture review](ARCHITECTURE.md#critical-coordination-review--2026-11-03)
records primary sources, implementation gaps, and alternatives.

Work package identifiers below are local planning references. They carry no assignment or tracker status. Calendar commitments should follow the initial interaction prototype, protocol experiments, and reuse audits so estimates reflect measured work.

## Milestones and dependencies

| Milestone | Outcome | Depends on | Status |
| --- | --- | --- | --- |
| M0 Foundations and interaction prototype | Agreed product contract, tested desktop direction, failure model, and audit boundaries. | This planning package | Draft planning exists; validation open. |
| M1 Shared Hub and personal clients | Two humans can operate durable shared work from independent clients. | M0 core and identity decisions | Planned |
| M2 Runner execution and continuity | Two Runners execute work with explicit authority, recovery, and capacity. | M1 execution and identity contracts | Planned |
| M3 Agent collaboration and review | Two agent families collaborate through work, decisions, evidence, and review. | M1 + M2 + relevant audits | Planned |
| M4 Visual desktop and UI development | Independent native windows, remote preview, annotations, review, and personal restoration. | M0 desktop experiment; M1 contracts; M2 preview services; M3 evidence | Planned |
| M5 MVP integration and dogfooding | The complete reference journey and release gates pass. | M1 through M4 | Planned |
| M6 Fleet scale and operations | Larger fleets remain understandable and recoverable under sustained load. | M5 measurements | Proposed after MVP |
| M7 Rich collaborative desktop | Deeper visual work, shared presence, and more powerful human teamwork. | M5 + validated product needs | Proposed after MVP |
| M8 Ecosystem and V1 transition | Additional integrations, platforms, selective migration, and release operations. | M5 + audits for each capability | Proposed after MVP |

M4 interaction work starts in M0 and evolves alongside the services. Its integration dependencies do not postpone native window and preview validation until the backend is complete. M6 through M8 can overlap after their individual prerequisites are satisfied.

## M0 Foundations and interaction prototype

**Goal:** settle the expensive product and boundary questions through a narrow prototype and executable contract examples.

| Package | Deliverable | Acceptance evidence |
| --- | --- | --- |
| V2 001 Product contract | Review the reference journey, MVP boundary, terminology, and deferred scope. | A walkthrough covering both humans, both agent families, device handoff, review, and integration. |
| V2 002 Core records and state machines | Define stable identities, work/execution/review states, ownership, and transition rules. | Transition examples include stale edits, agent replacement, unknown execution state, and source changes. |
| V2 003 Native desktop experiment | Use React/TypeScript surfaces in Tauri; evaluate Dockview and a native window host adapter. | Detach a live console, close its original host, reconnect, and restore across physical display changes without restarting execution. |
| V2 004 Remote preview experiment | Route a development application from a Runner host into the desktop. | Assets, cookies, navigation, WebSockets, screenshot capture, and origin separation demonstrated. |
| V2 005 Protocol and recovery examples | Evaluate Protobuf/ConnectRPC commands and subscriptions, Runner gRPC control, revisions, assignment identities, and reconciliation. | Generated Go/TypeScript clients demonstrate browser server streams and Go control streaming; dropped responses, duplicate starts, replay gaps, and partitions have reproducible cases. |
| V2 006 First reuse audit batch | Review identity/work storage, agent stream adapters, git operations, and desktop lifecycle candidates. | Recorded dispositions and dependency boundaries; fixes have counterfactual regression evidence. |
| V2 007 Development foundation | Establish isolated Go Hub/Runner entry points, shared TypeScript client/surface packages, state configuration, fixtures, checks, and benchmark harness. | Reproducible launch and checks establish the selected language boundaries with V1 state untouched. |
| V2 008 Agent communication and command capture experiment | Deliver the [runnable test bench](FIRST_DELIVERABLE.md) with a generic Go engine, minimal Claude/Codex adapters, command capture, and a React browser console. | Follow-ups, images, questions, real approval replies, interruption, resume limits, and one command whose output reaches both agent and independent console viewers. |

Exit: D01 through D06, D09, and D11 in [Decisions](DECISIONS.md) have dispositions, including revising a proposal where the experiment disproves it. The runtime, native desktop, and preview experiments have evidence; the first executable core contract is ready to implement. No V1 module is admitted solely because it was inventoried.

## M1 Shared Hub and personal clients

**Goal:** establish shared authority and continuity with two humans before introducing a large execution fleet.

| Package | Deliverable | Acceptance evidence |
| --- | --- | --- |
| V2 101 Hub lifecycle and persistence | Standalone service, configuration, migrations, durable records, and separate V2 state. | Restart preserves acknowledged work; Desktop exit leaves the service running. |
| V2 102 Participant and device enrollment | Individual humans, memberships, device authentication, scoped agent/Runner identities, and revocation. | Two humans act independently; unauthorized and revoked operations are rejected. |
| V2 103 Shared work graph | Projects, objectives, criteria, assignments, dependencies, revisioned changes, and provenance. | Cycle prevention and concurrent ownership/decision conflicts are exercised. |
| V2 104 Commands and change delivery | Idempotent commands, transactional events, snapshot plus deltas, cursor recovery, and capability negotiation. | Dropped acknowledgements and expired cursors recover without lost mutations. |
| V2 105 Personal continuation | Personal attention cursor, durable surface references, and drafts separated from shared records. | A second device restores useful context without changing another user's navigation. |
| V2 106 Minimal work surfaces | Work list/map, detail, participants, decisions, and visible mutation outcomes over real Hub data. | Two clients see shared updates while maintaining independent selections. |

Exit: A04 and A10 pass in the integration tier; shared-state parts of A03, A06, and A11 pass. The native prototype uses real shared records. Automated tests cover persistence boundaries and concurrency.

## M2 Runner execution and continuity

**Goal:** make execution independent of desktop presence and safe to reconcile after uncertainty.

| Package | Deliverable | Acceptance evidence |
| --- | --- | --- |
| V2 201 Runner lifecycle and enrollment | Independent service, registration, capabilities, heartbeats, reconnect, and local journal. | Two distinct hosts enroll and report availability truthfully. |
| V2 202 Assignment and capacity | Explicit placement, queueing, atomic reservations, assignment epochs, and duplicate-start prevention. | Simultaneous dispatch respects capacity; redelivery starts one attempt. |
| V2 203 Repository execution environment | Runner Collections of named worktrees across many repositories, repository preparation, source revisions, service configuration, and discovered ports. Initial local Collections backend/RPC/CLI implemented; broader environment integration remains open. | Two reference application executions do not collide in files, services, or test data; Collection provisioning, ownership, retries, and persistence are verified independently of windows. |
| V2 204 Process and output lifecycle | Start, cancel, exit reporting, shared log pipes, managed command wait/read, replayable console output, bounded spooling, and retained failure evidence. | A13 passes; startup failures, cancellation, storage exhaustion, large output, and child-process cleanup are exercised. |
| V2 205 Partition and restart reconciliation | Persist accepted attempts and reconcile after Hub restart, Runner disconnect, or Runner reboot. | A05 and A06 execution paths pass; stale attempts cannot integrate. |
| V2 206 Artifacts and preview services | Durable artifact upload, availability metadata, authenticated preview routing, and service discovery. | Another computer can fetch saved evidence and open a live remote preview. |

Exit: each Runner completes real work; A05 and A06 pass; the failure foundation for A07 and A08 is available. The local topology follows the same contracts as the remote topology.

## M3 Agent collaboration and review

**Goal:** have agents and humans coordinate through shared work with explicit results and decisions.

| Package | Deliverable | Acceptance evidence |
| --- | --- | --- |
| V2 301 Structured adapter contract | Normalize bidirectional messages, images, queue/steer/interrupt, tools, actual protocol questions/approvals, turn completion, failures, and adapter capabilities. | A14 and the adapter conformance suite separate process, turn, request, and work lifecycle. |
| V2 302 Two supported adapters | Audited Claude Code and Codex paths, or a documented replacement if an experiment rejects a candidate. | Both adapters complete the required collaboration interactions using real binaries. |
| V2 303 Agent work operations | Bounded assignment/context queries, findings, contracts, questions, and result submission through Hub owners. | One agent consumes the other's contract and both can read the same decision. |
| V2 304 Delivery and handoffs | Durable notifications, receipts, recipient context, failure visibility, and feedback assignment. | A dropped connection cannot silently discard an acknowledged handoff. |
| V2 305 Review and acceptance | Evidence anchors, review findings, dispositions, revision freshness, and attributable acceptance. | Stale review evidence is detected; a result proposal does not accept itself. |
| V2 306 Tracker and integration path | Audited GitHub record linkage, source revision tracking, explicit merge action, dependencies, and uncertain-write reconciliation. | A09 passes against a controlled repository; no duplicate workspace or repeated uncertain side effect. |

Exit: two real agents collaborate across the reference repositories. A07 and A09 pass. Shared decisions and results are inspectable without recovering them from terminal screen captures.

## M4 Visual desktop and UI development

**Goal:** make the complete workflow practical through visual tools and independent working surfaces.

| Package | Deliverable | Acceptance evidence |
| --- | --- | --- |
| V2 401 Desktop shared model | Shared TypeScript core, React surfaces, scoped subscriptions, local derived views, visible sync state, bounded rendering, and mutation feedback. | Heavy process output and several windows preserve responsive shared work; browser and native hosts use the same application operations. |
| V2 402 Native windows and restoration | Detach/open surfaces including shared live command consoles, independent navigation, personal layout, display adaptation, and service independence. | A02 passes with native windows and physical display changes; A13 verifies viewer independence. |
| V2 403 Work and decision views | Dependency map, ownership, blockers, changes since last visit, and contextual decisions. | A returning person can identify changes and act without a status-summary prompt. |
| V2 404 Live application inspection | Authorized local/remote preview surfaces with interactive application behavior. | Two computers inspect the real application with development updates working. |
| V2 405 Visual feedback | Screenshot capture, pinned/rectangular comments, work assignment, discussion, and resolution. | A second human's annotation becomes an agent fix with accessible evidence. |
| V2 406 Review and integration surfaces | Revision-bound diffs, test evidence, findings, integration blockers, and explicit actions. | A09 completes through the Desktop with accurate pending and failed states. |
| V2 407 Keyboard and interaction quality | Isolated React surface scenarios, keyboard paths, focus, discoverable actions, loading/empty/error states, and usable window titles. | Reproducible surface states, browser workflow tests, native checks, and a recorded two-person usability pass cover required surfaces. |

Exit: A02, A03, and A08 pass. The Desktop supports the complete reference journey on actual computers. Fixture-based prototypes have been replaced on the acceptance path by real services.

## M5 MVP integration and dogfooding

**Goal:** establish a usable, measured, recoverable product rather than a collection of individual capabilities.

| Package | Deliverable | Acceptance evidence |
| --- | --- | --- |
| V2 501 Full reference journey | Two-person, two-agent, two-repository workflow with visual feedback and integration. | A01 completes twice, including one run with an intervening device handoff. |
| V2 502 Failure suite | Automated contract/integration scenarios and native manual coverage. | A01 through A14 have evidence, with failures linked to regression coverage. |
| V2 503 Scale baseline | Reproducible synthetic fleet workload, hardware/network profile, latency and memory measurements. | The [MVP benchmark](MVP.md#scale-and-responsiveness) is measured against frozen targets. |
| V2 504 Deployment and recovery | Local setup, remote setup, enrollment, version checks, logs, backup/restore, and independent service updates. | A12 plus a documented remote deployment and backup restoration drill. |
| V2 505 Audit closure and release review | Approved reuse records, unresolved-defect triage, product limitations, and release procedure. | Every [MVP release gate](MVP.md#mvp-release-gates) has an explicit outcome. |

Exit: MVP release gates pass. Record actual defects and intervention burden, then reprioritize M6 through M8 from those observations.

## M6 Fleet scale and operations

**Goal:** increase parallel work while keeping coordination, integration, and recovery understandable.

| Package | Direction | Evidence needed before expansion |
| --- | --- | --- |
| V2 601 Scheduling and resource control | Capability-aware placement, priorities, quotas, project budgets, fair queues, and Runner draining. | Capacity and cost reporting are trustworthy; admission control works under burst load. |
| V2 602 Dependency and integration scheduling | Critical path, cross-repo contracts, review gates, merge order, and invalidation after upstream changes. | Existing graph and review states remain explainable at the measured fleet size. |
| V2 603 Recovery and observability | End-to-end traces, reconciliation tooling, stuck-work diagnosis, bounded retention, and disaster recovery. | Operators can reconstruct a failed attempt and restore service without duplicate writers. |
| V2 604 Larger projects | Incremental queries, virtualization, scoped subscriptions, artifact lifecycle, and search. | Successively larger published benchmarks retain interaction and recovery budgets. |
| V2 605 Controlled automation | Policy-bound coordinator agents, automatic dispatch/review where justified, and visible intervention controls. | Automation produces auditable decisions and respects scoped authority and budgets. |
| V2 606 Additional execution environments | Container/sandbox providers, environment snapshots, stronger workload boundaries, and remote service isolation. | Provider conformance covers authority, credentials, ports, cancellation, and cleanup. |

Gate: declare the tested fleet size and event rates, supported environment, recovery guarantees, and human intervention cost. Do not infer real-agent throughput from simulated execution counts.

## M7 Rich collaborative desktop

**Goal:** support more ways for humans and agents to inspect, explain, and shape work visually.

| Package | Direction | Product validation |
| --- | --- | --- |
| V2 701 Visual composition | Arrange linked work, diagrams, documents, previews, and evidence into persistent surfaces. | A real planning or review task becomes easier to complete and resume. |
| V2 702 Team awareness | Presence, deliberate follow mode, shared presentations, and invitations to inspect specific evidence. | Collaboration preserves personal navigation and makes shared control explicit. |
| V2 703 Deeper UI development | DOM-bound comments, browser console/network context, responsive previews, recordings, and visual comparison. | Feedback can be reproduced and verified against the relevant revision. |
| V2 704 Rich review and decisions | Multi-reviewer workflows, alternatives, design discussions, artifact comparison, and approval routing. | Decisions retain provenance and affected work; conflicting conclusions remain visible. |
| V2 705 Personal continuity | Saved working sets, search across evidence, device-adaptive layouts, and useful offline reading. | Switching devices reduces reconstruction effort in observed use. |
| V2 706 Extensible visual components | A scoped component contract usable by product surfaces and agents. | Extensions cannot bypass Hub authority or turn application content into privileged commands. |

Gate: each visual capability solves an observed working situation and shares the existing records and artifact references.

## M8 Ecosystem and V1 transition

**Goal:** broaden adoption while preserving explicit compatibility and audited boundaries.

| Package | Direction | Prerequisite |
| --- | --- | --- |
| V2 801 Provider expansion | Linear, Jira, Slack, and additional source control integrations. | Audited provider and credential boundaries, with shared mutation/reconciliation contracts. |
| V2 802 Agent expansion | Additional structured adapters and capability negotiation. | Adapter conformance suite and reliable scope/identity mapping. |
| V2 803 V1 migration | Selective import of useful work, links, decisions, and history; retain source provenance. | Inventory, format/version mapping, backups, dry run, restartability, and validation of imported records. |
| V2 804 Coexistence and rollout | Independent installation/state, supported upgrades, compatibility ranges, release signing, and rollback. | Local/remote deployment and recovery gates. |
| V2 805 Additional clients | Full web delivery, Expo/React Native iPhone workflows, additional desktop platforms, and platform host adapters over the shared TypeScript core. | Shared contracts, actual-device stream recovery, preview access, and personal-context continuity proven; desktop DOM layouts are adapted for touch. |
| V2 806 Multiple Hubs | Federated discovery, cross-space work references, delegated authority, and shared evidence where needed. | A demonstrated multi-Hub need; explicit ownership and consistency model. |

Gate: each migration or integration has its own audit and acceptance evidence. V1 retirement requires an explicit decision about supported workflows and data retention.

## Requirements across milestones

- Each implementation package identifies the owning component, contract change, dependency impact, audit references, tests, and remaining limits.
- Changes that repair behavior are classified at the owning boundary under the repository's existing conventions.
- Reuse approval is scoped to source revision, responsibility, and dependency closure. Changed boundaries trigger review again.
- Read models remain bounded; the product shows stale or missing observations explicitly.
- Schema, deployment, recovery, and operator documentation change with the behavior they describe.
- Real-agent and native UI tests complement deterministic contract and failure tests.
- Repository checks and resource-awareness procedures apply to implementation work; they are not replaced by this roadmap.

## Critical risks and early experiments

| Risk | First resolution point | Consequence if unresolved |
| --- | --- | --- |
| Desktop technology cannot support usable independent windows or previews. | V2 003 and V2 004 | Change the proposed desktop approach before building the full client. |
| V1 reuse imports identity, lifecycle, or global-configuration assumptions. | V2 006 and each later audit | Repair or rewrite the boundary; keep the candidate out of production V2. |
| Hub/Runner partitions produce duplicate work or conflicting integration. | V2 005 and V2 205 | Execution authority blocks the MVP. |
| Agent adapters cannot return durable structured results and decisions. | V2 301 and V2 302 | Resolve the adapter capability before accepting the collaboration path. |
| Artifacts or previews depend on one person's machine. | V2 004 and V2 206 | Cross-device continuity blocks the MVP. |
| Too many features obscure the first working journey. | M0 scope and M5 dogfood review | Keep the mandatory journey intact and defer unrelated breadth. |
| Multi-human credential ownership is ambiguous. | V2 102 and V2 306 | Identity and provider mutation paths block the MVP. |

## Immediate implementation order

1. Review the proposed defaults and close the M0 decisions that block contracts.
2. Prove bidirectional agent communication and shared command capture with real harnesses, recording adapter capabilities and limitations.
3. Run the native multi-window and remote-preview experiments with the reference application.
4. Audit the first required V1 modules, including the log pipe, and record dispositions.
5. Implement the minimal Hub with two identities and real shared work.
6. Connect a Runner and one structured agent through durable assignment, shared command consoles, and results.
7. Add the second agent family and second human's visual-feedback path.
8. Exercise the complete journey across computers, then run failure and scale gates.

Each step produces an inspectable result. A milestone stays open until its acceptance evidence exists.

### Coordination intent (design requirement, not implemented)

“Coordinate work” must be distinct from fetching, pulling or synchronizing Git.
It should open a planning conversation using a configured high-capability model
and the user's selected account/permission policy. The planner proposes durable
workspace objectives, dependencies, acceptance criteria and ownership. The
workspace board is the primary view; a dependency graph is an optional view of
the same records. Execution state and completion evidence must remain separate
from task state, and all records must outlive an individual agent conversation.

Expose this through a small Runner-owned contract, independently testable with
headless planners and event replay. Do not build a Hub in this repository: the
cross-workspace coordination authority belongs to Obin platform V2. A tool named
for requesting coordination must not silently perform Git synchronization.
Interactive workspace PTYs and a full snippet manager remain separate missing
capabilities; favorites/catalog search do not constitute either implementation.
