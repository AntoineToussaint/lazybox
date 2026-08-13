#!/usr/bin/env bash
set -euo pipefail

BIN_DEST="${LAZYBOX_BIN_DEST:-/usr/local/bin/lazybox}"
ATTEMPTS="${LAZYBOX_DIRECT_SERVICE_ATTEMPTS:-100}"
LOG_PATH="${LAZYBOX_DIRECT_SERVICE_LOG:-$HOME/.lazybox/daemon.log}"

"$BIN_DEST" server stop >/dev/null 2>&1 || true
for _ in $(seq 1 "$ATTEMPTS"); do
  if "$BIN_DEST" server status | grep -q '^stopped$'; then
    break
  fi
  sleep 0.1
done
"$BIN_DEST" server status | grep -q '^stopped$'

install -d "$(dirname "$LOG_PATH")"
nohup "$BIN_DEST" server start >"$LOG_PATH" 2>&1 </dev/null &
daemon_pid=$!

for _ in $(seq 1 "$ATTEMPTS"); do
  if "$BIN_DEST" server status | grep -q '^running'; then
    exit 0
  fi
  if ! kill -0 "$daemon_pid" 2>/dev/null; then
    if wait "$daemon_pid"; then
      exit_code=1
    else
      exit_code=$?
    fi
    echo "lazybox direct daemon exited before becoming ready" >&2
    exit "$exit_code"
  fi
  sleep 0.1
done

kill "$daemon_pid" 2>/dev/null || true
wait "$daemon_pid" 2>/dev/null || true
echo "lazybox direct daemon did not become ready" >&2
exit 1
