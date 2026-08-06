#!/usr/bin/env bash
#
# Stop this GCE box when it has been idle for LAZYBOX_IDLE_MINUTES.
#
# "Idle" means no one is connected and no agent is working:
#   - no established inbound SSH connection (the IAP/`ssh -L` tunnel a client
#     holds open shows up as an ESTABLISHED socket on the SSH port), and
#   - no agent CLI (claude/codex/…) burning CPU above LAZYBOX_IDLE_AGENT_CPU,
#     so a detached agent still mid-task keeps the box alive until it settles.
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
AGENT_CPU="${LAZYBOX_IDLE_AGENT_CPU:-5}"
AGENT_PROCS="${LAZYBOX_IDLE_AGENT_PROCS:-claude codex cursor-agent aider}"
MARKER="${LAZYBOX_IDLE_MARKER:-/run/lazybox/idle-since}"

log() { echo "lazybox-idle-stop: $*" >&2; }

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

# True while any watched agent process is above the CPU floor. `ps` reports the
# lifetime-average %CPU, which stays high for a process that is actually
# working and decays for one parked at a prompt.
agent_busy() {
  local name pids pid cpu
  for name in $AGENT_PROCS; do
    pids="$(pgrep -f "$name" 2>/dev/null || true)"
    for pid in $pids; do
      cpu="$(ps -o %cpu= -p "$pid" 2>/dev/null | tr -d ' ')"
      [ -n "$cpu" ] || continue
      awk -v c="$cpu" -v f="$AGENT_CPU" 'BEGIN { exit !(c + 0 >= f + 0) }' && return 0
    done
  done
  return 1
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
    gcloud compute instances stop "$name" --zone "$zone" --quiet
  else
    log "gcloud/metadata unavailable — guest shutdown"
    shutdown -h now
  fi
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
mkdir -p "$(dirname "$MARKER")"
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
