#!/usr/bin/env bash
#
# rust-cleanup.sh — reclaim disk space in a Rust workspace worktree.
#
# Wire this into lazybox as a per-repo script so it lands in every worktree at
# <worktree>/_lazybox/scripts/cleanup. In ~/.lazybox/config.yaml:
#
#   repos:
#     owner/name:
#       scripts:
#         - name: cleanup
#           source: /absolute/path/to/examples/rust-cleanup.sh
#
# Run it from inside a worktree:  ./_lazybox/scripts/cleanup
#
set -euo pipefail

# Resolve the workspace root (the dir containing Cargo.toml), starting from the
# current directory and walking up. Falls back to the current directory.
find_root() {
  local dir
  dir="$(pwd)"
  while [[ "$dir" != "/" ]]; do
    if [[ -f "$dir/Cargo.toml" ]]; then
      printf '%s\n' "$dir"
      return 0
    fi
    dir="$(dirname "$dir")"
  done
  pwd
}

ROOT="$(find_root)"
cd "$ROOT"

echo "Cleaning Rust workspace at: $ROOT"

if command -v cargo >/dev/null 2>&1; then
  # Drop all build artifacts. Fast and complete; next build recompiles deps.
  echo "==> cargo clean"
  cargo clean
else
  echo "cargo not found on PATH; removing target/ directly"
  rm -rf target
fi

# Prune incremental compilation caches that cargo clean does not always remove.
if [[ -d target/debug/incremental ]]; then
  echo "==> pruning target/debug/incremental"
  rm -rf target/debug/incremental
fi

# Drop stray editor/test scratch files if present.
rm -f ./*.profraw 2>/dev/null || true

echo "Done."
