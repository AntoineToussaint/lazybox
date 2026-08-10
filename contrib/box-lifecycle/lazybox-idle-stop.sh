#!/usr/bin/env bash
#
# Stop this GCE box when it has been idle for LAZYBOX_IDLE_MINUTES.
#
# "Idle" means no one is connected and no agent is working:
#   - no established inbound SSH connection (the IAP/`ssh -L` tunnel a client
#     holds open shows up as an ESTABLISHED socket on the SSH port),
#   - no fresh daemon liveness file — the `lazybox server` touches
#     `~/.lazybox/run/active` while it holds live PTYs, so a client attached
#     over a relay (which does not present as inbound sshd) still counts, and
#   - no watched agent CLI (claude/codex/…) whose process *tree* has consumed
#     CPU since the previous tick — the delta is summed over each agent's whole
#     descendant tree, so an agent blocked on a long `cargo build` / `pytest`
#     child (the agent itself near-idle) keeps the box alive until the work
#     settles, not just an agent burning CPU directly.
#
# Meant to run on a short timer (see lazybox-idle-stop.timer). Idle is measured
# across ticks via a marker file rather than within one tick: the first idle
# tick stamps "idle since now"; a busy tick clears the stamp; once the stamp is
# older than the threshold the box stops itself. On GCE a guest-initiated stop
# transitions the instance to TERMINATED — compute billing halts, only the disk
# lingers — and the marker lives on tmpfs, so a later start begins fresh.

set -euo pipefail

IDLE_MINUTES="${LAZYBOX_IDLE_MINUTES:-30}"
SSH_PORT="${LAZYBOX_IDLE_SSH_PORT:-22}"
AGENT_CPU_SECS="${LAZYBOX_IDLE_AGENT_CPU_SECS:-2}"
AGENT_PROCS="${LAZYBOX_IDLE_AGENT_PROCS:-claude codex cursor-agent aider}"
MARKER="${LAZYBOX_IDLE_MARKER:-/run/lazybox/idle-since}"
SNAP="${MARKER}.agent-cpu"
# The `lazybox server` keeps this file's mtime fresh while it holds a live PTY.
# On a root-run timer point it at the box user's home (the daemon writes under
# *its* $HOME) via /etc/lazybox/idle-stop.env. `${HOME:-/root}` guards the
# default expansion: systemd oneshots without a `User=` may run with $HOME
# unset, and under `set -u` a bare `$HOME` would abort the whole check every
# tick — the box would then never reap. root's home is the safe fallback.
ACTIVE_FILE="${LAZYBOX_IDLE_ACTIVE_FILE:-${HOME:-/root}/.lazybox/run/active}"
# Treat the daemon as active while the file is younger than this. Default is two
# timer ticks (2 × 5min) so a single missed touch never reaps a live daemon.
ACTIVE_MAX_AGE="${LAZYBOX_IDLE_ACTIVE_MAX_AGE:-600}"

log() { echo "lazybox-idle-stop: $*" >&2; }

# The marker and the per-process CPU snapshot both live here; create it before
# any check so agent_busy can write the snapshot on the very first tick.
mkdir -p "$(dirname "$MARKER")"

# Count established connections on the SSH port. `ss` is present on any modern
# systemd host; fall back to `netstat` where it is not.
ssh_connections() {
  if command -v ss >/dev/null 2>&1; then
    ss -Htn state established "( sport = :${SSH_PORT} )" | grep -c . || true
  elif command -v netstat >/dev/null 2>&1; then
    netstat -tn 2>/dev/null | grep -c "[:.]${SSH_PORT}[[:space:]].*ESTABLISHED" || true
  else
    # Can't tell — assume connected so we never stop a reachable box.
    echo 1
  fi
}

# File mtime as a Unix timestamp. GNU coreutils (`stat -c`) first, BSD/macOS
# (`stat -f`) second, so the behavioral tests can exercise this on a dev host.
# Empty if the file is gone or neither form works.
file_mtime() {
  stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null
}

# True while the lazybox daemon reports itself active: a liveness file it keeps
# fresh (mtime younger than ACTIVE_MAX_AGE) while it holds at least one live
# terminal (PTY). A missing file means no daemon is installed — not busy. An
# existing-but-unreadable mtime resolves to busy (fail-safe: never reap on doubt).
daemon_active() {
  [ -e "$ACTIVE_FILE" ] || return 1
  local mtime now
  mtime="$(file_mtime "$ACTIVE_FILE")"
  [ -n "$mtime" ] || return 0
  now="$(date +%s)"
  [ "$(( now - mtime ))" -lt "$ACTIVE_MAX_AGE" ]
}

# Every pid in the process tree rooted at $1 (the pid itself first, then its
# descendants depth-first). `pgrep -P` lists direct children; both it and the
# `ps --ppid` alternative ship in procps on the box's Ubuntu.
proc_tree() {
  printf '%s\n' "$1"
  local child
  for child in $(pgrep -P "$1" 2>/dev/null || true); do
    proc_tree "$child"
  done
}

# CPU-seconds a pid has used over its lifetime, from the portable, *cumulative*
# `ps -o time=` (unlike `%cpu`, which is a lifetime average). Handles the
# `[D-]HH:MM:SS` / `MM:SS` shapes ps emits. Empty if the pid is gone.
cputime_secs() {
  local t
  t="$(ps -o time= -p "$1" 2>/dev/null | tr -d ' ')"
  [ -n "$t" ] || return 0
  awk -v t="$t" 'BEGIN {
    d = 0; n = split(t, dp, "-"); if (n == 2) { d = dp[1]; t = dp[2] }
    m = split(t, p, ":"); s = 0
    for (i = 1; i <= m; i++) s = s * 60 + p[i]
    print int(d * 86400 + s)
  }'
}

# True while any watched agent's process *tree* has consumed CPU since the
# previous tick. We diff each pid's cumulative CPU time against a snapshot from
# the last tick rather than reading an instantaneous or lifetime-average %CPU: a
# light-but-active agent (orchestrating `gh`, waiting on an API between short
# bursts) reliably accrues CPU across a 5-minute window, so it is not mistaken
# for idle and reaped mid-task.
#
# Crucially the delta is summed over each agent's *whole descendant tree*, not
# the agent pid alone: `pgrep -f claude` matches the agent, but the `cargo
# build` / `pytest` child it spawned and is blocking on is where the CPU goes.
# Summing the tree keeps a box alive through a 40-minute build the agent kicked
# off before its laptop closed. A newly-seen pid in a watched tree counts as
# active for that tick (a just-spawned build is work, not idle).
agent_busy() {
  local name pid tpid cur prev tmp active=1 sum=0 delta
  tmp="${SNAP}.tmp"
  : >"$tmp"
  for name in $AGENT_PROCS; do
    for pid in $(pgrep -f "$name" 2>/dev/null || true); do
      for tpid in $(proc_tree "$pid"); do
        cur="$(cputime_secs "$tpid")"
        [ -n "$cur" ] || continue
        printf '%s %s\n' "$tpid" "$cur" >>"$tmp"
        prev="$(awk -v p="$tpid" '$1 == p { print $2; exit }' "$SNAP" 2>/dev/null || true)"
        if [ -z "$prev" ]; then
          active=0
        else
          delta=$(( cur - prev ))
          if [ "$delta" -gt 0 ]; then
            sum=$(( sum + delta ))
          fi
        fi
      done
    done
  done
  mv -f "$tmp" "$SNAP"
  if [ "$sum" -ge "$AGENT_CPU_SECS" ]; then
    active=0
  fi
  return "$active"
}

# Stop the instance. Prefer a self-`gcloud … stop` via the attached service
# account (clean TERMINATED transition); fall back to a guest shutdown, which
# GCE also records as a stop. Override the whole action with LAZYBOX_IDLE_STOP_CMD.
metadata() {
  curl -sf -H 'Metadata-Flavor: Google' \
    "http://metadata.google.internal/computeMetadata/v1/instance/$1"
}

stop_box() {
  if [ -n "${LAZYBOX_IDLE_STOP_CMD:-}" ]; then
    log "running LAZYBOX_IDLE_STOP_CMD"
    eval "$LAZYBOX_IDLE_STOP_CMD"
    return
  fi
  local name zone
  if command -v gcloud >/dev/null 2>&1 \
    && name="$(metadata name)" \
    && zone="$(metadata zone)"; then
    zone="${zone##*/}"
    log "gcloud stop $name in $zone"
    if gcloud compute instances stop "$name" --zone "$zone" --quiet; then
      return
    fi
    # A rejected stop (SA missing compute.instances.stop, transient API error)
    # must not leave the box running forever — a guest shutdown needs no cloud
    # permission and GCE still records it as a stop.
    log "gcloud stop failed — falling back to guest shutdown"
  else
    log "gcloud/metadata unavailable — guest shutdown"
  fi
  shutdown -h now
}

if [ "$(ssh_connections)" -gt 0 ]; then
  rm -f "$MARKER"
  exit 0
fi
if daemon_active; then
  rm -f "$MARKER"
  exit 0
fi
if agent_busy; then
  rm -f "$MARKER"
  exit 0
fi

now="$(date +%s)"
if [ ! -f "$MARKER" ]; then
  echo "$now" >"$MARKER"
  log "idle since $now"
  exit 0
fi

since="$(cat "$MARKER" 2>/dev/null || echo "$now")"
idle_secs=$(( now - since ))
if [ "$idle_secs" -ge $(( IDLE_MINUTES * 60 )) ]; then
  log "idle ${idle_secs}s >= ${IDLE_MINUTES}m — stopping"
  stop_box
else
  log "idle ${idle_secs}s (< ${IDLE_MINUTES}m)"
fi
