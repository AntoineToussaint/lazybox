#!/usr/bin/env bash
#
# Stop this GCE box when it has been idle for LAZYBOX_IDLE_MINUTES.
#
# "Idle" means no one is connected and no agent is working:
#   - no established inbound SSH connection (the IAP/`ssh -L` tunnel a client
#     holds open shows up as an ESTABLISHED socket on the SSH port), and
#   - no watched agent CLI (claude/codex/…) that has consumed CPU since the
#     previous tick — so a detached agent still mid-task, even one mostly
#     blocked on I/O between short bursts, keeps the box alive until it settles.
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

# True while any watched agent has consumed CPU since the previous tick. We
# diff each process's cumulative CPU time against a snapshot from the last tick
# rather than reading an instantaneous or lifetime-average %CPU: a light-but-
# active agent (orchestrating `gh`, waiting on an API between short bursts)
# reliably accrues CPU across a 5-minute window, so it is not mistaken for idle
# and reaped mid-task — only a process that does no work for the whole window
# stops counting. A newly-seen process counts as active for its first tick.
agent_busy() {
  local name pid cur prev tmp active=1
  tmp="${SNAP}.tmp"
  : >"$tmp"
  for name in $AGENT_PROCS; do
    for pid in $(pgrep -f "$name" 2>/dev/null || true); do
      cur="$(cputime_secs "$pid")"
      [ -n "$cur" ] || continue
      printf '%s %s\n' "$pid" "$cur" >>"$tmp"
      prev="$(awk -v p="$pid" '$1 == p { print $2; exit }' "$SNAP" 2>/dev/null || true)"
      if [ -z "$prev" ] || [ "$(( cur - prev ))" -ge "$AGENT_CPU_SECS" ]; then
        active=0
      fi
    done
  done
  mv -f "$tmp" "$SNAP"
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
