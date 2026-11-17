# Lazybox V2 — internal design, plan and status

This is the contributors' index: the design notes, audits, plans and evidence
behind Lazybox V2. **Using Lazybox?** Start at the [user docs](../README.md)
instead. These pages describe how things are built, what was decided and what is
still open; they are written for people changing the code, and they use the
internal vocabulary (Runner RPCs, session hosts, the Hub, `Collection`).

The internal pages live next to the code at the repository root, where code
comments and tests refer to them. This page only indexes them.

## Start here

- [Audit and roadmap, 2026-11-16](../../audits/2026-11-16/README.md) — where V2
  stands against the product, how it compares with other tools, what is next.
- [The GUI next to herdr's](../../audits/2026-11-16/herdr-gui.md) — interface
  comparison and prioritized UI work, including *Decision: no terminal
  emulator*.
- [Product](../../PRODUCT.md), then [MVP](../../MVP.md), then the
  [roadmap](../../ROADMAP.md).

## Where V2 comes from

Lazybox V2 is a development environment across phone, iPad, laptop and several
displays. The target is completing real work from any device while execution
continues on an available Runner. The **Hub** (owned by a separate platform)
coordinates durable work; **Runners** execute it; **clients** provide different
experiences over the same state.

What exists today is the local half: a Runner built as a Codefly `go-grpc`
service with Claude and Codex adapters, configurable metering gateways, file
uploads, the `lazybox` CLI, and one React client hosted by Tauri on the desktop
and by a browser for tests and phones. The Hub contract, remote Runners and the
native iPhone app are delivery work. Check behaviour against the code and
[runtime evidence](../../RUNTIME_EVIDENCE.md), not against plans.

### Confirmed direction

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
- V2 is not a terminal emulator ([decision](../../audits/2026-11-16/herdr-gui.md#decision-no-terminal-emulator)):
  the workspace shell stays a minimal escape hatch; people use their own
  terminal and pipe output in with `lazybox log` / `lazybox run`.

### Scope and authority

These documents describe V2. V1 and its documentation live in
[lazybox-v1](https://github.com/AntoineToussaint/lazybox-v1). The V1
[positioning](https://github.com/AntoineToussaint/lazybox-v1/blob/28a9a73d1a44e7797d1285597257123a1db0a13f/docs/positioning.md) and
[desktop boundary](https://github.com/AntoineToussaint/lazybox-v1/blob/28a9a73d1a44e7797d1285597257123a1db0a13f/docs/desktop-spike.md) are historical inputs to
this redesign; their product assumptions do not settle V2 decisions.

Repository [AGENTS.md](../../AGENTS.md), area instructions and the resolved
agent policies govern implementation and tracker operations. Local roadmap
identifiers name planning packages; they are not filed tracker records.

V2 has its own repository, with the Go module and npm workspace
recorded in [D01](../../DECISIONS.md#d01-repository-and-implementation-boundary).
"V2" is a product name: V1 already uses storage and protocol names containing
`v2`, which do not establish V2 compatibility. V2 storage locations and
configuration are resolved explicitly and stay isolated during development.

### Status

| Area | Status |
| --- | --- |
| Product direction | Captured from the design discussion. |
| Stack direction | Selected; native, provider and transport integrations still need experimental evidence. |
| MVP and delivery sequence | Proposed for review. |
| Implementation | Local Runner, adapters, commands, generated RPC, CLI, the shared client and the native Tauri host implemented. Hub and remote Runners remain open. |
| V1 reuse | Narrow, audited ports (gateway behaviour, review freshness, snippet catalog through a read-only adapter); no V1 runtime imported. See the [reuse audit](../../REUSE_AUDIT.md). |
| Validation | `make check` and the [release gates](../../RELEASE_GATES.md); live-provider paths are listed separately in [runtime evidence](../../RUNTIME_EVIDENCE.md). |

## Everything, by topic

### Product and plan

| Document | Purpose |
| --- | --- |
| [Product](../../PRODUCT.md) | The problem, working situations, participants, and product principles. |
| [MVP](../../MVP.md) | The first complete product slice, explicit scope, acceptance scenarios, and release gates. |
| [Roadmap](../../ROADMAP.md) | Dependencies, work packages, milestones, and expansion beyond the MVP. |
| [Implementation packages](../../IMPLEMENTATION_ROADMAP.md) | Full phone-to-desktop delivery plan, Hub agent ownership, Codefly/Hub platform reuse and implementation status. |
| [Decisions](../../DECISIONS.md) | Confirmed direction, proposed implementation defaults, and unresolved choices. |
| [Device experience](../../DEVICE_EXPERIENCE.md) | Phone/iPad/desktop flows, portable state and proposed Hub boundary. |
| [Software factory](../../SOFTWARE_FACTORY.md) | The end state for orchestrated work: contract, holdouts, verifier, merge queue. |
| [First deliverable](../../FIRST_DELIVERABLE.md) | The runnable agent-and-console test bench and its completion evidence. |

### Architecture

| Document | Purpose |
| --- | --- |
| [Architecture](../../ARCHITECTURE.md) | Component ownership, core objects, collaboration contracts, deployment, and recovery. |
| [Implementation stack](../../STACK.md) | Go services, the shared React/TypeScript client, Tauri desktop, Expo iPhone, and surface ownership. |
| [Source structure](../../STRUCTURE.md) | Folders, package boundaries, workspace setup, and commands. |
| [Interfaces and RPC](../../INTERFACES.md) | The generic Go agent contract and Protobuf/ConnectRPC communication; telemetry and metering. |
| [Agent sessions and consoles](../../EXECUTION.md) | Bidirectional agent control and shared shell output for agents and human consoles. |
| [Session hosts](../../SESSION_HOSTS.md) | Agents and their commands outlive the Runner: re-attachment, delivery contract, upgrades, the Processes panel. |
| [Resource ownership](../../RESOURCE_OWNERSHIP.md) | Who owns which process, file and credential; agent credentials. |
| [Collections (storage)](../../COLLECTIONS.md) | The on-disk storage and ownership contract behind workspaces. |
| [Several Runners](../../MULTI_RUNNER.md) | Dev, Working and remote Runners in one window: design, and the first step shipped in the CLI. |

### Features, as built

| Document | Purpose |
| --- | --- |
| [Workspaces](../../WORKSPACES.md) | Workspaces of worktrees across repositories, panels and docking, the Web panel, review, checkpoints, navigation, snippets. |
| [Appearance](../../APPEARANCE.md) | Themes, user theme files, accent, fonts, density, zoom, minimum contrast; design tokens for developers. |
| [Voice](../../VOICE.md) | Dictation (macOS Speech on-device, Web Speech), spoken commands, voice in the palette, read-aloud; the engine choice, the Whisper design, the manual macOS checklist. |
| [Workspace runtime](../../WORKSPACE_RUNTIME.md) | `.lazybox/workspace.yaml`: setup, actions and archive scripts, approval, port blocks, Run and Preview. |
| [GitHub](../../GITHUB.md) | The optional GitHub connection, the PR loop, review comments. |
| [Subscriptions](../../SUBSCRIPTIONS.md) | Account profiles, isolated credentials, rotation, native resume and checkpoint handoff. |
| [Orchestration](../../ORCHESTRATION.md) | Missions, events and the configurable process (`.lazybox/process.yaml`). |
| [Artifacts](../../ARTIFACTS.md) | Plans and reports as versioned pages. |
| [Command output](../../COMMAND_OUTPUT.md) | The full console for people and the compact view for agents. |
| [Terminals](../../TERMINALS.md) | The workspace shell on the Runner, `lazybox log` / `lazybox run`. |
| [Remote control](../../REMOTE.md) | Pairing a phone by QR code over Tailscale, device grants, threat model. |
| [Mobile, cloud and Hub](../../MOBILE_CLOUD_HUB.md) | Proposal: native iPhone app, an end-to-end-encrypted relay and push gateway, the Hub's scope and phases, decisions for the user. |
| [Runners on other computers](../../REMOTE_RUNNERS.md) | Proposal: the full experience on a VM, cloud box, second Mac or devcontainer; owner and device access, per-client keys, one `RunnerLink` over SSH, tailnet, relay or the Hub; the Linux Runner; phases and decisions. |
| [Agent browser](../../BROWSER.md) | The browser agents drive and people watch: tools, engine choice, site permissions, Take over, safety. |
| [ACP agents](../../ACP.md) | Any Agent Client Protocol agent as a conversation: the adapter, the protocol mapping, files and terminals served by Lazybox, restarts, gaps. |
| [Computer control](../../COMPUTER_CONTROL.md) | Design only: agents driving other Mac apps, its permissions, risks and smallest safe first step. |
| [Power users](../../POWER.md) | CLI parity, `actions.yaml` actions and hooks, hook credentials, `lazybox open` / `shot`. |
| [Extensions](../../EXTENSIONS.md) | Sandboxed panel extensions: manifest and grants, the approval sheet, the content origin and its policies, the bridge and the extension credential, the threat model. |
| [The `lazybox` CLI](../../services/runner/code/cmd/lazybox/README.md) | Every verb, its flags, exit codes and credentials. |

### Running, releasing, evidence

| Document | Purpose |
| --- | --- |
| [Running the bench](../../RUNNING.md) | Launch, Runner options, gateway routing, credentials, continuation, CLI, retention, the dogfooding loop. |
| [Working and Dev installations](../../ENVIRONMENTS.md) | Separate Dev and Working apps and Runners; stage, promote, restart; isolation. |
| [Release validation](../../RELEASE_GATES.md) | One catalog of gates for staging, local and CI runs, with receipts and promotion rules. |
| [Runtime evidence](../../RUNTIME_EVIDENCE.md) | Actual test results, pinned versions, and unverified acceptance paths. |
| [Reuse audit](../../REUSE_AUDIT.md) | The audit required for every V1 component reused in V2. |

### Audits

| Date | Documents |
| --- | --- |
| 2026-11-16 | [Audit and roadmap](../../audits/2026-11-16/README.md) · [GUI next to herdr](../../audits/2026-11-16/herdr-gui.md) |
| 2026-11-09 | [Audit](../../audits/2026-11-09/README.md) · [release gates](../../audits/2026-11-09/RELEASE_GATES.md) · [resource ownership](../../audits/2026-11-09/RESOURCE_OWNERSHIP.md) · [service lifecycle](../../audits/2026-11-09/SERVICE_LIFECYCLE.md) · [sidebar UX](../../audits/2026-11-09/SIDEBAR_UX.md) |

## Develop

```sh
make deps              # npm dependencies (once, and after lockfile changes)
make dev               # Lazybox Dev: its own Runner, hot reload of UI, Go and Rust
make check             # format, vet, typecheck, unit tests, generated code
make check-release         # the release gate catalog
```

`make help` lists every target; [Source structure](../../STRUCTURE.md#commands)
explains their scope. The user docs are checked by `npm run test:client`:
every relative link and anchor must resolve, and the
[keyboard shortcut page](../reference/keyboard-shortcuts.md) must match the
action registry (`npm run docs:shortcuts` regenerates it).

**Screenshots** are taken, never drawn: `npm run docs:screenshots` drives the
real web client against the fake Runner, on a fixed clock with the same
story every time, and writes every image in `docs/images/` in the dark and the
light theme (`hero.webp`, `hero-light.webp`; some also `-narrow`). What each
image shows is one entry in `scripts/docs-shots.mjs`; `npm run
docs:screenshots hero inbox` retakes some, `-- --variants=light,narrow` some
variants, `-- --out=<dir>` writes elsewhere (an audit's folder). Pages reference
the dark image; Help → Documentation shows its light twin in the light theme,
and the README uses `<picture>` for the same. `npm run test:client` checks that
every image a page references exists and is one the script takes, that every
file in `docs/images/` is, and that every shot is used. A new screen in a
guide is a new shot: add it, run the script, look at both themes.
