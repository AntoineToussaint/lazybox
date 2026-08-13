#!/usr/bin/env bash

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
lazybox_bin="${LAZYBOX_BIN:-${root}/target/release/lazybox}"
relay_image="${LAZYBOX_RELAY_IMAGE:-lazybox-relay:smoke}"
relay_addr="${1:-}"
work="$(mktemp -d)"
container=""
server_pid=""
serve_pid=""

cleanup() {
  if [ -n "$serve_pid" ]; then kill "$serve_pid" 2>/dev/null || true; fi
  if [ -n "$server_pid" ]; then kill "$server_pid" 2>/dev/null || true; fi
  if [ -n "$container" ]; then docker rm -f "$container" >/dev/null 2>&1 || true; fi
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

if [ ! -x "$lazybox_bin" ]; then
  echo "smoke-hosted-relay: build lazybox first, or set LAZYBOX_BIN" >&2
  exit 2
fi

if [ -z "$relay_addr" ]; then
  docker build -f "$root/crates/relay/Dockerfile" -t "$relay_image" "$root"
  docker_env=()
  for name in LAZYBOX_PLATFORM_URL LAZYBOX_PLATFORM_API_KEY; do
    if [ -n "${!name:-}" ]; then docker_env+=(--env "$name"); fi
  done
  container="$(docker run --detach --publish 127.0.0.1::9443 "${docker_env[@]}" "$relay_image")"
  relay_addr="$(docker port "$container" 9443/tcp | sed -n '1s/.*://p')"
  relay_addr="127.0.0.1:${relay_addr}"

  deadline=$(( $(date +%s) + 30 ))
  until [ "$(docker inspect --format '{{.State.Health.Status}}' "$container")" = healthy ]; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      docker logs "$container" >&2
      echo "smoke-hosted-relay: relay container did not become healthy" >&2
      exit 1
    fi
    sleep 1
  done
fi

box_home="$work/box"
client_home="$work/client"
mkdir -p "$box_home" "$client_home"
box_id="relay-smoke-$$"

LAZYBOX_HOME="$box_home" "$lazybox_bin" server start >"$work/server.log" 2>&1 &
server_pid=$!
deadline=$(( $(date +%s) + 20 ))
while [ ! -S "$box_home/run/daemon.sock" ]; do
  if ! kill -0 "$server_pid" 2>/dev/null; then
    cat "$work/server.log" >&2
    echo "smoke-hosted-relay: box daemon exited before creating its socket" >&2
    exit 1
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    cat "$work/server.log" >&2
    echo "smoke-hosted-relay: timed out waiting for the box daemon" >&2
    exit 1
  fi
  sleep 1
done

LAZYBOX_HOME="$box_home" "$lazybox_bin" serve \
  --relay "$relay_addr" --box-id "$box_id" \
  >"$work/serve.log" 2>&1 &
serve_pid=$!

deadline=$(( $(date +%s) + 30 ))
box_key=""
while [ -z "$box_key" ]; do
  box_key="$(sed -n 's/^box channel key \([0-9a-f]*\).*/\1/p' "$work/serve.log" | head -1)"
  if ! kill -0 "$serve_pid" 2>/dev/null; then
    cat "$work/serve.log" >&2
    echo "smoke-hosted-relay: box relay process exited" >&2
    exit 1
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    cat "$work/serve.log" >&2
    echo "smoke-hosted-relay: timed out waiting for the box channel key" >&2
    exit 1
  fi
  sleep 1
done

probe_log="$work/probe.log"
deadline=$(( $(date +%s) + 30 ))
until LAZYBOX_HOME="$client_home" "$lazybox_bin" --connect-relay "$box_id" \
  --relay "$relay_addr" --box-key "$box_key" --smoke >"$probe_log" 2>&1; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    cat "$work/serve.log" >&2
    cat "$probe_log" >&2
    echo "smoke-hosted-relay: encrypted daemon round trip failed" >&2
    exit 1
  fi
  sleep 1
done

cat "$probe_log"
