# box lifecycle — stop-on-idle + start-on-connect

An always-on per-user `e2-standard-8` burns money doing nothing. These
artifacts give a lazybox box a cheap lifecycle: it **stops itself when idle**
and **starts again the moment you connect**. A stopped GCE instance is
TERMINATED — you pay only for its disk, not the vCPU/RAM — so an untouched box
costs cents a day. Part of epic #885 (see [`docs/byo-remote-runbook.md`][runbook]).

- **`lazybox-idle-stop.sh`** + **`lazybox-idle-stop.service`** /
  **`.timer`** — run *on the box*. A short timer checks for idle and stops the
  instance once it has been idle long enough.
- **`connect.sh`** — run *on your laptop*. Starts the box if stopped, waits for
  SSH, then opens the IAP tunnel. The in-lazybox tunnel supervisor that
  replaces this is [#889].

## What counts as idle

The box is idle when **all** of these hold:

- **no active tunnel** — zero established connections on the SSH port. A client
  holding an `ssh -L` / IAP forward keeps an ESTABLISHED socket open, so a
  connected laptop always reads as active;
- **no live daemon** — the `lazybox server` refreshes a liveness file
  (`~/.lazybox/run/active`, override with `LAZYBOX_IDLE_ACTIVE_FILE`) while it
  holds a live PTY. A fresh mtime reads as active, so a client attached over a
  **relay** — which, unlike an IAP tunnel, does not present as inbound sshd —
  still keeps the box alive. On a bare box (no daemon) the file never appears
  and nothing changes; and
- **no agent working** — no watched agent CLI (`claude`, `codex`, … — see
  `LAZYBOX_IDLE_AGENT_PROCS`) whose process *tree* has consumed CPU since the
  previous tick. The CPU delta is summed over each agent's whole descendant
  tree, so an agent blocked on a long `cargo build` / `pytest` child (the agent
  itself near-idle) keeps the box alive until the work settles — not just an
  agent burning CPU directly. Measuring CPU *used between ticks* — not an
  instantaneous or lifetime-average reading — also keeps a light-but-active
  agent (orchestrating `gh`, waiting on an API between short bursts) from being
  mistaken for idle, so disconnecting mid-run never kills the work.

> **New built-in agents:** `crates/agents/tests/idle_stop_roster.rs` fails the
> build if `LAZYBOX_IDLE_AGENT_PROCS`'s default drops behind the agent
> registry, so a new built-in can't be silently un-watched. Operator-added
> `GenericCli` agents aren't in the registry — extend `LAZYBOX_IDLE_AGENT_PROCS`
> in `/etc/lazybox/idle-stop.env` to keep their sessions alive.

> **Root-run timer:** the systemd timer runs as root, so the script's default
> `~/.lazybox/run/active` resolves under `/root`, while the daemon writes under
> the box user's home. On such a box, set `LAZYBOX_IDLE_ACTIVE_FILE` in
> `/etc/lazybox/idle-stop.env` to the box user's path
> (e.g. `/home/alice/.lazybox/run/active`).

Idle is measured **across timer ticks**, not within one: the first idle tick
stamps a marker, a busy tick clears it, and once the marker is older than
`LAZYBOX_IDLE_MINUTES` (default 30) the box stops. The marker lives on tmpfs,
so a fresh start begins the clock from zero.

## Install on the box (golden image)

```sh
install -m0755 contrib/box-lifecycle/lazybox-idle-stop.sh /usr/local/bin/
install -m0644 contrib/box-lifecycle/lazybox-idle-stop.service /etc/systemd/system/
install -m0644 contrib/box-lifecycle/lazybox-idle-stop.timer   /etc/systemd/system/
install -d -m0755 /etc/lazybox
install -m0644 contrib/box-lifecycle/idle-stop.env.example /etc/lazybox/idle-stop.env  # optional, edit to taste
systemctl enable --now lazybox-idle-stop.timer
```

The self-stop uses `gcloud compute instances stop` via the instance's attached
service account (needs `compute.instances.stop` on itself); it discovers its own
name and zone from the metadata server. Where `gcloud` is absent — or the stop
call is rejected (e.g. the SA lacks that permission) — it falls back to a guest
`shutdown`, which GCE also records as a stop, so a misconfigured SA can't leave
the box running forever. Override the whole action with `LAZYBOX_IDLE_STOP_CMD`.

Tunables (all optional, all in `/etc/lazybox/idle-stop.env`) are listed in
[`idle-stop.env.example`](idle-stop.env.example).

## Connect from your laptop

```sh
export LAZYBOX_BOX_INSTANCE=alice-box
export LAZYBOX_BOX_PROJECT=internal-robin-dev
export LAZYBOX_BOX_ZONE=us-central1-a
contrib/box-lifecycle/connect.sh
# then, in another shell:
lazybox --connect /tmp/lazybox.sock
```

`connect.sh` starts the instance if it's stopped, polls SSH until it answers,
then holds one IAP-tunnelled SSH connection forwarding the daemon socket
(`/tmp/lazybox.sock` ← `~/.lazybox/run/daemon.sock`) and the workload ports
(`3000 8082 8787` by default, bindable via `LAZYBOX_BOX_PORTS`). TCP forwards
bind to `127.0.0.1`, so the obin web app stays a clean `localhost:3000` and
WorkOS needs no redirect-allowlist change.

## Budget guardrail — reaping long-idle boxes

Stop-on-idle already zeroes compute cost; a box left stopped for weeks still
holds its disk. GCE stamps `lastStopTimestamp` on every stop, so a reaper needs
no state of its own. Run this from an always-on place (Cloud Scheduler → a tiny
Cloud Function, or cron on a bastion) — **not** on the boxes, which are asleep:

```sh
# Delete boxes stopped more than $DAYS ago (label your boxes lazybox-box=true).
gcloud compute instances list \
  --filter='labels.lazybox-box=true AND status=TERMINATED' \
  --format='value(name,zone,lastStopTimestamp)' \
| while read -r name zone stopped; do
    age=$(( ( $(date +%s) - $(date -d "$stopped" +%s) ) / 86400 ))
    [ "$age" -ge "${DAYS:-14}" ] && gcloud compute instances delete "$name" --zone "$zone" --quiet
  done
```

Left as a documented recipe rather than a shipped unit: it runs off-box on a
schedule you own, and deleting instances is a policy call, not a default.

[runbook]: ../../docs/byo-remote-runbook.md
[#889]: https://github.com/AntoineToussaint/lazybox/issues/889
