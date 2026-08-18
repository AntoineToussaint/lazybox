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
   under a service unit with restart-on-crash — `server start` runs in the
   foreground. Packaged systemd units (`lazybox-daemon@`, `lazybox-api@`) live
   in [`contrib/systemd/`][systemd] so the box auto-starts the daemon on boot.

   On a cloud box you pay for by the hour (a per-user GCE box), don't leave it
   always-on — give it a lifecycle instead. [`contrib/box-lifecycle/`][lifecycle]
   ships a stop-on-idle timer that stops the instance when no tunnel is
   connected and no agent is working, and a `connect.sh` that starts it back up
   on connect. See its README for install.

## On your laptop (machine A)

Forward the daemon's socket over SSH, then attach a local TUI:

```sh
ssh -L /tmp/lazybox.sock:$HOME/.lazybox/run/daemon.sock user@box
lazybox --connect /tmp/lazybox.sock
```

You get the full inbox, and every shell (`s`) and agent (`a c`, `w w`, …)
you spawn runs **on the box** — its terminals stream back over the tunnel.

## Link a box to a hosted account

Self-hosted SSH and relay use does not require a lazybox-platform account. To
use a hosted, entitlement-enforcing relay, create a one-time claim code in the
platform UI and run this **on the box**:

```sh
lazybox account claim ABCD1234 --name my-dev-box
lazybox account status
```

`LAZYBOX_PLATFORM_URL` or `--platform-url` selects a non-production platform;
otherwise the command uses `https://platform.lazybox.ai`. Claim sends only the
one-time code, the box's Ed25519 **public** key, and the display name. Lazybox
stores only the returned organization/device ids, plan and entitlement state
in the owner-only `~/.lazybox/config.yaml`; the code and private key are never
written there.

`lazybox device mint` remains fail-open for local/self-hosted use, but warns
before minting when the box is unlinked, unentitled, or its cached entitlement
is unknown, so a hosted relay refusal is not a surprise.

### Let lazybox own the forward

Rather than run the `ssh -L` yourself (and babysit it with `autossh`),
declare the forward under `remote.tunnel` and `--connect` spawns and
keepalive-supervises it for you before it dials:

```yaml
remote:
  tunnel:
    mode: ssh                                       # ssh | iap
    host: user@box                                  # ssh destination
    remote_socket: /home/user/.lazybox/run/daemon.sock
    ports: [3000, 8082]                             # workload ports, localhost-bound
```

```sh
lazybox --connect /tmp/lazybox.sock
```

The supervisor forwards the daemon socket (to the `--connect` path) **and**
each workload port (`localhost:<p>` → `localhost:<p>` on the box), with SSH
keepalive (`ServerAliveInterval`) and `ExitOnForwardFailure` so a dead link
is detected and re-established on capped-exponential backoff — no external
`autossh`. sshd does not expand `~`, so give `remote_socket` as an absolute
path. Omit it to forward only the workload ports.

For a box with no public IP, `mode: iap` runs the same SSH over GCP
Identity-Aware Proxy (`gcloud compute ssh <instance> --tunnel-through-iap`):

```yaml
remote:
  tunnel:
    mode: iap
    instance: my-box
    zone: us-central1-a       # optional; falls back to gcloud's active zone
    project: my-project       # optional; falls back to gcloud's active project
    remote_socket: /home/user/.lazybox/run/daemon.sock
    ports: [3000, 8082]
```

### Reconnect is automatic

A dropped socket — laptop sleep, wifi change, the SSH tunnel resetting — no
longer kills the session. The `--connect` client re-dials the socket on its
own with capped backoff, re-`Subscribe`s, and the daemon replays a resync
snapshot (workspaces plus each terminal's ring buffer), so the screen
reconstructs without a manual restart. While it's re-dialing, a
`⟳ daemon connection lost — reconnecting…` banner shows so an extended outage
isn't a silent freeze; it clears the moment the link is back. Re-establish the
`ssh -L` forward (or use an autossh/`ServerAliveInterval` keepalive) and the
client reattaches as soon as the socket is back — and with `remote.tunnel`
above, lazybox re-establishes the forward itself, so the banner clears on its
own without you touching the tunnel.

A flapping endpoint (connects, then drops immediately) is backed off
progressively instead of hammered, and if the box comes back on an
incompatible lazybox build the client stops retrying and shows the usual
disconnect banner naming the build mismatch.

## The desktop app

The desktop app speaks HTTP/JSON to the daemon's API gateway rather than the
TUI's Unix socket, so it takes a different door to the same box. By default it
spawns its own in-process daemon and a loopback gateway; point it at an
existing gateway instead and it skips the local daemon entirely.

On the box, run the standalone gateway (not `server start`):

```sh
LAZYBOX_API_TOKEN=secret lazybox server api   # binds 127.0.0.1:8787
```

On your laptop, forward that loopback port and tell the desktop where to
reach it — either in `~/.lazybox/config.yaml`:

```yaml
desktop:
  remote:
    url: http://127.0.0.1:8787   # the SSH-forwarded loopback port
    token: secret                # the box's LAZYBOX_API_TOKEN
```

or through the environment (these win over config):

```sh
ssh -L 127.0.0.1:8787:127.0.0.1:8787 user@box
LAZYBOX_DESKTOP_GATEWAY_URL=http://127.0.0.1:8787 \
LAZYBOX_DESKTOP_GATEWAY_TOKEN=secret \
  lazybox-desktop
```

The gateway is loopback-only and unencrypted by design (it refuses any
non-loopback bind), so the `url` always points at a forwarded `127.0.0.1`
port, never a routable host — SSH is the trust boundary, exactly as for the
TUI socket. Leave `token` empty only if the box runs the gateway with
`--insecure-no-auth`.

The agents you can spawn and the repositories the project picker offers come
from the **box** — the desktop reads them from the gateway (`/v1/info`) on
attach, not from your laptop's config — so, like the TUI, a remote desktop
offers exactly what the box is set up to run.

The desktop's `/v1/events` and `/v1/terminal` streams re-dial on a dropped
link (laptop sleep, wifi change, tunnel reset). Each re-dial opens a fresh
gateway connection that re-`Subscribe`s and replays the authoritative
per-terminal ring buffer in its opening snapshot, so the inbox and terminals
reconstruct after a remote hop the same way the socket path resyncs. The
initial attach also tolerates a tunnel that isn't up yet: the handshake
retries for a few seconds before giving up, so launching the desktop a beat
before the `ssh -L` forward is ready still connects.

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

For a **sandbox** box (`lazybox sandbox …`, #977) this is handled for you:
`ensure` provisions a box that builds its daemon from the client's own commit,
so the first `connect` matches by construction. When you later rebuild the
client at a new commit that changes the wire format, re-sync the box with:

```sh
lazybox sandbox rebuild        # rebuild the box daemon at the client's commit
lazybox sandbox rebuild --sha <commit>   # or pin a specific commit
```

The TUI's `r`-spawn surfaces the same mismatch as an actionable notice
("box daemon is built from a different commit — run `lazybox sandbox rebuild`")
rather than a silent drop.

**Upgrade note (#977):** provisioning now runs the daemon as a dedicated
`lazybox` user, so `connect`/`r`-spawn default the SSH login to `lazybox`
whenever `install_lazybox` is on (the default). If you manage a box whose
daemon runs as a different account, set `sandbox.user` (or `--user`)
explicitly, or set `install_lazybox: false` to keep gcloud's default user.

[scoping]: remote-daemon-scoping.md
[systemd]: ../contrib/systemd/README.md
[lifecycle]: ../contrib/box-lifecycle/README.md
