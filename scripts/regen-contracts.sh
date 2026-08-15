#!/usr/bin/env bash
#
# Regenerate the generated wire contracts (the desktop TS fixtures and the
# web-control JSON fixture) from the Rust DTOs.
#
# Two modes:
#
#   (no args)    Regenerate unconditionally — the same work `make contracts`
#                does. Use it to refresh the fixtures by hand.
#
#   --if-staged  Run a cheap `git diff --cached` guard FIRST and regenerate
#                only when the staged changes touch a DTO source, then
#                re-stage the regenerated fixtures so they land in the same
#                commit. This is what the opt-in pre-commit hook runs, so a
#                DTO edit can't reach CI with stale fixtures. When nothing
#                relevant is staged it exits 0 immediately with no build —
#                that guard is what keeps the ~11-min contract build off the
#                overwhelming majority of commits.
#
# Run directly: `bash scripts/regen-contracts.sh [--if-staged]`.

set -euo pipefail

# DTO sources that feed the generators. A staged change under any of these
# can move a generated fixture, so --if-staged regenerates exactly when one
# is touched (mirrors the contract inputs the CI drift gates guard).
CONTRACT_SRC_PATHS=(
	'crates/ipc/src/'
	'crates/core/src/'
	'crates/tui-core/src/'
	'crates/server/src/api_gateway.rs'
)

# Fixtures the generators write, re-staged after a hook-triggered regen.
CONTRACT_OUTPUTS=(
	'apps/desktop/src/generated'
	'crates/server/src/api_client_contract.json'
)

cd "$(git rev-parse --show-toplevel)"

guarded=""
if [ "${1:-}" = "--if-staged" ]; then
	guarded=1
elif [ -n "${1:-}" ]; then
	echo "usage: $0 [--if-staged]" >&2
	exit 2
fi

if [ -n "$guarded" ]; then
	staged="$(git diff --cached --name-only --diff-filter=ACM -- "${CONTRACT_SRC_PATHS[@]}")"
	if [ -z "$staged" ]; then
		exit 0
	fi
	echo "regen-contracts: DTO sources staged, regenerating contracts:"
	while IFS= read -r path; do
		echo "  $path"
	done <<< "$staged"
fi

make contracts

if [ -n "$guarded" ]; then
	git add -- "${CONTRACT_OUTPUTS[@]}"
	echo "regen-contracts: re-staged regenerated contracts"
fi
