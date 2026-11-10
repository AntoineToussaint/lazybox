# Lazybox V2

Lazybox V2 is a development environment across phone, iPad, laptop and multiple
displays. The target is completing real work from the phone while execution
continues on an available remote Runner. The **Hub**, owned by Obin platform V2,
coordinates durable work; **Runners** execute it; **clients** provide different
experiences over the same state.

Start with the [November 9 audit](audits/2026-11-09/README.md),
[device experience contract](DEVICE_EXPERIENCE.md) and current [roadmap](ROADMAP.md).
They distinguish implemented source, the unapplied coordination candidate,
verified behavior and required remote/mobile work. The iPhone package is still
a scaffold; remote phone development is not implemented yet.

This folder contains the V2 plan and a runnable local Runner built as a Codefly `go-grpc` service, with Claude/Codex adapters, configurable metering gateways, generic file uploads, a Go test CLI, and a reusable React `AgentWindow` hosted by Tauri. Run `make desktop` after starting the Runner. A browser host supports surface tests. The external Hub contract, remote integration and mobile experience are current delivery work. See [Running](RUNNING.md) and [runtime evidence](RUNTIME_EVIDENCE.md).

The Runner now also owns **[Workspaces](WORKSPACES.md)**: a persistent root containing named worktrees from several repositories, independent of agent sessions. Creation, inspection, attachment, archive, and explicit ownership reconciliation are available through generated RPC and the CLI. The native workspace prototype supports workspace selection, configurable pages, detached chat windows, and independent command viewers. New workspace, Add repositories, and Settings → Repositories share one persistent catalog and picker. Search discovered GitHub or local repositories and create directly; the Runner prepares only selected sources, with progress and idempotent retry. GitHub access and local folder discovery are configured in Connections & folders.

The Runner also exposes application assistant conversations, provider-derived main/sub-agent status, persistent account availability, and explicitly enabled subscription rotation. Account enrollment is once per isolated profile; ordinary rotation does not run login again. The main **Settings → Subscriptions** tab adds named accounts and refreshes provider-reported usage/reset windows. **New session** starts in a retained empty Runner workspace with your selected account. See [Subscriptions](SUBSCRIPTIONS.md) for implemented boundaries and live-account validation limits.

The first [GitHub integration](GITHUB.md) supports a private personal-token connection with no App installation or GitHub CLI, selected repository imports, and read-only issue/PR lists through the Go GitHub client. GitHub App device sign-in remains optional. [Working and Dev installations](ENVIRONMENTS.md) keep native identities, Runner state and source snapshots separate.

## Read the plan

The current local dogfooding slice includes conversation rename/search, retained
history and native resume, persistent native pages/drafts across Runner port
changes, workspace-wide local Git review, and metered usage. Start it with
`make -C v2 dev` from the repository root. `make -C v2 check-dogfood` runs the
deterministic gate; [Running](RUNNING.md#local-dogfooding-loop) describes the
working loop and opt-in real-provider continuity check. The distributed Hub and
full MVP remain separate roadmap milestones.

| Document | Purpose |
| --- | --- |
| [Current audit](audits/2026-11-09/README.md) | Code-backed findings, fresh checks, reproduced defects and delivery priorities. |
| [Implementation packages](IMPLEMENTATION_ROADMAP.md) | Full phone-to-desktop delivery plan, Hub agent ownership, Codefly/Obin reuse and current implementation status. |
| [Release validation](RELEASE_GATES.md) | One catalog for staging/local/CI entry points, per-gate receipts and promotion rules. |
| [Device experience](DEVICE_EXPERIENCE.md) | Phone/iPad/desktop flows, portable state and proposed Hub boundary. |
| [Product](PRODUCT.md) | The problem, working situations, participants, and product principles. |
| [Architecture](ARCHITECTURE.md) | Component ownership, core objects, collaboration contracts, deployment, and recovery. |
| [Implementation stack](STACK.md) | Selected Go services, shared React/TypeScript client, Tauri desktop, Expo iPhone, and surface ownership. |
| [Source structure](STRUCTURE.md) | Actual folders, package boundaries, workspace setup, and scaffold checks. |
| [Interfaces and RPC](INTERFACES.md) | Proposed generic Go agent contract and Protobuf/ConnectRPC communication. |
| [Subscriptions and continuation](SUBSCRIPTIONS.md) | Account profiles, isolated credentials, native resume, and checkpoint handoff. |
| [Session hosts](SESSION_HOSTS.md) | Agents and their commands outlive the Runner: re-attachment across restarts, delivery contract, upgrades, evidence. |
| [Workspaces](WORKSPACES.md) | Runner-owned groups of worktrees across repositories, provisioning, attachment, and ownership. |
| [Running the bench](RUNNING.md) | Launch, runner options, gateway routing, credentials, continuation, CLI, and retention. |
| [Runtime evidence](RUNTIME_EVIDENCE.md) | Actual test results, pinned versions, and unverified acceptance paths. |
| [First deliverable](FIRST_DELIVERABLE.md) | Runnable agent-and-console test bench and its completion evidence. |
| [Agent sessions and consoles](EXECUTION.md) | Bidirectional agent control and shared shell output for agents and human consoles. |
| [MVP](MVP.md) | The first complete product slice, explicit scope, acceptance scenarios, and release gates. |
| [Roadmap](ROADMAP.md) | Dependencies, work packages, milestones, and expansion beyond the MVP. |
| [Reuse audit](REUSE_AUDIT.md) | The audit required for every V1 component reused in V2, with an initial candidate inventory. |
| [Decisions](DECISIONS.md) | Confirmed direction, proposed implementation defaults, and unresolved choices. |

Start with Product, then MVP. Use Architecture and the reuse audit to assess implementation choices. Roadmap defines the order of delivery. Decisions distinguishes agreed direction from proposals that still need validation.

## Confirmed direction

- Design around complex shared work and the coordination burden experienced in V1.
- Make collaboration between several humans and several agents foundational.
- Support beginning work in one place and continuing elsewhere.
- Build a smart desktop client with independent windows, multiple screens, and substantial visual tools.
- Make UI development practical through live application previews, screenshots, and contextual feedback.
- Separate the shared coordination server, **Hub**, from execution servers, **Runners**.
- Allow local and remote deployments, including several clients and several Runners.
- Review and audit every piece of V1 code brought into V2.
- Preserve the log-pipe workflow: agents can read command output while humans follow the same execution in live consoles.
- Use Go for Hub and Runner, React/TypeScript for shared desktop/web surfaces, Tauri for the desktop host, and Expo/React Native for iPhone.
- Share the client core and model personal surfaces explicitly so independent windows do not share navigation accidentally.
- Support several account/subscription profiles and continue work through verified resume or an explicit checkpoint handoff when capacity runs out.

## Scope and authority

These documents describe the V2 proposal. Existing repository documentation continues to describe V1 unless it explicitly says otherwise. The V1 [positioning](../docs/positioning.md) and [desktop boundary](../docs/desktop-spike.md) are historical inputs to this redesign; their product assumptions do not settle V2 decisions.

Repository [AGENTS.md](../AGENTS.md), applicable area instructions, and the resolved agent policies continue to govern implementation and tracker operations. Local roadmap identifiers identify planning packages; they are not filed tracker records or implementation workspaces.

The source scaffold stays in this repository with separate V2 Go/npm workspaces and entry-point locations, as recorded in [D01](DECISIONS.md#d01-repository-and-implementation-boundary). Existing uncommitted V1 work is outside this change.

## Status

| Area | Status |
| --- | --- |
| Product direction | Captured from the design discussion. |
| Stack direction | Selected; native, provider, and transport integrations still need experimental evidence. |
| MVP and delivery sequence | Proposed for review. |
| Implementation | Local Runner, adapters, commands, generated RPC, CLI, reusable AgentWindow, and native Tauri host implemented. Hub and full desktop lifecycle remain open. |
| V1 reuse | Narrow gateway behavior port reviewed; no V1 crate/runtime imported. See the audit record. |
| Validation | Codex live scenarios and a real Claude reply through AgentWindow passed. Claude protocol cases also use a deterministic gateway. Full gate remains open for the broader live Claude suite and real multi-profile paths. |

Product V2 is a product name. V1 already uses versioned storage and protocol names containing `v2`; those names do not establish V2 compatibility. V2 storage locations and configuration must be resolved explicitly and remain isolated during development.

The [agent-and-console test bench](FIRST_DELIVERABLE.md) is runnable. Its remaining acceptance evidence is listed separately from implemented behavior. The native multi-window proof follows that gate. The Runner RPC now uses Codefly's generated gRPC implementation and Connect listener; distributed Hub/Runner contracts remain proposed.

Run `make -C v2 check` to validate the implementation. See [Source structure](STRUCTURE.md#current-commands) for command scope and current limitations.

The workspace sidebar collapses a single conversation into its workspace entry.
Vim/tmux-style bindings are configurable in Settings → Shortcuts; `]]` leaves the
composer and `]]s` opens snippets by default. The snippet picker reuses V1's
canonical catalog through the read-only `lazybox-snippets` adapter. See
[workspace navigation and snippets](WORKSPACES.md#compact-navigation-and-keyboard-controls).
