#!/usr/bin/env bash
# Fast refusal-path tests for scripts/cut-release.sh. No network is used.

set -euo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/cut-release.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }

expect_rejected() {
	local name="$1" expected="$2"
	shift 2
	local output
	if output="$("$@" 2>&1)"; then
		fail "$name: command unexpectedly succeeded"
	fi
	printf '%s\n' "$output" | grep -F "$expected" >/dev/null \
		|| fail "$name: expected '$expected', got: $output"
	echo "PASS $name"
}

expect_rejected "missing version" "version must be SemVer" bash "$SCRIPT"
expect_rejected "leading v" "version must be SemVer" bash "$SCRIPT" v0.1.15
expect_rejected "unknown option" "unknown option: --wat" bash "$SCRIPT" 0.1.15 --wat
expect_rejected "duplicate version" "version supplied more than once" bash "$SCRIPT" 0.1.15 0.1.16

help="$(bash "$SCRIPT" --help)" || fail "help exited non-zero"
printf '%s\n' "$help" | grep -F -- '--manual-checks-confirmed' >/dev/null \
	|| fail "help omits the manual-check attestation"
echo "PASS help documents the manual-check boundary"

grep -F 'target_directory="$(cargo metadata --locked --no-deps --format-version 1' "$SCRIPT" >/dev/null \
	|| fail "release artifact check does not resolve Cargo's target directory"
if grep -F './target/release/lazybox' "$SCRIPT" >/dev/null; then
	fail "release artifact check hardcodes a worktree-local target directory"
fi
echo "PASS release artifact follows Cargo target directory"
