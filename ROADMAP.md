# Lazybox Roadmap

This roadmap tracks execution status against the architecture in
`DESIGN.md`. The server/client split underpins:

- the current Rust TUI client with native terminal emulation;
- structured Claude Code `stream-json` runs for non-terminal clients;
- local desktop clients such as Tauri;
- remote clients such as iOS over a daemon-owned API.

## 0. Baseline: Terminal Client

Status: done.

- Server owns sessions, PTYs, persistence, polling, and terminal replay.
- Rust TUI is a client that renders terminal bytes locally.
- Terminal mode remains the compatibility path for Claude Code, Codex,
  Cursor Agent, shells, and logs.

## 1. Structured Agent Runtime

Status: foundation implemented.

The server can now launch an agent through a structured runtime surface:

- `Command::StartAgentRun`
- `Command::SendAgentInput`
- `Command::InterruptAgentRun`
- approval/question response commands
- `Event::AgentRunStarted`
- raw JSON preservation via `Event::AgentRawJson`
- normalized text, tool, usage, turn-finished, and run-finished events

Claude Code is launched with:

```text
claude -p --input-format stream-json --output-format stream-json
  --include-partial-messages --include-hook-events --replay-user-messages
```

Important semantics:

- A Claude `result` line finishes one turn, not necessarily the process.
  Lazybox emits `AgentTurnFinished` for that.
- `AgentRunFinished` means the child process exited or the run was
  interrupted.
- Raw JSON is always forwarded so clients can adopt new Claude event
  fields before Lazybox has normalized them.

Next:

- Persist structured run metadata so reconnecting clients can discover
  active runs and recent events.
- Decide whether terminal and structured modes should be mirrorable for
  the same agent process, or remain separate launch modes.
- Add a production client-side model that accumulates text/tool deltas
  into a stable view state.

## 2. JSON API Gateway

Status: foundation implemented.

The server exposes an HTTP gateway for non-Rust clients:

- `GET /v1/health`
- `GET /v1/workspaces`
- `GET /v1/events`
- `POST /v1/commands`
- `POST /v1/stream`

Streaming endpoints use newline-delimited JSON:

```json
{"type":"Command","payload":{"StartAgentRun":{"session_key":"github:o/r#1","session_id":null,"agent":"claude","mode":"StreamJson","cwd":null,"initial_input":{"text":"Review this PR","json":null}}}}
{"type":"Event","payload":{"AgentAssistantTextDelta":{"run_id":1,"delta":"..."}}}
```

Defaults:

- Bind address: `127.0.0.1:8787`
- Override with: `LAZYBOX_API_ADDR`
- Bearer token: `LAZYBOX_API_TOKEN` (required unless `--insecure-no-auth`)
- Launch with: `lazybox server api [addr:port] [--insecure-no-auth]`

Next:

- Add OpenAPI-style schema docs for command/event JSON.
- Add connection/session ids to the API layer for better diagnostics.
- Decide whether to add WebSocket in addition to NDJSON. NDJSON is
  enough for Tauri/iOS prototypes and keeps dependencies small.
- Add explicit CORS policy only when there is a browser-based client
  that needs it.

## 3. Reconnect And Persistence

Status: partial for terminals; pending for structured runs.

Terminal replay already has bounded ring buffers and snapshot replay.
Structured runs need the same treatment:

- Persist active run records keyed by `AgentRunId`.
- Keep a bounded event replay buffer per structured run.
- Include structured run snapshots in `Command::Subscribe` or a new
  targeted subscribe command.
- Let API clients reconnect and resume rendering without scraping raw
  logs or restarting Claude.

## 4. Client SDK Surface

Status: pending.

Build a small protocol package for clients:

- TypeScript package for Tauri and web experiments.
- Swift package or generated models for iOS.
- Shared examples for:
  - list workspaces;
  - start Claude in `StreamJson`;
  - send user input;
  - render assistant deltas;
  - render tool calls;
  - interrupt a run.

The SDK should treat raw JSON as an escape hatch, but normal clients
should build on the normalized `Agent*` events.

## 5. Security Model

Status: localhost-only foundation.

Current API is intended for local clients:

- localhost bind by default;
- optional bearer token;
- no CORS by default.

Before remote/mobile use:

- Require bearer auth for non-loopback binds.
- Document SSH tunnel and reverse tunnel setups.
- Add token rotation and a generated-token bootstrap command.
- Consider mTLS or platform-specific secure pairing only after the
  product shape is clear.

## 6. Multi-User Auth And Provider Credentials

Status: design needed before more provider work.

The current code is effectively single-user: provider polling resolves
GitHub and Linear credentials from the server process environment
(`GH_TOKEN`, `GITHUB_TOKEN`, `gh auth token`, `LINEAR_API_KEY`). That is
wrong for a shared daemon. The server must know which human/principal a
client represents and must use that principal's provider credentials.

Required split:

- **Lazybox client auth**: proves which Lazybox user is connected to the
  daemon.
- **Provider auth**: GitHub, Linear, Jira, etc. credentials owned by
  that Lazybox user.
- **Workspace ownership**: every workspace/session/run belongs to a
  Lazybox user or shared project and all reads/writes are scoped through
  that ownership.

Target model:

- Every API connection authenticates as a `PrincipalId`.
- Every command carries an implicit or explicit `principal_id`.
- `ServerConfig` owns an `AuthService` / `CredentialStore`.
- Provider polling is per principal, not process-global.
- Workspaces are keyed by `(principal_id, workspace_key)` or equivalent.
- Agent runs inherit the principal from the workspace/session they run in.

Credential bootstrap options:

- Local desktop client can resolve a local credential (`gh auth token`,
  OAuth device flow, keychain) and upload a provider credential grant to
  the server over an authenticated local channel.
- Remote/mobile client should use an OAuth/device-flow style grant so
  the server receives a refreshable provider token without depending on
  local CLI tools.
- Environment fallback stays available only for single-user dev mode and
  should be tagged as a server-local principal.

Storage rules:

- Never persist raw provider tokens in normal workspace rows.
- Store credentials in a dedicated encrypted credential table or OS
  keychain-backed store.
- Persist source metadata and scopes separately from secret material.
- Redact provider credentials from logs, snapshots, API frames, and
  debug events.

Minimum implementation sequence:

1. Add `PrincipalId` to IPC/API connection context.
2. Add credential store traits: `put_provider_credential`,
   `get_provider_credential`, `delete_provider_credential`, `list_sources`.
3. Add `Command::UpsertProviderCredential` and
   `Command::RemoveProviderCredential` for local bootstrap.
4. Change polling source construction from process env to
   `CredentialStore::get(principal, provider)`.
5. Scope workspace listing, polling, and command handling by principal.
6. Keep a dev-mode fallback that creates one `local` principal from
   process env so existing local workflows keep working.

Open questions:

- Whether the first production credential store is SQLite + local
  encryption key, macOS Keychain, or a pluggable trait with SQLite
  dev-mode first.
- Whether shared/team workspaces are in scope for v2.0, or if the initial
  daemon is multi-user but workspaces remain private per principal.
- Token refresh: OAuth providers need refresh jobs; PAT-style tokens
  only need validation and revocation.

## 7. UI Clients

Status: pending.

Tauri:

- Use the HTTP NDJSON API locally.
- Render structured agent output without terminal emulation.
- Keep terminal mode available for shell/log panes.

iOS:

- Connect through a user-controlled tunnel or relay.
- Treat the server as the owner of sessions and worktrees.
- Render workspaces, structured agent turns, tool calls, approvals,
  and questions.

## 8. Hardening

Status: in progress.

Already covered:

- IPC bincode round-trip coverage.
- Claude stream-json parser coverage.
- Server structured-run integration test with a fake Claude process.
- API stream integration test that starts a structured run.

Next:

- Add long-running multi-turn fake process tests.
- Add interrupted-run tests that verify child cleanup.
- Add API backpressure tests for slow event consumers.
- Add golden JSON fixtures for command/event compatibility.
