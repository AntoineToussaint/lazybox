# lazybox — design

This is the architectural reference for lazybox. For execution status and
phased deliverables see `ROADMAP.md`. For day-to-day conventions see
`CLAUDE.md`.

## North star: an OSS project people want to use

Every design decision is filtered through: *would a new contributor
understand this in 30 minutes? Would a user try this after 5 minutes
of reading the README?* Concretely:

- **Zero-config first run.** `lazybox` with an empty `~/.lazybox/config.yaml`
  works — agents auto-detect (claude binary in PATH → Claude; codex in
  PATH → Codex). GitHub credentials come from `gh auth token`.
- **Single binary, no subprocess juggling.** In-process daemon is the
  default. Remote daemon is opt-in, not required.
- **One-paragraph pitch** at the top of the README: what lazybox is,
  what problem it solves, one GIF.
- **Every crate has a top-of-file doc comment** explaining its role in
  one paragraph. Newcomers can navigate by folder.
- **Traits are small and focused.** `Agent`, `SessionWrapper`,
  `TaskProvider`, `Store` — each is a 5-method contract at most. No
  god-interfaces.
- **Config is discoverable.** Every option has a default, a type, and
  a one-line description. A `lazybox config dump` command prints the
  effective config (defaults + user overrides + `$LAZYBOX_*` env).
- **Errors tell you what to do next.** No "Error: GraphQL: A query
  attribute must be specified" with zero context. Messages link to
  docs or suggest a remediation.

## Goals

1. **Source-agnostic.** GitHub PRs, GitHub Issues, Linear tickets,
   Jira, Gerrit — all first-class via `trait TaskProvider`. The inbox
   doesn't care where a task came from; filters let the user scope to
   one provider, one repo, one project.
2. **Agent-agnostic.** Codex, Cursor, Claude Code, generic CLI — all
   first-class via a `trait Agent` in `crates/agents/`.
3. **Client / server split.** Server owns PTYs and state; TUI is a thin
   renderer that talks to the daemon over a socket. Enables remote
   deployment (daemon on a beefy box, TUI on a laptop).
4. **No class of recurring bugs.** Component tree with single-path focus
   replaces the four-slot state desync (see `../REWRITE.md`).
5. **Multi-terminal right pane.** Shell, agent, log tail, CI output
   coexist per session — not "one terminal per PR."

## Non-goals (v2.0)

- Windows support.
- Plugin loader for agents (recompile to add — `GenericCli` covers most
  cases via YAML).
- Visual theming config.

Multi-user / multi-principal support is on the roadmap (see `ROADMAP.md` §6)
but not in v2.0.

## Architecture

```
┌───────────────────────────┐
│ lazybox (TUI client)        │   Component tree + message bus.
│ crates/tui/               │   Holds its own libghostty-vt for render.
└──────────────┬────────────┘
               │  Transport:
               │  - In-process (default): tokio mpsc channel pair
               │  - Local out-of-process: Unix socket at ~/.lazybox/v2/daemon.sock
               │  - Remote: SSH tunnel to a remote Unix socket
               │                (no TCP/TLS in v2.0 — SSH handles both)
               │  Framing: length-prefixed bincode
               │  Wire types: crates/ipc/
┌──────────────▼────────────┐
│ lazybox-server              │   Owns: SessionManager, TaskProviders,
│ crates/server/            │     WorktreeManager, PTY TerminalManager,
│                           │     AgentRuntime registry, Store, JSON API.
└───────────────────────────┘
```

### Answered design questions

| Question | Answer |
|----------|--------|
| Configurability | **Everything that could reasonably be configured, is.** Dashboard tile set + order, agent registry (name, spawn cmd, resume args, state patterns), keybindings per component, filter defaults, snooze presets. YAML, with sensible built-in defaults so empty config is fine. |
| Client / daemon communication | **Abstracted behind a `Client` trait so local == in-process.** When client and daemon are in the same process (the common case) the "transport" is a pair of tokio mpsc channels — zero serialization, zero sockets. Only when actually remote does it serialize over a socket. TUI code doesn't branch on local vs remote. |
| Session wrapper (tmux etc.) | **Abstracted via `SessionWrapper` trait.** `TmuxWrapper` is the default impl. Swappable so we can add `ScreenWrapper`, `ZellijWrapper`, or a no-wrapper "raw PTY" mode later without touching the daemon core. |
| Remote access | **SSH-tunneled Unix socket.** Server binds `~/.lazybox/v2/daemon.sock`; remote clients connect through `ssh -L`. No TCP, no TLS cert management — SSH is the trust boundary. |
| Server lifetime | **Long-running service when out-of-process.** First client auto-starts the daemon subprocess; survives client disconnect; `lazybox server stop` terminates. Same model as tmux server. For the common in-process case, daemon lives and dies with the TUI. |
| Binary | **One binary.** `lazybox` with subcommands: `lazybox` (default: TUI + in-process daemon), `lazybox server start/stop/status` (manage a standalone daemon), `lazybox server api [addr]` (foreground JSON HTTP API gateway), `lazybox --connect <socket>` (remote TUI, don't start a local daemon). |

## Crate layout

```
crates/
├── core/             source-agnostic types (Task, Session, SessionKey, …)
├── auth/             credential chain
├── store/            SQLite backend
├── config/           YAML loader
├── git-ops/          worktrees (bare clones + per-task worktrees)
├── tui-term/         PTY + ghostty-vt parser + widget
├── gh-provider/      GitHub PRs + Issues
├── linear-provider/  Linear issues via GraphQL
├── ipc/              wire types + framing + transport (channel + socket)
├── agents/           Agent trait + Claude/Codex/Cursor/GenericCli + SessionWrapper
├── server/           PTY lifecycle, polling, agent runs, JSON API gateway
└── tui/              the `lazybox` binary — component tree + key/event dispatch
```

The core libraries (`core`, `auth`, `store`) must not depend
on each other. Provider crates depend only on `core` + `auth`.

## IPC protocol

Length-prefixed bincode over the transport.

```rust
// Command: client → daemon
enum Command {
    Subscribe(SubscribeSpec),          // start streaming events
    Spawn { session_key, kind, cwd },  // agent or shell
    Write { terminal_id, bytes },
    Resize { terminal_id, cols, rows },
    Close { terminal_id },
    Kill { session_key },              // tmux + metadata
    MarkRead { session_key },
    Snooze { session_key, until },
    Merge { session_key },
    Approve { session_key },
    // ...etc
}

// Event: daemon → client (broadcast to subscribers)
enum Event {
    SessionUpserted(Session),
    SessionRemoved(SessionKey),
    TerminalSpawned { terminal_id, session_key, kind },
    TerminalOutput { terminal_id, bytes, seq },
    TerminalExited { terminal_id, code },
    AgentState { session_key, state },
    ProviderError { source, message },
    Notification { title, body },
}
```

Reconnect: daemon keeps a per-terminal ring buffer (64 KB). On
`Subscribe`, daemon replays the ring before streaming live bytes. Client
feeds bytes into its local libghostty-vt, reconstructs the screen.

## Component tree (TUI)

```rust
trait Component {
    fn keymap(&self) -> &[Binding];
    fn handle(&mut self, key: KeyEvent) -> Outcome;
    fn on_event(&mut self, ev: &Event);      // from the daemon stream
    fn render(&self, area: Rect, frame: &mut Frame, focused: bool);
    fn children(&mut self) -> &mut [Box<dyn Component>];
}

enum Outcome {
    Consumed,                // key handled, stop bubbling
    BubbleUp,                // parent handles
    Dispatch(Command),       // send to daemon
    Focus(FocusTarget),      // change focus (child id, sibling, root)
}
```

- **One focus chain**, root → leaf. Key dispatch walks innermost to
  outermost; unhandled keys bubble. Focus is one id; every other position
  is derived from it.
- **Internal bus** between components (for cross-cutting concerns that
  don't go to the daemon): selection changes, layout events. Separate
  from the daemon event stream.

### Tree sketch

```
App
├── TabBar                   — subscribes: TerminalSpawned/Exited
├── Sidebar                  — default focus
│   ├── FilterRow            — owns search + time filter
│   └── SessionList          — subscribes: SessionUpserted/Removed
│       └── SessionRow × N   — subscribes: AgentState(key)
├── RightPane                — Tab reaches here from Sidebar
│   ├── Header
│   ├── Dashboard
│   │   ├── CommentsTile
│   │   ├── CiLogTile
│   │   ├── DiffTile
│   │   └── AgentStateTile
│   └── TerminalStack        — multi-terminal per session
│       ├── AgentTerminal    — subscribes: TerminalOutput(agent_id)
│       ├── ShellTerminal    — subscribes: TerminalOutput(shell_id)
│       └── LogsTerminal     — subscribes: TerminalOutput(logs_id)
└── OverlayStack             — Help / Picker / NewWorktree /
                               ConfirmKill — steals focus when active
```

## Sources (`TaskProvider`)

`TaskProvider` lives in `lazybox-core`. Each source returns a stream of
`Task`s; lazybox doesn't care whether a task is a GitHub PR, a GitHub issue,
or a Linear ticket. All share the same row model, sidebar, status tags,
search.

### Shipping in v2.0

| Source | Crate | Notes |
|--------|-------|-------|
| GitHub PRs | `gh-provider` | octocrab polling |
| GitHub Issues | `gh-provider` | single GraphQL query alongside PRs |
| Linear | `linear-provider` | GraphQL, `LINEAR_API_KEY` env |
| GenericHttp | `http-provider` (planned) | poll any JSON endpoint, map fields via config |

### Per-source behaviors that still matter

- **Merge semantics.** PRs have "merge"; issues have "close"; Linear
  tickets have "done". The `Task` model already has a neutral
  `state: TaskState` enum; providers set it.
- **Comments / replies.** `r` to reply → provider-specific API (GH
  comment vs Linear comment vs Slack message for integrations).
- **Worktrees.** Only sources that carry a `branch` get a worktree.
  Linear tickets with no linked branch are "just an inbox row" —
  opens a notes pane instead of Claude-in-worktree. A Linear ticket
  that IS linked to a PR can be rendered as one merged row or two
  linked rows (config: `display.merge_linked_items`).
- **Auth.** Each provider builds its own `CredentialProvider` chain.
  GitHub uses `gh auth token`; Linear uses `LINEAR_API_KEY` env or
  `lazybox auth linear` flow (TBD).

### Filter UX

Search grows provider tokens:

```
source:github          # only GitHub (PRs + issues)
source:gh/pr           # only GitHub PRs
source:gh/issue        # only GitHub issues
source:linear          # only Linear
project:ENG            # Linear project key
```

All existing tokens (`needs:reply`, `ci:failed`, `role:author`,
`is:unread`, etc.) still work and compose with AND.

## LLM proxy — structured telemetry from agents

> **Superseded / not shipped.** The in-process HTTP proxy described
> below was never wired into the running daemon and the `llm-proxy`
> crate has been removed. What ships today (#222) is the simpler half of
> the idea: the daemon injects a per-provider base-URL env var
> (`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` ← `agent.llm_gateway_url`)
> so a spawned agent points at the operator's *own* gateway. lazybox
> itself does no proxying and records no telemetry from agent API
> traffic. The structured-telemetry design below is kept for reference;
> reviving it would start from `agent.llm_gateway_url` wiring in
> `crates/server/src/spawn_handler.rs` (`gateway_env_for_agent`).

Parsing PTY output to understand what an agent is doing is brittle
(it worked well enough for "working vs asking", but we hit the ceiling
fast on tool calls, token counts, cost). Instead, lazybox interposes as an
**LLM API proxy**: the daemon runs a tiny HTTP server, injects
`ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL` into the agent's env, and
captures structured metadata on every request/response.

```
┌──────────────────────────┐
│ Claude Code (in tmux)    │
│ ANTHROPIC_BASE_URL=       │
│   http://127.0.0.1:PORT/  │
└────────────┬─────────────┘
             │ /v1/messages
             ▼
┌──────────────────────────┐  records:
│ LLM Proxy                │    - timestamp, session_key
│ (in daemon)              │    - model, token counts (in/out/cache)
│                          │    - tool calls (name, args, result size)
│                          │    - request latency
│                          │    - estimated cost
│                          │    - assistant text (for summary / search)
└────────────┬─────────────┘
             │ /v1/messages  (forward upstream, verbatim)
             ▼
        api.anthropic.com
```

### What the proxy gives us

- **Token & cost per session.** Live counter in the sidebar row.
  Aggregate per day / per repo / per agent in the dashboard.
- **Tool-call timeline.** A real structured list of what the agent did
  (read file X, ran `cargo test`, edited Y:42) instead of scraping PTY
  frames. Powers a new "Activity" tile in the right pane dashboard.
- **Reliable state detection.** "Request in flight" = working,
  "response ended without tool_use" = idle, "tool_use with name
  `AskUserQuestion` or `bash` and unresolved" = asking. No more
  pattern-matching on `Esc to cancel`.
- **Search the conversation.** Because we have every assistant turn's
  text, users can `s` for prior conversations. "Find the session where
  I asked about the config_toml migration".
- **Budget guardrails.** Optional: cap $/day or tokens/day per agent;
  return a 429 that the agent surfaces as "your budget is exhausted".
- **Offline cache / replay.** Not v2.0, but the architecture allows it.

### What it deliberately does NOT do

- **Re-routing / model swap.** Proxy is read-only — it forwards the
  exact request to the exact upstream the agent chose. No "secretly
  use Haiku instead of Sonnet." That surprise is not worth the value.
- **Modification of responses.** Same reason. Observability only.
- **Policy enforcement by default.** Budgets are opt-in. Default is
  pure pass-through.

### Implementation

New crate `crates/llm-proxy/`:

```rust
pub struct ProxyConfig {
    pub listen: std::net::SocketAddr,      // typically 127.0.0.1:<ephemeral>
    pub record_bodies: bool,               // true by default
    pub redact: Vec<String>,                // headers/fields to strip from records
}

pub struct ProxyServer {
    /// Records flow out via this channel so the daemon's main loop
    /// can persist them + emit Events.
    pub records_tx: mpsc::Sender<ProxyRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRecord {
    pub session_key: SessionKey,          // pulled from a request header
                                           // we inject when spawning
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub duration: std::time::Duration,
    pub provider: ApiProvider,             // Anthropic / OpenAI / …
    pub endpoint: String,                  // "/v1/messages"
    pub request_model: Option<String>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    pub tokens_cache_create: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub tool_calls: Vec<ToolCall>,
    pub assistant_text: Option<String>,    // for search
    pub status: u16,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args_summary: String,              // truncated / structured
    pub result_size_bytes: Option<u64>,
    pub duration: Option<std::time::Duration>,
}
```

### Agent wiring

`Agent::spawn` gets a `ProxyCtx` in `SpawnCtx`:

```rust
pub struct ProxyCtx {
    pub anthropic_url: Option<String>,     // base URL to inject
    pub openai_url: Option<String>,
    pub session_tag: String,               // header value linking
                                           // requests → session_key
}
```

The daemon builds this `ProxyCtx` when starting a terminal, points the
proxy at the upstream for whichever API keys the user has configured,
and injects the env vars into the wrapped command. Agent impls don't
need to know — the env vars are standard.

### Trust model

- **Only listens on 127.0.0.1** — never exposed to the network.
- **Forwards the agent's `Authorization` header verbatim.** The
  daemon never sees the user's API key unless the user explicitly
  configures one for its own polling.
- **Bodies are recorded by default.** Opt-out via `proxy.record_bodies:
  false` in config. A redact list strips known-sensitive headers
  (cookies, extra auth) and JSON paths (`messages[].content` if the
  user wants content-free telemetry).

### Storage

Proxy records land in SQLite next to sessions. A `proxy_records`
table keyed by `(session_key, started_at)`. Separate so users can
easily `DELETE FROM proxy_records` if they change their mind about
recording bodies.

Rough size budget: a verbose Claude turn is ~50 KB of JSON; a busy day
is maybe 100 turns. So ~5 MB/day/active-session. Fine for SQLite;
users who care can enable `record_bodies: false` and keep only the
counters (tiny).

### Open questions

- **Aider / custom agents that hit a model we don't route?** The env
  vars are a hint, not a requirement. If the agent ignores them we
  just don't record that agent. Graceful degrade.
- **Streaming responses.** Proxy needs to be a duplex streamer, not
  buffer-then-forward. Hyper's streaming body types work fine; we
  tee the bytes into a parser that assembles the record as SSE frames
  arrive.
- **OpenAI vs Anthropic wire formats.** Different JSON shapes. Crate
  has a per-provider adapter module.
- **Cost estimation.** Hard-coded price table by model, updated via
  a small `prices.rs` module. Acceptable because models change slowly.

## Agent abstraction

```rust
trait Agent: Send + Sync {
    fn id(&self) -> &'static str;
    fn spawn(&self, ctx: &SpawnCtx) -> Vec<String>;   // argv for tmux inner
    fn resume(&self, ctx: &SpawnCtx) -> Vec<String>;  // --continue-like
    fn detect_state(&self, ring: &[u8]) -> Option<AgentState>;
    fn hooks(&self) -> Option<HookConfig>;            // Claude has, Codex doesn't
    fn inject_prompt(&self, prompt: &str) -> Vec<u8>; // bytes to write
}
```

Built-ins (shipped in `crates/agents/`):

- `Claude` — today's behavior: spawn cmd from config, Claude Code
  lifecycle hooks for state.
- `Codex` — spawn cmd per config, no hooks; pattern-detect state from
  recent output.
- `Cursor` — same as Codex, different pattern set.
- `GenericCli` — user-defined via YAML. Fallback for anything else.

## Server responsibilities

1. **Providers.** `TaskProvider` pollers (currently GitHub; Linear etc.
   plug in). Emit `Event::SessionUpserted` on results.
2. **Worktrees.** `WorktreeManager` owns the layout. On startup
   reconciles disk against the session DB per the rules in `REWRITE.md`.
3. **Agent runtimes.** Look up Agent by id, spawn inside tmux, hold
   `TermSession`, stream output to subscribers.
4. **Store.** SQLite persistence of sessions, read state, snooze.
5. **IPC server.** Bind Unix socket, accept connections, route
   commands, multicast events.

## Open questions

- **Config format.** YAML today; no plans to move. `lazybox config dump`
  prints the effective merged config.
- **Dashboard tile layout.** Fixed grid (2×2) or stacked (1×N with user
  reorder)? Leaning stacked, user can drag/reorder later.
- **Streaming responses in `llm-proxy`.** Hyper's streaming body types
  work; we tee the bytes into a parser that assembles the record as SSE
  frames arrive. OpenAI vs Anthropic wire formats differ — per-provider
  adapter modules.
- **Cost estimation.** Hard-coded price table by model in `prices.rs`,
  updated manually. Acceptable because models change slowly.

## Testing discipline (non-negotiable)

**Every public function has a test; every component has a render
snapshot; every bug fix lands with a regression test.**

| Layer | What gets tested |
|-------|------------------|
| `ipc` | Serde round-trip per Command/Event variant; framing on synthetic streams; property tests for arbitrary frame sizes + malformed bytes. |
| `agents` | Registry lookup; each Agent's spawn/resume argv snapshotted; SessionWrapper behaviors (tmux mocked by intercepting Command). |
| `llm-proxy` | Record serde round-trip; pricing rates for known models; Unknown returns None; redaction on headers + nested JSON; streaming SSE assembly from recorded fixtures. |
| `server` | End-to-end via `channel::pair` — Subscribe → Snapshot; PTY spawn → output stream → exit; ring buffer wraparound; reconnect replay fidelity. |
| `tui` components | Pure key-routing tests (no render). Golden render snapshots via `insta` + ratatui `TestBackend`. Event-subscription dispatch tests. Focus chain invariants. |
| providers | GraphQL fixtures checked into `tests/fixtures/`. Never hit live APIs in unit tests. One opt-in integration test per provider gated on env var. |
| Cross-crate | Integration suite exercising the full in-process stack (TUI → in-process daemon → mock provider → mock agent). |

CI matrix: Linux + macOS, `cargo test --workspace` + `cargo clippy
--workspace -- -D warnings` + `cargo fmt --check` on every PR.
`cargo test --doc` enabled. Coverage tracked via `cargo llvm-cov` —
target 80% on library crates (server/ipc/agents/llm-proxy/providers).
TUI render tests count as coverage via the ratatui TestBackend.
