# Desktop product spike

## Result

A focused Tauri client can reuse the existing daemon without moving provider,
store, worktree, PTY, or agent logic into the GUI. The spike in
`apps/desktop/` embeds the production `ServerConfig`, starts polling and
session recovery, binds `api_gateway` on an authenticated ephemeral loopback
port, and renders the persisted inbox plus one live agent terminal.

The terminal path is end to end, not a mock:

```text
xterm.js input/resize
  → typed Tauri command
  → POST /v1/stream (duplex NDJSON)
  → existing Command::{Write,Resize}
  → daemon-owned PTY
  → Event::{TerminalOutput,TerminalResync}
  → the same /v1/stream response
  → ordered Tauri channel
  → xterm.js
```

The frontend also consumes `Snapshot`, `WorkspaceUpserted`, and
`WorkspaceRemoved`, so the inbox follows the same authoritative state as the
TUI. Terminal sequence gaps trigger `RequestTerminalResync`; reconnecting the
NDJSON stream produces a fresh snapshot and daemon replay.

## Reuse boundary

The desktop Rust shell directly depends on `server`, `ipc`, and `config`. Those
crates already pull in the source-agnostic core, store, auth, providers,
git-ops, agent registry, polling, structured runs, and PTY backends. New
product code is limited to:

- process startup and an authenticated loopback gateway;
- a small Tauri-to-HTTP adapter;
- the web inbox/activity renderer;
- xterm.js state and replay handling.

`tui`, `tui-term`, and `libghostty-vt` are not desktop dependencies. The
current IPC contract has 64 `Command` variants and 59 `Event` variants; JSON
serialization makes the transport mechanically complete even though a
production GUI still needs behavior for every workflow it chooses to expose.

## Gateway gaps

| Area | What works now | Gap before a paid release |
|---|---|---|
| Command/event coverage | `JsonClientFrame` and `JsonServerFrame` serialize the full IPC enums. One-shot commands and a duplex NDJSON endpoint exist. | Publish generated TypeScript types/schema and compatibility fixtures. Add command correlation/typed failure responses; a streamed mutation currently relies on later events for outcome. |
| Inbox baseline | `/v1/workspaces` and `Subscribe` snapshots expose authoritative workspaces, projects, and terminals. | Add explicit API/protocol version discovery. The standalone `lazybox server api` boot path should start the same polling/update services as embedded clients. |
| Terminal transport | PTY input, resize, output, replay, sequence numbers, and resync all cross the duplex NDJSON connection today. This spike exercises them. | `Vec<u8>` becomes a JSON number array. Use a binary WebSocket/channel for sustained terminal throughput, with measured backpressure limits. |
| Reconnect | Terminal snapshots and rings recover interactive PTYs; the desktop client reconnects the event stream. | Structured agent runs are not persisted or replayed, so a reconnect cannot rediscover an active structured run and its accumulated turns. |
| Security | Loopback by default, constant-time bearer comparison, connection caps, bounded bodies/lines, no CORS. The spike keeps a random token in Rust. | Add token bootstrap/rotation and OS-keychain storage. Remote access needs TLS or an explicit trusted tunnel plus principal-scoped authorization; bearer auth alone is not transport encryption. |
| Browser transport | Tauri can proxy the local API without CORS. | If a browser client becomes a product, define a narrow origin policy and preflight behavior. Do not add wildcard CORS to the agent-control API. |
| Diagnostics | Event-pipeline metrics and bounded command execution exist. | Add connection/session identifiers, structured gateway errors, client-visible stream health, and tracing that correlates a UI action with its daemon work. |
| Product semantics | The TUI demonstrates every policy, confirmation, and modal flow. | Port intent resolution and safety confirmations for each GUI feature. Transport availability does not make destructive workflows safe by default. |

## Delivery plan and estimate

Estimates assume one senior product engineer familiar with Rust and TypeScript,
one supported desktop OS first (macOS), and no TUI-parity requirement for the
initial sale. They include implementation, tests, and release hardening but not
calendar delay for Apple account approval.

### Phase 0 — completed spike (about 1 engineer-week)

- embedded production daemon and authenticated local gateway;
- live inbox/activity renderer;
- start an agent and interact with one xterm.js terminal;
- replay, resize, reconnect, and resync proof;
- unit/build checks and a desktop-specific CI lane.

### Phase 1 — private MVP (4–6 engineer-weeks)

- generated TypeScript protocol package and stable client-side state model;
- production duplex/binary terminal transport and load/backpressure tests;
- inbox filters, workspace creation/opening, activity detail, replies, and the
  small set of confirmations needed by those flows;
- settings/setup for provider credentials and agent selection;
- crash reporting and opt-in product analytics;
- macOS signing, notarization, packaging, auto-update, and private beta.

Exit criterion: a single user can install the app, connect GitHub, triage the
inbox, start/resume work, use an agent or shell, and safely update the app.

### Phase 2 — sellable v1 (another 4–6 engineer-weeks)

- license entitlement with offline grace and customer/account recovery;
- reliable structured-run reconnect or an explicit terminal-only v1;
- snippets, reviewer/assignee/label mutations, CI/review actions, notifications,
  and polished empty/error/loading states;
- accessibility, keyboard navigation, telemetry consent, support diagnostics,
  update rollback, and release automation;
- Windows packaging if market demand justifies it; Linux remains best-effort
  until its WebKit/package matrix has dedicated QA.

Exit criterion: paid distribution has supportable onboarding, updates,
entitlement recovery, and no routine need to fall back to the TUI.

### Phase 3 — selective parity (8–14 engineer-weeks)

Port features by observed paid-user demand: multi-session/tile management,
advanced filters/sorts, workspace adoption and cleanup, policies/auto-fix,
on-main workflows, setup tour, theme/keymap breadth, Slack administration, and
remote-daemon ergonomics. Full pixel/keystroke parity is not a launch
requirement and should not be scheduled as one block.

The realistic path is therefore roughly 9–13 engineer-weeks from this spike to
a supported macOS v1, then 8–14 weeks for selective parity. A two-engineer team
can shorten calendar time, but signing/release and client-state work sit on the
critical path and will not scale linearly.

## Monetization and licensing recommendation

Start with an open-core paid desktop app:

- keep the current daemon, IPC contract, providers, and TUI under the repository's
  existing MIT license;
- ship the desktop UI, updater, entitlement client, and hosted account service
  as a separate proprietary product;
- sell an annual subscription with a perpetual fallback for the last version
  released during an active term, rather than making the local tool stop
  working when a subscription lapses;
- make telemetry opt-in and keep provider/agent credentials local in the OS
  keychain.

This is the lowest-risk commercial boundary because the valuable local engine
is already public and reusable, while customers pay for the polished GUI,
signed updates, cross-device/account conveniences, and support. A one-time
license creates update-funding pressure; a hosted/team product requires
principal isolation, encrypted credential storage, tenancy, billing, audit,
and operations that are explicitly not present yet.

Revisit hosted/team pricing only after the local paid app validates demand.
The remote daemon and gateway are useful foundations, but they are not a
multi-tenant control plane.
