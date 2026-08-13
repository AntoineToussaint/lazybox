#!/usr/bin/env bash
set -euo pipefail

: "${E2B_API_KEY:?export E2B_API_KEY before running this probe}"

LAZYBOX_BIN="${LAZYBOX_BIN:-lazybox}"
E2B_TEMPLATE="${E2B_TEMPLATE:-lazybox-e2b}"
WAIT_SECONDS="${WAIT_SECONDS:-300}"
CYCLES="${CYCLES:-1}"
SESSION="lazybox-e2b-spike"
MARKER="lazybox-e2b-scrollback-marker"

if [ "$WAIT_SECONDS" -lt 300 ]; then
  echo "WAIT_SECONDS must be at least 300 for the persistence probe" >&2
  exit 2
fi
if ! [[ "$CYCLES" =~ ^[1-9][0-9]*$ ]]; then
  echo "CYCLES must be a positive integer" >&2
  exit 2
fi

for command in curl python3 ssh websocat; do
  command -v "$command" >/dev/null || {
    echo "missing required command: $command" >&2
    exit 2
  }
done

now_ms() {
  python3 -c 'import time; print(round(time.monotonic() * 1000))'
}

ensure_log="$(mktemp)"
trap 'rm -f "$ensure_log"' EXIT
"$LAZYBOX_BIN" sandbox ensure --provider e2b --template "$E2B_TEMPLATE" | tee "$ensure_log"
sandbox_id="$(sed -n 's/^Box ready: \([^ ]*\).*/\1/p' "$ensure_log" | tail -1)"
if [ -z "$sandbox_id" ]; then
  echo "could not read sandbox id from ensure output" >&2
  exit 1
fi

ssh_args=(
  -o BatchMode=yes
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
  -o ConnectTimeout=1
  -o "ProxyCommand=websocat --binary -B 65536 - wss://8081-%h.e2b.app"
  "user@$sandbox_id"
)

remote() {
  ssh "${ssh_args[@]}" "$@"
}

verify_remote_state() {
  remote "/usr/local/bin/lazybox server status | grep -q '^running' \
    && test -S ~/.lazybox/run/daemon.sock \
    && tmux has-session -t '$SESSION' \
    && tmux capture-pane -p -t '$SESSION' -S - | grep -Fqx '$MARKER-1' \
    && pane_pid=\$(tmux display-message -p -t '$SESSION' '#{pane_pid}') \
    && ps -p \"\$pane_pid\" -o args= | grep -q '[c]laude'"
}

memory_mb="$({
  curl -fsS -H "X-API-Key: $E2B_API_KEY" \
    "https://api.e2b.app/sandboxes/$sandbox_id"
} | python3 -c 'import json,sys; print(json.load(sys.stdin)["memoryMB"])')"

remote "tmux kill-session -t '$SESSION' 2>/dev/null || true; \
  tmux new-session -d -s '$SESSION' \
  \"bash -lc 'for i in \\$(seq 1 200); do echo $MARKER-\\$i; done; exec claude'\""
sleep 5
verify_remote_state

for cycle in $(seq 1 "$CYCLES"); do
  pause_started="$(now_ms)"
  "$LAZYBOX_BIN" sandbox sleep --provider e2b --template "$E2B_TEMPLATE"
  pause_finished="$(now_ms)"

  sleep "$WAIT_SECONDS"

  wake_started="$(now_ms)"
  "$LAZYBOX_BIN" sandbox wake --provider e2b --template "$E2B_TEMPLATE"
  resume_deadline=$((wake_started + 5000))
  while ! remote true 2>/dev/null; do
    if [ "$(now_ms)" -ge "$resume_deadline" ]; then
      echo "sandbox did not become reachable within the 5s acceptance bound" >&2
      exit 1
    fi
    sleep 0.1
  done
  wake_finished="$(now_ms)"

  pause_api_ms=$((pause_finished - pause_started))
  held_ms=$((wake_started - pause_finished))
  resume_ms=$((wake_finished - wake_started))
  printf 'cycle=%s memory_mb=%s pause_api_ms=%s paused_ms=%s perceived_resume_ms=%s\n' \
    "$cycle" "$memory_mb" "$pause_api_ms" "$held_ms" "$resume_ms"

  if [ "$resume_ms" -ge 5000 ]; then
    echo "resume exceeded the 5s acceptance bound" >&2
    exit 1
  fi
  verify_remote_state
done

echo "PASS: tmux session, scrollback, and Claude process survived $CYCLES pause/resume cycle(s)"
