# BYO remote box — runbook

_Run your agents on a cloud box you own (an EC2 instance, a home server, a
GPU box), and drive them from the TUI on your laptop. Lazybox hosts
nothing: it's **your** box, **your** credentials, one trusted operator._

This is the SSH-tunnel path (v0). SSH provides the encryption and identity;
lazybox adds nothing of its own on the wire. For the design rationale and the
single-user trust assumptions, see [`remote-daemon-scoping.md`][scoping]; for
the one-scan consumer pairing that builds on this, see #749.

## On the box (machine B)

1. Install lazybox and its agent CLIs (`claude`, `codex`, `gh`, …). The
   **daemon runs the agents**, so these must exist on the box, not on your
   laptop.
2. Authenticate the providers you use as the daemon's process environment —
   `gh auth login` (or `GH_TOKEN`), `LINEAR_API_KEY`, Slack tokens. The daemon
   acts as this single identity for every connected client.
3. Start the standalone daemon:

   ```sh
   lazybox server start        # long-lived daemon on ~/.lazybox/run/daemon.sock
   ```

   It survives client disconnects like a tmux server, keeping every PTY,
   agent, and poll loop alive between sessions. For an always-on box, put it
   under a service unit (systemd / launchd) with restart-on-crash — `server
   start` runs in the foreground.

## On your laptop (machine A)

Forward the daemon's socket over SSH, then attach a local TUI:

```sh
ssh -L /tmp/lazybox.sock:$HOME/.lazybox/run/daemon.sock user@box
lazybox --connect /tmp/lazybox.sock
```

You get the full inbox, and every shell (`s`) and agent (`a c`, `w w`, …)
you spawn runs **on the box** — its terminals stream back over the tunnel.

### Reconnect is automatic

A dropped socket — laptop sleep, wifi change, the SSH tunnel resetting — no
longer kills the session. The `--connect` client re-dials the socket on its
own with capped backoff, re-`Subscribe`s, and the daemon replays a resync
snapshot (workspaces plus each terminal's ring buffer), so the screen
reconstructs without a manual restart. While it's re-dialing, a
`⟳ daemon connection lost — reconnecting…` banner shows so an extended outage
isn't a silent freeze; it clears the moment the link is back. Re-establish the
`ssh -L` forward (or use an autossh/`ServerAliveInterval` keepalive) and the
client reattaches as soon as the socket is back.

A flapping endpoint (connects, then drops immediately) is backed off
progressively instead of hammered, and if the box comes back on an
incompatible lazybox build the client stops retrying and shows the usual
disconnect banner naming the build mismatch.

## What degrades under remote

Client-local actions that need a **local** filesystem are unavailable or
adjusted when attached to a remote daemon, because paths in the inbox are the
box's paths, not your laptop's:

- **Editor (`e`)** declines with a notice steering you to the remote-safe
  server shell (`s`), which is a PTY on the box.
- **Shell (`s`) and agents** are server PTYs and work unchanged.
- **Browser/URL open** and **OS notifications** still fire on your laptop —
  the daemon just sends the trigger.

The agents you can spawn (`a c`, `a x`, `w w`, …) reflect the **box's**
configured agents (its `setup.agents`), not your laptop's, and `w` defaults
to the box's configured default agent. The daemon reports both on connect, so
a remote client offers exactly what machine B is set up to run — you won't be
missing an agent the box runs, or offered one it isn't configured for, just
because your laptop's own config differs. (Availability follows the box's
configuration; an agent enabled in config but not actually installed on the
box still fails at spawn, same as it would locally.)

## Build parity

Daemon and client must be built from the same commit — the wire handshake
rejects a fingerprint mismatch. If `--connect` reports a mismatch, rebuild the
daemon on the box (or the client) so both sides match, then reconnect.

[scoping]: remote-daemon-scoping.md
