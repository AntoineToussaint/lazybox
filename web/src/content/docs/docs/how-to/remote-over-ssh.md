---
title: Remote over SSH
description: Run the daemon remotely and the TUI locally over a forwarded Unix socket.
---

Goal: run the lazybox daemon on a remote machine (where your repos and agents
live) while driving the TUI from your laptop, over a forwarded Unix socket.

By default lazybox runs the daemon and TUI in the same process. Out-of-process
mode splits them: the daemon owns state and IO, and the TUI connects to it over
a Unix socket carrying length-prefixed bincode. SSH local forwarding (`-L`)
bridges the two machines.

## Prerequisites

- lazybox built and runnable on the remote host (see the
  [Quickstart](/docs/tutorials/quickstart/)).
- SSH access to the remote host.

## 1. Start the daemon on the remote host

SSH into the remote machine and start the server:

```sh
lazybox server start
lazybox server status     # confirm it is up
```

The daemon listens on a Unix socket. Note its path (it lives under the remote
host's lazybox home; `LAZYBOX_HOME` overrides where lazybox writes everything).

## 2. Forward the socket over SSH

From your laptop, forward a local socket path to the remote socket path:

```sh
ssh -L /tmp/lazybox-remote.sock:/path/to/remote/lazybox.sock user@remote-host
```

Keep this SSH session open; it carries the connection.

## 3. Connect the TUI

In another terminal on your laptop, connect the TUI to the forwarded socket:

```sh
lazybox --connect /tmp/lazybox-remote.sock
```

You now have the full inbox and embedded terminals — but the worktrees, polling,
and agent sessions all run on the remote host.

## Stopping

```sh
lazybox server stop       # run on the remote host
```

## Related

- The [CLI reference](/docs/reference/cli/) for `server` subcommands and
  `--connect`.
- The [architecture explanation](/docs/explanation/architecture/) for how the
  client/daemon split works.
