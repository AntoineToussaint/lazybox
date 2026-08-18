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
# is touched. Two overlapping input sets must BOTH be covered here, and
# neither is enforced against this list — keep them in sync by hand:
#
#   * the protocol-fingerprint inputs hashed by `crates/ipc/build.rs`
#     (crates/ipc/src, crates/core/src, AND Cargo.lock). That fingerprint is
#     embedded in the desktop compatibility fixture, so a Cargo.lock bump
#     alone (e.g. `cargo update`) moves the fixture with no source edit.
#   * the ts-rs DTO sources the generators export — the same crates plus the
#     tui-core types and the desktop DTOs in crates/server/src/api_gateway.rs.
#
# Miss an input and the guard silently skips a real drift, which is exactly
# the failure this hook exists to prevent.
CONTRACT_SRC_PATHS=(
	'crates/ipc/src/'
	'crates/core/src/'
	'crates/tui-core/src/'
	'crates/server/src/api_gateway.rs'
	'Cargo.lock'
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
	# ACMRD, not ACM: deleting a DTO source (D) or renaming one (R, when the
	# user has `diff.renames` on — the default) also moves a fixture, so the
	# guard must fire on those too, not just add/copy/modify.
	staged="$(git diff --cached --name-only --diff-filter=ACMRD -- "${CONTRACT_SRC_PATHS[@]}")"
	if [ -z "$staged" ]; then
		exit 0
	fi
	echo "regen-contracts: DTO sources staged, regenerating contracts:"
	while IFS= read -r path; do
		echo "  $path"
	done <<< "$staged"
fi

# `make contracts` regenerates from the WORKING TREE, and the `git add`
# below stages the output files whole. So a DTO source with both staged and
# unstaged hunks (a partial `git add -p`) bakes its unstaged hunks into the
# committed fixture too. In the pre-commit hook this is already moot for the
# .rs sources: step 1's fmt pass whole-file-restages every staged .rs before
# step 4 runs, so working tree == index for them by the time we get here.
# Stage whole files (or `git commit --no-verify`) if that distinction
# matters — the same caveat the fmt re-stage carries.
make contracts

if [ -n "$guarded" ]; then
	git add -- "${CONTRACT_OUTPUTS[@]}"
	echo "regen-contracts: re-staged regenerated contracts"
fi
