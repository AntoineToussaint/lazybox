#!/usr/bin/env bash
#
# Clean Rust build artifacts in the current worktree.
#
# Usage: run from anywhere inside the worktree; this script walks
# up to the git toplevel, finds the Cargo workspace (root or
# nested under crates/), prints before/after disk usage, then
# `cargo clean`.
#
# Mounted into lazybox worktrees via:
#   repos.tensorzero/nanogateway.scripts:
#     - name: cleanup
#       source: ~/Development/scripts/rust-cleanup.sh
# → available as `./_lazybox/scripts/cleanup` inside the worktree.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# Lazybox's nanogateway-style repos have Cargo.toml under crates/.
# Plain Rust repos have it at the root. Support both.
if [[ -f Cargo.toml ]]; then
    workspace_dir="."
elif [[ -f crates/Cargo.toml ]]; then
    workspace_dir="crates"
else
    echo "rust-cleanup: no Cargo.toml at repo root or crates/; aborting" >&2
    exit 1
fi

cd "$workspace_dir"

human_size() {
    if [[ -d target ]]; then
        du -sh target 2>/dev/null | cut -f1
    else
        echo "(none)"
    fi
}

before=$(human_size)
echo "rust-cleanup: $(pwd)/target/ = $before"
cargo clean
echo "rust-cleanup: cleaned. $(pwd)/target/ = $(human_size)"
