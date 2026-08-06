#!/usr/bin/env bash
#
# Start-on-connect for a lazybox GCE box.
#
# Run from your laptop. If the box was stopped by the idle timer this starts it,
# waits for SSH to come up, then opens one IAP-tunnelled SSH connection that
# forwards the daemon socket plus the obin workload ports — so a stopped box
# costs nothing yet reconnecting is a single command. Kept as a shell helper for
# a plain `ssh`/`gcloud` setup; the in-lazybox tunnel supervisor is #889.
#
# Config comes from the environment (or a sourced env file):
#   LAZYBOX_BOX_PROJECT   gcloud project           (default: gcloud config)
#   LAZYBOX_BOX_ZONE      instance zone            (default: gcloud config)
#   LAZYBOX_BOX_INSTANCE  instance name            (required)
#   LAZYBOX_BOX_SOCK      remote daemon socket     (default: ~/.lazybox/run/daemon.sock)
#   LAZYBOX_LOCAL_SOCK    local socket to bind     (default: /tmp/lazybox.sock)
#   LAZYBOX_BOX_PORTS     extra TCP ports to -L    (default: "3000 8082 8787")
#   LAZYBOX_SSH_TIMEOUT   seconds to wait for SSH  (default: 120)

set -euo pipefail

INSTANCE="${LAZYBOX_BOX_INSTANCE:?set LAZYBOX_BOX_INSTANCE to the box name}"
PROJECT="${LAZYBOX_BOX_PROJECT:-$(gcloud config get-value project 2>/dev/null)}"
ZONE="${LAZYBOX_BOX_ZONE:-$(gcloud config get-value compute/zone 2>/dev/null)}"
# Absolute, or a path under the box's home. ssh -L does not expand ~/$HOME in a
# forward, so a relative default is resolved against the remote $HOME below.
REMOTE_SOCK="${LAZYBOX_BOX_SOCK:-.lazybox/run/daemon.sock}"
LOCAL_SOCK="${LAZYBOX_LOCAL_SOCK:-/tmp/lazybox.sock}"
PORTS="${LAZYBOX_BOX_PORTS:-3000 8082 8787}"
SSH_TIMEOUT="${LAZYBOX_SSH_TIMEOUT:-120}"

[ -n "$PROJECT" ] || { echo "connect.sh: no project (set LAZYBOX_BOX_PROJECT)" >&2; exit 1; }
[ -n "$ZONE" ] || { echo "connect.sh: no zone (set LAZYBOX_BOX_ZONE)" >&2; exit 1; }

gc() { gcloud compute "$@" --project "$PROJECT" --zone "$ZONE"; }

status="$(gc instances describe "$INSTANCE" --format='value(status)')"
if [ "$status" != "RUNNING" ]; then
  echo "connect.sh: box is $status — starting…" >&2
  gc instances start "$INSTANCE" >/dev/null
fi

echo "connect.sh: waiting for SSH (up to ${SSH_TIMEOUT}s)…" >&2
deadline=$(( $(date +%s) + SSH_TIMEOUT ))
# The readiness probe also echoes the remote $HOME, so we resolve a relative
# socket path to an absolute one in the same round-trip.
remote_home=""
until remote_home="$(gc ssh "$INSTANCE" --tunnel-through-iap \
        --command='printf %s "$HOME"' -- -o ConnectTimeout=10 2>/dev/null)"; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "connect.sh: SSH did not come up within ${SSH_TIMEOUT}s" >&2
    exit 1
  fi
  sleep 5
done
remote_home="${remote_home//$'\r'/}"

case "$REMOTE_SOCK" in
  /*) ;;
  *) REMOTE_SOCK="${remote_home%/}/${REMOTE_SOCK}" ;;
esac

# One IAP-tunnelled SSH connection carrying every forward. The socket forward is
# streamlocal (Unix socket <-> Unix socket); TCP ports bind to localhost so the
# obin web app stays a clean localhost:3000 (no WorkOS redirect change).
rm -f "$LOCAL_SOCK"
forwards=(-L "${LOCAL_SOCK}:${REMOTE_SOCK}")
for p in $PORTS; do
  forwards+=(-L "127.0.0.1:${p}:127.0.0.1:${p}")
done

echo "connect.sh: tunnel up — socket ${LOCAL_SOCK}, ports ${PORTS}" >&2
echo "connect.sh: then, in another shell: lazybox --connect ${LOCAL_SOCK}" >&2
exec gc ssh "$INSTANCE" --tunnel-through-iap \
  -- -N -o ServerAliveInterval=30 -o ServerAliveCountMax=3 \
     -o ExitOnForwardFailure=yes "${forwards[@]}"
