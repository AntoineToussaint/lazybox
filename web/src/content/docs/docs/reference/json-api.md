---
title: JSON HTTP API
description: Drive the daemon over HTTP — start the gateway, authenticate, and call the command and workspace endpoints.
---

The daemon can expose a JSON HTTP API so non-terminal clients (scripts, a Tauri
or iOS app, another host) can read the inbox and issue commands. It's the same
event bus the TUI consumes, surfaced over HTTP.

## Start the gateway

```sh
lazybox server api [addr:port] [--insecure-no-auth]
```

The listen address resolves in order: the `[addr:port]` argument, then the
`LAZYBOX_API_ADDR` environment variable, then the default `127.0.0.1:8787`.

## Authentication

The gateway **refuses to start without an explicit auth decision**:

- Set `LAZYBOX_API_TOKEN` and clients send `Authorization: Bearer <token>`, or
- Pass `--insecure-no-auth` to serve unauthenticated (a warning is printed).

There is no built-in TLS, so **non-loopback binds are refused**. Forward the
loopback port through an encrypted SSH tunnel for remote use. Direct routable
transport remains disabled until the daemon provides encryption and
principal-scoped authorization. See
[`SECURITY.md`](https://github.com/AntoineToussaint/lazybox/blob/main/SECURITY.md).

## Browser web control

The loopback gateway serves a responsive **web-control client** at `/`. Open the
gateway URL on the daemon host, enter its bearer token, and the page loads the
current workspaces, streams live daemon events, and lets you drive the daemon:
select a workspace, spawn an agent or a shell, read a running agent's output,
and send it an instruction. It renders with the shared **Lazybox Dark** palette
(the same chrome the TUI and desktop use), so it is a first-class peer of those
clients rather than a bare debug page. Leave the token blank only when the
gateway was explicitly started with `--insecure-no-auth`. A submitted token is
cleared from the form, retained only in page memory, and reused when
reconnecting.

The desktop app links to this same page — its 🌐 topbar button opens the
web-control client for whichever gateway the desktop is attached to (embedded
loopback or a remote one), so "one control shell — TUI, desktop, and browser"
all speak the same `/v1` gateway.

### What web control can and cannot do

Web control drives the **text-oriented** slice of the gateway, so it stays a
single self-contained page with no binary terminal codec:

- **Can:** browse workspaces, watch the live event stream, spawn agents/shells
  (`POST /v1/commands`), read a running agent's cleaned output tail
  (`POST /v1/agents/output`), and hand an agent free-form work or a snippet body
  (`POST /v1/agents/inject`).
- **Cannot (yet):** attach a live, interactive xterm view of raw terminal bytes
  (the binary `POST /v1/terminal` stream the desktop consumes), or reach the
  full TUI/desktop action catalog (merge/reviewers/policies/snippet-picker).
  Those remain desktop/TUI affordances — see the parity notes.

The page at `/` is intentionally available without authentication so a browser
can load it. When a token is configured, every `/v1/*` request it makes — reads
**and** the control POSTs above — is still bearer-authenticated. The client is
served by the gateway itself, so it does not need a permissive cross-origin
policy.

The gateway does not expose this client on a routable listener; it only binds
loopback. For remote use, forward the loopback port over SSH
(`ssh -L 8787:127.0.0.1:8787 host`) and open the forwarded port in a local
browser — the bearer token and same-origin `/v1` requests work unchanged across
the tunnel. Remote product transport still requires encryption and
principal-scoped authorization.

The exact request/response shapes the page depends on are pinned by a contract
gate (`web_control_contract_fixture_is_current` in the Rust test suite, mirrored
by `web/scripts/api-client.test.mjs`), so a `/v1` wire change that would break
web control fails the build instead of drifting silently.

## Endpoints

### `GET /`

Returns the browser web-control client (`api_client.html`). It is the only
unauthenticated route when a bearer token is configured; every `/v1/*` call the
page then makes still carries the token.

### `GET /v1/protocol`

Discovers the current protocol version, Rust IPC fingerprint, daemon build,
binary terminal media type, and terminal frame/write limits. Versioned clients
send the returned version in `x-lazybox-protocol-version`; unsupported versions
receive HTTP 426 with the requested and supported values.

### `POST /v1/commands`

Issues a command and **waits for the command handler to finish** before
returning:

```json
{ "ok": true, "completed": true }
```

Provider/domain outcomes (a merge landing, a poll completing) still arrive as
normal events rather than in the response body. Streaming requests are
connection- and command-bounded: a client must reconnect after the documented
stream limit instead of growing an unbounded daemon queue.

### `GET /v1/workspaces`

Returns the current workspaces. The response includes a `warnings` array when an
unreadable row was preserved and omitted from the decoded workspace list — so a
single corrupt record surfaces as a warning instead of failing the whole
listing (see [Recover persistent state](/docs/how-to/recover-state/)).

### `GET /v1/agents`

Returns which agent is running in which workspace — terminal id, workspace key,
agent id, lifecycle state, and the last prompt — so a caller can tell what each
agent is doing without scraping any PTY. Web control uses it to badge the
workspace rows.

### `POST /v1/agents/output`

Reads a running agent's recent output as a cleaned, line-limited text tail
(`{ "workspace": "<key>", "tail": 200 }`). The web-control client polls this to
show terminal output without the binary stream. Returns 404 when the workspace
has no running agent.

### `POST /v1/agents/inject`

Delivers an instruction or snippet body to a workspace's running agent
(`{ "workspace": "<key>", "text": "…", "submit": true }`) through the same
settle-gated inject path the TUI uses, so a paste never lands in a permission
prompt. `accepted` reports only that the workspace resolved to a running agent
and the prompt was handed off; a later drop surfaces on `/v1/events` as
`TerminalInputRejected`.

### `POST /v1/terminal`

Streams bounded, length-prefixed binary terminal frames for xterm-compatible
clients. Input, resize, resync, and close commands use the request body;
snapshots, output, scrollback, and resync results use the response body. Raw
terminal bytes never travel as JSON number arrays. (Consumed by the desktop app;
the browser web-control client uses `/v1/agents/output` instead.)

## Example

```sh
export LAZYBOX_API_TOKEN=$(openssl rand -hex 32)
lazybox server api 127.0.0.1:8787 &

curl -s http://127.0.0.1:8787/v1/workspaces \
  -H "Authorization: Bearer $LAZYBOX_API_TOKEN"
```

## See also

- [CLI reference → `lazybox server`](/docs/reference/cli/) — the full command
  and its flags.
- [Remote over SSH](/docs/how-to/remote-over-ssh/) — forward the daemon socket
  (or this port) to another machine.
