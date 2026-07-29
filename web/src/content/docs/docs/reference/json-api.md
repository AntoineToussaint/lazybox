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

## Local browser PoC

The loopback gateway serves a responsive, read-only client at `/`. Open the
gateway URL on the daemon host, enter its bearer token, and the page loads the
current workspaces and streams live daemon events. Leave the token blank only
when the gateway was explicitly started with `--insecure-no-auth`. A submitted
token is cleared from the form, retained only in page memory, and reused when
reconnecting.

The static page is intentionally available without authentication so a browser
can load it. When a token is configured, every `/v1/*` request it makes is
still bearer-authenticated. The client is served by the gateway itself, so the
PoC does not need a permissive cross-origin policy.

The gateway does not expose this client on a routable listener. Remote product
transport still requires encryption and principal-scoped authorization.

## Endpoints

### `GET /`

Returns the static browser shell. It is the only unauthenticated route when a
bearer token is configured.

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

### `POST /v1/terminal`

Streams bounded, length-prefixed binary terminal frames for xterm-compatible
clients. Input, resize, resync, and close commands use the request body;
snapshots, output, scrollback, and resync results use the response body. Raw
terminal bytes never travel as JSON number arrays.

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
