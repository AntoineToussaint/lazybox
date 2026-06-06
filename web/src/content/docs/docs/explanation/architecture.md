---
title: Architecture
description: Client/daemon split, the crates, the event bus, the credential chain, and the embedded terminal.
---

This page explains how lazybox is put together and why. For the canonical,
always-current detail, read
[CLAUDE.md](https://github.com/AntoineToussaint/lazybox/blob/main/CLAUDE.md) and
[DESIGN.md](https://github.com/AntoineToussaint/lazybox/blob/main/DESIGN.md) in
the repository.

## Client / daemon split

lazybox is organized as a **client/daemon split**. The daemon (server) owns all
state and IO — the PTYs behind embedded terminals, provider polling, and the
store. The TUI is a thin renderer on top.

By default both halves run in the **same process**, connected by a tokio mpsc
channel pair, so there is no serialization cost and nothing extra to launch. The
same code can run **out of process**: the daemon exposes a Unix socket speaking
length-prefixed bincode, and the TUI connects over it. Because it's a Unix
socket, SSH local forwarding (`ssh -L`) carries it across machines — that's how
[remote over SSH](/docs/how-to/remote-over-ssh/) works.

## The crates

lazybox is built from 16 lazybox crates (plus two vendored libghostty crates),
split across shared libraries, providers, the daemon, and the client binary. The
four **core** libraries — core, auth, events, and store — are deliberately
isolated: they never depend on each other. That keeps the foundation acyclic and
each concern independently testable.

- **core** — the source-agnostic domain types (Task, Session, Activity, time
  helpers).
- **auth** — the credential provider trait and chain.
- **events** — the in-process event bus.
- **store** — the persistence trait and its SQLite backend.

Providers (GitHub, Linear) depend only on core, events, and auth. The daemon
side wires everything together; the client renders it.

## The event bus

The reactive behaviour described in the [mental model](/docs/explanation/mental-model/)
is built on an in-process broadcast bus inside the daemon. **Providers produce**
events as they poll upstream; **subscribers consume** them — the TUI to render,
the JSON API gateway to forward. This producer/subscriber decoupling is why
adding a new source (or a new consumer) doesn't require touching the others.

## The credential chain

Authentication is a chain of credential providers tried in order, so the common
case needs zero configuration. For GitHub the chain resolves the token from
`gh auth token` by default, falling back to the `GH_TOKEN` / `GITHUB_TOKEN`
environment variables. The chain is trait-based and extensible (env, command,
and static providers exist today).

## The embedded terminal

Each workspace's terminal is a real PTY, read on a dedicated thread, parsed by a
vendored **libghostty-vt** parser, and rendered as a widget — the same component
both the daemon (which owns the PTY) and the TUI (which replays it) use. The
daemon keeps a per-terminal **ring buffer** so that when a client reconnects, the
recent screen contents replay instantly instead of starting blank.

## Where to go next

- The [mental model](/docs/explanation/mental-model/) for the concepts these
  mechanisms serve.
- [CLAUDE.md](https://github.com/AntoineToussaint/lazybox/blob/main/CLAUDE.md) and
  [DESIGN.md](https://github.com/AntoineToussaint/lazybox/blob/main/DESIGN.md) for
  the deep, current architecture notes.
