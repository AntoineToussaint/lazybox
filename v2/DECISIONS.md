# Lazybox V2 decisions

This log separates direction confirmed in the design discussion from implementation proposals. A proposal becomes an implementation decision when its rationale, alternatives, and validation evidence have been reviewed and recorded. Changes remain explicit so older V1 assumptions cannot silently settle V2 behavior.

## Confirmed direction

**2026-11-09 — C19 (user direction):** make development from the phone the target,
with intentionally different phone, iPad and laptop/multi-display experiences.
Remote continuity and mobile UX enter the first release. This supersedes the
November 3 mobile deferral; C17's external Obin Hub ownership remains in force.
[Device experience](DEVICE_EXPERIENCE.md) proposes the interaction and integration
contract. The [audit](audits/2026-11-09/README.md) records current evidence; it does
not approve a Hub API, backend framework replacement or the staged workflow.

| ID | Decision | Consequence |
| --- | --- | --- |
| C01 | Start with the core problems of coordination, continuity, and usable visual work. | The first release follows the [reference journey](PRODUCT.md#the-reference-journey). |
| C02 | Support several humans and several collaborating agents from the beginning. | Individual identity, attribution, concurrent actions, and shared decisions are foundational. |
| C03 | Support starting in one place and finishing elsewhere. | Remote execution, portable evidence, reconnect, and personal context restoration enter the MVP. |
| C04 | Build a smart desktop environment with independent windows and multiple screens. | Native interaction and display adaptation are tested early. |
| C05 | Make UI development practical through visual components and running applications. | The MVP includes a real preview, annotated feedback, and revision-aware review. |
| C06 | Separate coordination and execution into Hub and Runner. | The Hub has the shared view; Runners own worktrees, processes, agents, and development services. |
| C07 | Support local and remote deployment. | The same component contracts apply in both topologies. |
| C08 | Audit every V1 implementation brought into V2. | Reuse requires [recorded admission evidence](REUSE_AUDIT.md). |
| C09 | Create a dedicated planning folder, full roadmap, and explicit MVP definition. | This folder is the V2 planning entry point. |
| C10 | Preserve shared shell output: agents can inspect command output while humans watch it in live consoles through a simple publishing workflow. | The log pipe, shared command identity, replayable consoles, and output access are MVP requirements. |
| C11 | Use Go for Hub/Runner, a shared TypeScript client core, React for desktop/web surfaces, Tauri for the desktop host, and Expo/React Native for iPhone. | [Stack](STACK.md) records the selected direction; framework and adapter integration tests remain open. |
| C12 | Treat product views as reusable surfaces with shared, personal, and per-view state ownership. | Detaching or restoring a console cannot create another execution or redirect another window's navigation. |
| C13 | Support multiple subscriptions/accounts and continue work when the current one has no capacity. | Account profiles, credential isolation, verified resume, and checkpoint handoff are foundational execution requirements. |
| C14 | Start repository work from a Runner repo library; a Collection contains one or more repositories and their worktrees, and coding sessions attach to it. Confirmed 2026-10-29. | Single-repo starts use a one-member Collection through a direct shortcut. Selecting a Collection changes the view; Git mutations remain explicit. See [Collections](COLLECTIONS.md#agreed-entry-model). |
| C15 | Use Claude Code and Codex as LLM providers for application features as well as coding agents. Confirmed 2026-10-29. | Help, planning, and summaries share the provider boundary, selected account, streaming, and cancellation. Their requests do not implicitly provision worktrees or grant execution tools. See [provider use modes](INTERFACES.md#provider-use-modes). |

| C16 | Start Settings in the main workspace with Subscriptions as its first tab. Confirmed 2026-10-29. | Add Claude/Codex profiles and show each account’s provider-reported usage. Missing data stays explicit; subscription allowance and session cost remain distinct. |

## Current scope corrections — 2026-11-03

- **C17 (user direction):** build the shared Hub in Obin platform V2, not here.
  Local workflow experiments must not quietly become a competing Hub or claim
  distributed ownership. This supersedes older repository-placement assumptions.
- **C18 (user direction):** the workspace is the primary UI unit, with multiple
  conversations and a unified repository picker. A physical checkout is an
  implementation resource, not a separate product identity or mandatory library
  registration step. This supersedes C14's user-facing Collection/library flow.
- **D14 (review proposal, not an implemented guarantee):** preserve native
  harnesses and introduce a narrow workspace coordination domain; separate task,
  assignment, attempt, evidence and integration; start with one writer per
  mutable checkout, and admit isolated parallel writers only after L1/L2.
  [Rationale and alternatives](ARCHITECTURE.md#critical-coordination-review--2026-11-03).
- **D15 (review proposal):** role/model policies and evidence-backed completion
  precede automatic routing, arbitrary workflow plugins or a general agent mesh.
  Measure a single-agent baseline and expose unsupported capabilities explicitly.

The isolated workflow board experiment is not a disposition of D03, D07 or D08.
It proves useful wiring and interactions, not fenced execution or verified
integration. No framework replacement or Obin Hub API was selected by this review.

## Proposed implementation defaults

These defaults make the plan concrete. They remain proposals until their validation gates pass.

| Topic | Proposed default | Resolution gate |
| --- | --- | --- |
| Shared authority | One authoritative Hub per collaboration space. | M0 protocol and failure walkthrough. |
| Initial layout | D01: Codefly Runner service Go module and root npm workspace under `v2/`. | Runnable local bench; production Hub/Desktop behavior open. |
| Language and framework selection | Selected in C11; Go services, React/TypeScript client, Tauri desktop, Expo iPhone. | D02 and D12 track validation; V1 code remains unaudited. |
| Docking | Evaluate Dockview for desktop/web panel layouts; implement native detachment through a platform host interface. | V2 003 |
| RPC | Runner: Codefly `go-grpc`, one gRPC implementation and a generated Connect listener. Hub/Runner control remains proposed. | D04; local browser consumption verified. |
| First platforms | macOS Desktop; macOS and Linux Hub/Runner; Linux remote reference deployment. | Native experiment and deployment baseline. |
| First agents | Claude Code and Codex, each through a verified structured adapter. | Adapter conformance experiment. |
| Agent runtime | Structured bidirectional headless sessions with actual protocol requests and responses; terminals for command consoles and interactive shell programs. | D11 and V2 008 |
| Tracker | Initial GitHub linkage and explicit integration, respecting tracker/workspace continuity. | Provider audit and controlled integration test. |
| Shared persistence | Transactional Hub store with durable mutation/event records. | D04 and storage audit. |
| Artifacts | Durable metadata plus Hub-accessible blob storage for saved evidence. | D06 |
| Placement | Explicit Runner selection and capacity reservations first. | M2 |
| Initial roles | Maintainer and contributor, plus bounded agent/Runner credentials. | D05 |
| Disconnected desktop | Cached reads and unsent drafts; shared writes resume after reconnection. | M1/M4 |
| Execution continuity | Reconcile known attempts; no automatic process migration. | M2 partition tests. |

## Decisions required before implementation

### D01 Repository and implementation boundary

**Status:** source layout selected and scaffolded. **Needed for:** V2 007.

Use an independent Runner Go module at `v2/services/runner/code/go.mod` and an npm workspace at `v2/package.json`. On 2026-10-27 the Runner adopted the user-requested Codefly `go-grpc` service layout; its generated composition and listener lifecycle replace the initial root-module scaffold. [Source structure](STRUCTURE.md) defines executable composition roots, Hub/Runner internals, provider-neutral interfaces, account profiles, checkpoints, service schemas, generated contracts, shared clients, platform applications, and tests. The user authorized scaffolding this structure after accepting the stack direction.

The initial scaffold imported no V1 implementation. The current Runner uses Codefly and the supported provider integration dependencies; React and the Claude SDK are pinned in the npm lockfile. The native Tauri host has its own Cargo manifest, lockfile, and checks and shares AgentWindow through a platform-supplied fetch transport. Dependency isolation and framework admission must continue to be checked as implementation lands.

Decide naming for configuration and storage independently of V1's existing versioned paths. Capture coexistence, versioning, and a future repository split if needed.

### D02 Desktop technology and window ownership

**Status:** stack and state ownership selected on 2026-10-25; native integration validation open. **Needed for:** V2 003 and V2 004.

Use React/TypeScript surfaces in a Tauri host, with a small Rust native layer. The shared TypeScript client core also serves web and the Expo/React Native iPhone app. [Stack](STACK.md) defines shared Hub records, personal working context, and independent surface/device state. This supersedes the earlier unspecialized Tauri/TypeScript evaluation and the conversational Svelte suggestion.

Evaluate Dockview for layout within windows. Native Tauri detachment, focus, multiple physical displays, browser previews, screenshot annotation, and restoration require their own proof. The first native test detaches a live console, closes its original host, and restores its view after reconnect and display changes while execution continues.

Decision basis: the user accepted this direction in the design discussion. The alternative native Rust UI approach was discussed; browser and mobile sharing favor the selected shared application layer. Reopen if required native behavior, preview access, or measured interaction budgets cannot be met. No native experiment has yet validated this selection.

### D03 Work and execution state machines

**Status:** open. **Needed for:** V2 002 and M1.

Settle work readiness, dependency satisfaction, ownership, result proposal, review, acceptance, integration, failure, and reopening. Define execution observation separately, including unknown state and replacement attempts. Specify how actor identity and source revisions bind each transition.

Use explicit transition tables and example races before encoding the model.

### D04 Protocol and durable change delivery

**Status:** open. **Needed for:** V2 005 and M1.

The Runner now uses Codefly `go-grpc:0.1.69`: Protobuf with generated Go/TypeScript clients, one native gRPC service implementation, and Codefly's Connect listener transcoding to that same implementation. The user requested reusing this service framework on 2026-10-27. The local React browser path has runtime evidence. Use unary commands and server streams for browser clients; evaluate a Runner-initiated gRPC bidirectional control stream for Hub/Runner traffic. [Interfaces](INTERFACES.md) records the proposal. Browser and iPhone streaming behavior must be verified rather than assuming native gRPC is portable to them.

Settle schema generation, capability negotiation, event ordering, snapshot boundaries, replay retention, and cursor recovery. Define idempotency and conflict semantics independently of transport.

Specify separate flow control for shared records, process output, preview traffic, and artifact transfer. Establish bounded buffers and the sequence of durable writes and acknowledgements.

### D05 Identity and operation authority

**Status:** open. **Needed for:** V2 102 and Runner enrollment.

Choose local and remote enrollment, human login, device binding, Runner trust, membership, revocation, agent scopes, and maintainer/contributor operations. Decide which principal owns source-control actions and execution credentials.

The same individual must be recognizable from another device. A shared server token cannot substitute for distinct human authorship.

### D06 Preview routing and durable artifacts

**Status:** open. **Needed for:** V2 004 and V2 206.

Choose endpoint discovery, remote reachability, authentication, preview origin boundaries, WebSocket support, capture, artifact transfer, and retention. Decide which saved evidence must be uploaded before it is advertised as available.

Prove the proposed route with a real application from another computer. V1's relay and encrypted transport remain candidates to audit if selected.

### D07 Runner disconnection and fencing

**Status:** open. **Needed for:** V2 202 and V2 205.

Define assignment epochs, deduplication, heartbeat interpretation, disconnected execution policy, capacity reservations, and reconciliation. Specify how an old attempt is prevented from integrating after a reassignment.

Define when an operator can declare an execution lost and what evidence is required before dispatching another writer. Timeout alone cannot prove that an old process stopped editing.

### D08 Review and integration authority

**Status:** open. **Needed for:** V2 305 and V2 306.

Define which evidence and source revision an acceptance covers, how findings are resolved, what changes invalidate review, and who can request integration. Specify ambiguous provider outcomes and recovery after a response is lost.

### D09 MVP workload and performance budgets

**Status:** open. **Needed for:** V2 007 and V2 503.

Calibrate the [proposed benchmark and targets](MVP.md#scale-and-responsiveness), record hardware and network conditions, and freeze the initial budgets. Distinguish control-plane simulation from real-agent and native interaction evidence.

### D10 Migration and retirement

**Status:** deferred until after the first working journey. **Needed for:** V2 803 and V2 804.

Choose which V1 records and workflows to migrate, how provenance is preserved, and how imports can be validated, resumed, and reversed. Define coexistence and support boundaries before retiring a V1 path.

### D11 Agent runtime and shared command output

**Status:** proposed direction; integration details open. **Needed for:** V2 008, before the main Desktop prototype.

Evaluate structured bidirectional sessions for repeated messages, images, queueing, supported steering, interruption, questions, approval responses, and resume. Candidate integrations are Codex app-server and Claude Agent SDK streaming input. Pin tested versions and record advertised versus demonstrated capabilities.

Prove the [shared command contract](EXECUTION.md), including pipe publication that retains output access for the agent, live human consoles, managed command lifecycle, background services, and reconnect. Decide how each harness's shell tools publish or delegate execution without bypassing its operation authority.

The reviewed V1 approval handler in `crates/server/src/agent_runs.rs` currently turns a decision into ordinary message text. Audit the actual protocol response path; message delivery alone cannot establish approval resolution. V1's log pipe currently drains stdin to a tailed file, so preservation of agent-facing output must be verified explicitly when adapting it.

### D12 Service language and shared application core

**Status:** selected on 2026-10-25; implementation validation open. **Needed for:** V2 007 and later service packages.

Implement Hub and Runner in Go with the standard HTTP foundation. Share TypeScript protocol clients, record models, reducers, commands, and suitable interaction logic across desktop, web, and iPhone. Keep native APIs behind platform host interfaces. A supported provider SDK may use a small supervised worker in its required language; the generic Runner remains Go.

Decision owner: the user, through acceptance of the stack recommendation. Rationale: service maintainability and the user's Go preference, while visual surfaces share a productive web ecosystem. Rust service continuity was considered; there is no measured service requirement establishing its necessity. Actual performance, dependency boundaries, and provider integration remain unverified. A Go port of V1 behavior still requires a scoped reuse audit.

Affected packages include V2 007, V2 008, M1 through M4, and V2 805. Reopen if concrete integration or measured workload requirements invalidate the selected boundary. Database technology remains a separate persistence decision.

### D13 Subscription selection and session continuation

**Status:** local mechanism implemented 2026-10-29; live multi-account validation and distributed policy remain open. **Needed for:** V2 008 and Runner execution.

Account/subscription profiles are explicit execution context from the first agent contract. Pin attempts to a profile; detect and classify capacity/authentication failures; choose an eligible replacement; preserve shared work identity. Prefer verified native resume and fall back to an attributable checkpoint handoff where necessary.

On 2026-10-29 the user explicitly required authenticating each subscription once
and rotating without repeated login. The local implementation selects only
from an explicitly enabled same-provider pool and retains credentials in
isolated contexts. It closes and confirms the old native process before a
portable checkpoint handoff inside the same Runner execution. No global login
is changed and native cross-account resume is refused. Native permissions are
inherited for coding sessions; explicit restrictions remain pinned. Account
renewal/removal UI and validation with two real paid subscriptions remain open.

[Subscriptions](SUBSCRIPTIONS.md) defines required credential isolation, predecessor fencing, state capture, sibling-session behavior, and real-provider evidence. A logout/login may preserve resumability in some environments but is not assumed to be safe for concurrent sessions or portable across profiles. Test the actual supported integration before advertising a capability.

## Recording a resolved decision

For each resolved entry, add the date, decision owner, chosen behavior, alternatives considered, rationale, experiment or audit evidence, affected roadmap packages, and conditions that would reopen it. Update the relevant product, architecture, or MVP document in the same change so the log and working plan remain aligned.
