# MCP as a cross-agent coordination layer

**Status:** design / scoping — no code yet (branch `mcp` is empty vs `main`).
**Host decision:** in the daemon (`crates/server`).
**Scope of this doc:** recommend the coordination *medium*, define the MCP
tool surface, and lay out the wiring + phasing. Implementation deferred.

---

## 1. The problem

> *Sometimes I want one session to know the contents of another, especially
> across repos.*

Today, cross-agent communication in lazybox is **push-only / write-only**:

- `x s` send-to-session — capture one agent's on-screen text, inject into
  another (#431; `dispatch.rs:2593`, delivered via the settle-gated
  `DeliverSnippet` path).
- `Shift-B` broadcast, `]]s` snippets, `Shift-W` spawn-from-anywhere.

An agent can be *told* something. It cannot *ask* "what does session X know?"
The manual `/status` → copy Session-ID → reference-it dance is a workaround
for the missing **pull / read** half. An MCP is the natural way to add it.

## 2. Does lazybox ship an MCP today?

No. Lazybox is an MCP **consumer**: spawned Claude agents inherit the user's
ambient MCP servers unless `agent.strict_mcp` is set, which adds
`--strict-mcp-config` (`crates/config/src/lib.rs:2064`, #1183/#1232). The
current design principle is explicitly the opposite of wrapping agent actions
behind a tool layer (CLAUDE.md:158). This proposal does **not** change that
principle for repo actions (`gh`/`git` stay direct) — it adds a *coordination*
surface, which is a different concern.

## 3. Key finding: we are ~70% there already

The daemon's JSON API gateway (`crates/server/src/api_gateway.rs`, #773)
already exposes the hard parts over loopback HTTP + bearer auth:

| Capability | Already exists |
|---|---|
| Discover sessions across repos | `GET /v1/workspaces`, `/v1/agents` |
| Read another session's recent output | `POST /v1/agents/output` → `spawn_handler::agent_output_snapshot` (cleaned, line-limited tail of the ring buffer) |
| Push into another session | `POST /v1/agents/inject` → settle-gated inject (`settle_submit_and_confirm`, `spawn_handler.rs:7247`) |
| Durable arbitrary shared state | `Store` kv: `get_kv` / `set_kv` / `list_kv_prefix` / `apply_batch` (`crates/store/src/traits.rs:207`) — arbitrary string values by prefix |
| Per-session identity for spawns | `LAZYBOX_SESSION_KEY` injected at spawn (`spawn_plan.rs:234`) |

So an MCP server is mostly a **protocol facade** (MCP tool schemas +
transport) over capabilities the daemon already has, **plus one new layer**:
a notes blackboard on the kv store.

## 4. Recommended medium

The three candidate media are not alternatives — they're a spectrum from
**zero-effort/noisy** to **high-effort/clean**:

1. **Tap live session output** — read another session's ring buffer. Zero new
   state, but raw terminal scrollback is noisy (ANSI, TUI redraws) and dumps
   thousands of lines into the reader's context budget. Best as a *fallback*
   for "what is A doing right now."
2. **Explicit shared notes** — agents publish distilled context to a
   blackboard; readers pull compact, opt-in signal. Persistent, low-noise,
   zero schema. **Best as the primary medium.**
3. **Structured context blocks** — typed decisions/contracts. Richest but
   brittle and premature; grow into it later via an optional `tags` field.

**Recommendation: a pull-based shared *notes blackboard* as the primary
medium, with on-demand session-output read as the fallback, and structured
blocks deferred.** Rationale:

- The valuable content is **decisions/context, not raw scrollback**. Notes
  capture the distilled signal; the output tap is the escape hatch.
- Freeform-markdown notes need **zero schema**, sidestepping option 3's
  brittleness; an optional `tags: []` field is the seam to grow into structure
  once real usage shows what's needed.
- Notes persist in the **kv store** (survive restart); ring buffers are
  bounded/ephemeral. Notes are the durable layer; the tap is "right now."
- Keep **push** (inject) as-is and expose it as one MCP tool for when you
  *do* want to actively poke another agent. MCP then covers the full duplex:
  **pull (notes + output) + push (notify)**.

This directly answers your framing: *prompt-inject is the push half (keep it),
MCP read is the pull half (new)* — together, a two-way coordination bus.

## 5. Tool surface

Six tools, namespaced `lazybox_*`. Identity is **implicit from the
connection** (see §6), so no tool takes a "who am I" argument.

| Tool | Purpose | Backed by |
|---|---|---|
| `lazybox_whoami()` | Your session key, workspace, repo, branch. Replaces the `/status` Session-ID hunt. | connection token → `SessionKey` |
| `lazybox_list_sessions(filter?)` | Discover sibling sessions: workspace, repo, agent state, last-active. | `/v1/workspaces` + agent state |
| `lazybox_read_session(workspace, tail?)` | On-demand tail of another session's output (the "right now" fallback). | `agent_output_snapshot` |
| `lazybox_post_note(text, scope?, tags?)` | Publish to the blackboard. Default `scope` = your own session. | kv `lazybox:note:<scope>:<seq>` |
| `lazybox_read_notes(scope?, tags?, since?)` | Read the blackboard (defaults to global + your scope). | `list_kv_prefix("lazybox:note:")` |
| `lazybox_notify_session(workspace, text, submit?)` | Active push into another agent (the existing inject, as a tool). | `/v1/agents/inject` (settle-gated) |

**Note record** (kv value, JSON): `{ author, scope, tags[], ts, text }`.
**Scope** = a `SessionKey`/`WorkspaceKey` string, or `global`. Because the
daemon spans every workspace, notes are cross-repo by construction. Bound the
ring per scope (prune oldest by count/bytes) so the blackboard can't grow
unbounded.

## 6. Wiring: implicit identity is the whole trick

The reason a daemon-hosted MCP beats the `/status` workaround: each agent's
connection is *already* bound to its session, so `whoami` and the default
note scope need no manual ID passing.

- **Transport:** streamable-HTTP MCP endpoint mounted on the existing gateway
  (loopback-only, bearer). Reuses `principal_for_request` / `check_bearer_token`
  (`api_gateway.rs:1363`). *Not* stdio — the daemon is long-lived and serves
  many agents; a subprocess-per-agent stdio server would be awkward.
- **Per-session token:** at spawn, `spawn_plan.rs` mints a per-session bearer
  and the daemon maps `token → SessionKey`. The token rides the MCP config so
  every call self-identifies.
- **Registering the server with the agent:** today lazybox pushes
  `--strict-mcp-config` but **never emits `--mcp-config`** (grep-confirmed), so
  under strict mode *zero* servers load. The fix — write a per-session
  `--mcp-config` file pointing at the daemon endpoint — slots into the same
  argv builders in `crates/agents/src/agent.rs` (`push_unattended_flags`,
  `agent.rs:672`). Nice side effect: **strict mode + the lazybox MCP = a clean,
  controlled tool surface** (only lazybox's server loads), turning today's
  strict-mode footgun into a feature.

## 7. Where the code lives (layering)

`lazybox-server` is the widest library crate and the correct home
(`dep_rules.rs:91`): it already owns the gateway, the ring buffers
(`pty.rs`), the settle-gated inject (`spawn_handler.rs`), and reaches
`Store` (kv), `SessionKey`/`WorkspaceKey` (core) — all within already-blessed
dep edges. The `lazybox mcp …` / gateway subcommand wiring goes in `tui-boot`
(pattern: `main.rs:2340`). It must **not** live in `tui` (forbidden from
`store`/`server`). The `rmcp` dependency (see §7a) is a third-party crate, so
it does not touch `dep_rules.rs` (which polices *internal* workspace edges) —
but pin it in the workspace `Cargo.toml` and gate its transport behind the
minimal feature set.

## 7a. SDK decision: adopt `rmcp`

Resolved in favor of the official Rust SDK,
[`rmcp`](https://crates.io/crates/rmcp)
([modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)),
over hand-rolling. Rationale: the MCP wire protocol churns (spec revisions,
capability negotiation, the `initialize` handshake, streamable-HTTP framing),
and the SDK absorbs that churn for a **one-time dep cost** — marginal next to
the SQLite build already in the tree. `rmcp` tracks the current spec
(2026-07-28, compatible back to 2025-11-25) and ships a
`transport-streamable-http-server` feature (off by default), which matches the
transport chosen in §6. Tool schemas derive from Rust types via its macros,
cutting the boilerplate of six hand-written JSON-RPC handlers.

Integration notes to verify at implementation time:

- **Mounting against the hyper-1 gateway.** `rmcp`'s streamable-HTTP server
  transport is built on the tower/axum stack. Confirm whether it mounts as a
  standalone service on its own loopback port or can be layered onto the
  existing hyper gateway; either is fine (both are loopback + bearer), but it
  decides whether §6's endpoint shares the gateway port or gets its own.
- **Feature minimalism.** Enable only `transport-streamable-http-server` (+
  server/tool macros); leave `client`, `auth`, `elicitation`, etc. off to keep
  the transitive tree (schemars, tower) small.
- **Auth bridge.** Reuse the gateway's per-session bearer (§6) rather than
  `rmcp`'s own auth feature — identity is minted at spawn, not negotiated.

## 8. Phasing

- **Phase 0 — read loop (proves it, zero new state): ✅ landed.** `rmcp 3.2`
  streamable-HTTP server (`crates/server/src/mcp.rs`), `whoami` /
  `list_sessions` / `read_session` over the existing agent snapshot + ring
  buffers, a per-session `TokenRegistry` for implicit identity
  (`ServerConfig::mcp`), a loopback listener started at daemon boot
  (`mcp::start`), and spawn-time provisioning (`provision_for_spawn` →
  per-session `.mcp.json` + `--mcp-config`, gated by
  `Agent::supports_mcp_config`). Verified by unit tests; two follow-ups
  remain — an `rmcp`-client end-to-end round trip, and token cleanup on
  session reap (respawn already self-heals by clearing the prior token).
- **Phase 1 — notes blackboard:** `post_note` / `read_notes` over kv
  (`lazybox:note:*`), with per-scope pruning. This is the primary medium.
- **Phase 2 — push + growth:** `notify_session` (wrap existing inject);
  optional `tags`-driven structure once usage justifies it.

The Phase 0 integration questions from §7a resolved in code: `rmcp`'s
streamable-HTTP service wraps onto the **existing hyper stack** via
`TowerToHyperService` (no axum listener), so §6's endpoint gets its **own
loopback port** — the same loopback-bind-at-boot + inject-at-spawn shape the
metering proxy and local gateway already use.

## 9. Open questions

1. **Note trust boundary:** notes are shared across all local sessions (single
   user, one machine). A note read into agent B is content authored by agent A
   — same trust domain, but worth a one-line caveat that it's untrusted-ish
   text (don't let it silently drive destructive actions).
2. **Default read scope:** global + own session, or explicit-only? Global is
   more discoverable but noisier as session count grows.
3. **Notification of new notes:** pure pull (agent must ask), or emit a
   gateway `/v1/events` signal so an interested agent can be nudged? Pull-only
   for v1 is simpler.
4. **Retention:** how many notes per scope / total bytes before pruning, and
   do notes outlive their authoring session?
