#!/usr/bin/env bash
# Dev-mode launcher for lazybox. Prepends the pinned zig (see
# `.zig-version`) to PATH — libghostty-vt's build.zig requires that exact
# minor — and forwards extra args to the binary (e.g. `./run.sh --fresh`).
#
# zig lives in a HOST-LEVEL cache (default ~/.cache/lazybox/zig, override
# with LAZYBOX_ZIG_CACHE) shared by every clone and worktree, so a fresh
# worktree runs immediately without re-bootstrapping. Keep the default
# in lockstep with scripts/bootstrap.sh and the Makefile's ZIG_CACHE.
#
# First-time: `make setup` to fetch zig.
# Day-to-day: `./run.sh [args…]` or `make run`.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "${ROOT}"

case "$(uname -s)" in
  Darwin) os=macos ;;
  Linux)  os=linux ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  arm64|aarch64) arch=aarch64 ;;
  x86_64)        arch=x86_64 ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

# Read the pin from `.zig-version` — the same single source of truth the
# Makefile and scripts/bootstrap.sh use. Hardcoding it here meant a version
# bump left this launcher pointing at a cache directory that `make setup`
# no longer populates, silently falling through to system zig.
slug="${arch}-${os}-$(cat "${ROOT}/.zig-version")"
# Resolve pinned zig from either a per-worktree local install or the
# shared host-level cache. Local wins when present (lets a worktree pin
# its own zig); otherwise fall back to the cache `make setup` populates
# (default ~/.cache/lazybox/zig, override with LAZYBOX_ZIG_CACHE) so a fresh
# worktree runs without re-bootstrapping.
LOCAL_DIR="${ROOT}/vendor/zig/${slug}"
CACHE_DIR="${LAZYBOX_ZIG_CACHE:-${HOME}/.cache/lazybox/zig}/${slug}"
if [ -x "${LOCAL_DIR}/zig" ]; then
  ZIG_DIR="${LOCAL_DIR}"
elif [ -x "${CACHE_DIR}/zig" ]; then
  ZIG_DIR="${CACHE_DIR}"
else
  echo "pinned zig missing — run \`make setup\` first." >&2
  echo "  looked in ${LOCAL_DIR} and ${CACHE_DIR}" >&2
  exit 1
fi

PATH="${ZIG_DIR}:${PATH}" cargo run -p lazybox-tui -- "$@"
