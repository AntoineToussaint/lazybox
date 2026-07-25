---
title: JSON HTTP API
description: Drive the daemon over HTTP — start the gateway, authenticate, and call the command and workspace endpoints.
---

The daemon can expose a JSON HTTP API so non-terminal clients (scripts, a Tauri
or iOS app, another host) can read the inbox and issue commands. It's the same
event bus the TUI consumes, surfaced over HTTP.

## Start the gateway

```sh
lazybox server api [addr:port] [--insecure-no-auth] [--allow-insecure-http]
```

The listen address resolves in order: the `[addr:port]` argument, then the
`LAZYBOX_API_ADDR` environment variable, then the default `127.0.0.1:8787`.

## Authentication

The gateway **refuses to start without an explicit auth decision**:

- Set `LAZYBOX_API_TOKEN` and clients send `Authorization: Bearer <token>`, or
- Pass `--insecure-no-auth` to serve unauthenticated (a warning is printed).

There is no built-in TLS. A **non-loopback bind is refused** unless you also
pass `--allow-insecure-http` — use that acknowledgement only behind an
authenticated TLS reverse proxy, an SSH tunnel, or a trusted private overlay
network. See [`SECURITY.md`](https://github.com/AntoineToussaint/lazybox/blob/main/SECURITY.md).

## Endpoints

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
