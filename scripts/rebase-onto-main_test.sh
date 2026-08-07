#!/usr/bin/env bash
#
# Regression test for scripts/rebase-onto-main.sh.
#
# Runs the real script against throwaway repos, stubbing `make desktop-contract`
# with a trivial Makefile target so no Rust/zig toolchain is needed — the loop's
# control flow is what's under test, not the generator. Every invocation is
# wrapped in `timeout` so a regression to the unbounded resolve loop (the guard
# added for the "rebase stopped with no unmerged files" case) fails here instead
# of hanging.
#
# Run directly: `bash scripts/rebase-onto-main_test.sh`.

set -euo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/scripts/rebase-onto-main.sh"
GEN='apps/desktop/src/generated'
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# `timeout` guards against a regression to an unbounded resolve loop. It's not
# on every macOS box; fall back to running unguarded rather than skipping.
if command -v timeout >/dev/null 2>&1; then TIMEOUT=(timeout 60)
elif command -v gtimeout >/dev/null 2>&1; then TIMEOUT=(gtimeout 60)
else TIMEOUT=(); echo "note: no timeout(1) — anti-hang guard disabled" >&2; fi

# Build an origin (bare) + clone whose feature branch, rebased onto origin/main,
# conflicts exactly as described by the caller. Populates globals ORIGIN, CLONE.
setup_repo() {
	local root="$1"
	ORIGIN="$root/origin.git"
	CLONE="$root/clone"
	rm -rf "$ORIGIN" "$CLONE"
	git init -q --bare "$ORIGIN"
	git clone -q "$ORIGIN" "$CLONE" 2>/dev/null   # empty-repo warning is expected
	git -C "$CLONE" config user.email t@t.com
	git -C "$CLONE" config user.name t
	git -C "$CLONE" checkout -q -B main   # deterministic branch name (unborn)
	# A stub Makefile: `make desktop-contract` writes the "regenerated" contract.
	# It stands in for the real (merged-tree) generator; the script only cares
	# that it produces marker-free content and exits 0. printf keeps the literal
	# tab a recipe line needs (a `<<-` heredoc would strip it).
	{
		printf 'desktop-contract:\n'
		printf '\t@printf "GEN merged\\n" > %s/contract.txt\n' "$GEN"
	} > "$CLONE/Makefile"
	mkdir -p "$CLONE/$GEN"
	printf 'GEN base\n' > "$CLONE/$GEN/contract.txt"
	printf 'src base\n' > "$CLONE/src.txt"
	git -C "$CLONE" add -A
	git -C "$CLONE" commit -qm base
	git -C "$CLONE" push -q origin main
}

run_script() { ( cd "$CLONE" && ${TIMEOUT[@]+"${TIMEOUT[@]}"} bash "$SCRIPT" ); }

# ── Case 1: a contract-only conflict is auto-resolved and the script finishes ──
t1="$WORK/t1"; mkdir -p "$t1"; setup_repo "$t1"
git -C "$CLONE" checkout -q -b feature
printf 'GEN feat\n' > "$CLONE/$GEN/contract.txt"   # feature side
git -C "$CLONE" commit -qam feat
git -C "$CLONE" checkout -q main
printf 'GEN main\n' > "$CLONE/$GEN/contract.txt"    # origin/main side → conflicts
git -C "$CLONE" commit -qam main
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature

if ! run_script >/dev/null 2>&1; then
	fail "case 1: script exited non-zero on a contract-only conflict"
fi
[ -d "$(git -C "$CLONE" rev-parse --git-path rebase-merge)" ] && \
	fail "case 1: rebase left in progress"
got="$(cat "$CLONE/$GEN/contract.txt")"
[ "$got" = "GEN merged" ] || fail "case 1: contract not regenerated (got '$got')"
echo "PASS case 1: contract-only conflict auto-resolved, rebase completed"

# ── Case 2: a conflict outside the contract dir stops with a non-zero exit ─────
t2="$WORK/t2"; mkdir -p "$t2"; setup_repo "$t2"
git -C "$CLONE" checkout -q -b feature
printf 'src feat\n' > "$CLONE/src.txt"              # conflict on a NON-generated file
git -C "$CLONE" commit -qam feat
git -C "$CLONE" checkout -q main
printf 'src main\n' > "$CLONE/src.txt"
git -C "$CLONE" commit -qam main
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature

if run_script >/dev/null 2>&1; then
	fail "case 2: script should exit non-zero on a conflict outside the contract"
fi
git -C "$CLONE" rebase --abort >/dev/null 2>&1 || true
echo "PASS case 2: non-contract conflict bails with non-zero exit"

echo "OK: rebase-onto-main.sh regression tests passed"
