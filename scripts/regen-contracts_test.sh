#!/usr/bin/env bash
#
# Regression test for scripts/regen-contracts.sh.
#
# Runs the real script against throwaway repos, stubbing `make contracts`
# with a trivial Makefile target so no Rust/zig toolchain is needed — the
# --if-staged guard and the re-stage step are what's under test, not the
# generator itself. A sentinel file records whether `make` actually ran, so
# a regression that drops the git-diff guard (and rebuilds on every commit)
# fails here instead of quietly wasting ~11 min per commit.
#
# Run directly: `bash scripts/regen-contracts_test.sh`.

set -euo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/scripts/regen-contracts.sh"
GEN='apps/desktop/src/generated'
JSON='crates/server/src/api_client_contract.json'
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Build a repo whose `make contracts` stub rewrites both fixtures and drops
# a `make-ran` sentinel, so a test can assert whether the heavy build fired.
# Populates the global CLONE.
setup_repo() {
	local root="$1"
	CLONE="$root/clone"
	rm -rf "$CLONE"
	mkdir -p "$CLONE"
	git -C "$CLONE" init -q
	git -C "$CLONE" config user.email t@t.com
	git -C "$CLONE" config user.name t
	git -C "$CLONE" checkout -q -B main
	# printf keeps the literal recipe tab (a `<<-` heredoc would strip it).
	{
		printf 'contracts:\n'
		printf '\t@touch make-ran\n'
		printf '\t@printf "GEN regenerated\\n" > %s/DesktopCommand.ts\n' "$GEN"
		printf '\t@printf "{\\"regenerated\\":true}\\n" > %s\n' "$JSON"
	} > "$CLONE/Makefile"
	mkdir -p "$CLONE/$GEN" "$CLONE/crates/server/src"
	mkdir -p "$CLONE/crates/ipc/src" "$CLONE/crates/core/src"
	mkdir -p "$CLONE/crates/tui-core/src"
	printf 'GEN committed\n' > "$CLONE/$GEN/DesktopCommand.ts"
	printf '{"committed":true}\n' > "$CLONE/$JSON"
	printf 'src\n' > "$CLONE/crates/ipc/src/lib.rs"
	printf 'dto\n' > "$CLONE/crates/server/src/api_gateway.rs"
	printf 'lock\n' > "$CLONE/Cargo.lock"
	printf 'readme\n' > "$CLONE/README.md"
	git -C "$CLONE" add -A
	git -C "$CLONE" commit -qm base
}

run() { ( cd "$CLONE" && bash "$SCRIPT" "$@" ); }

# ── Case 1: --if-staged with a staged DTO source regenerates + re-stages ──
t1="$WORK/t1"; mkdir -p "$t1"; setup_repo "$t1"
printf 'changed dto\n' > "$CLONE/crates/ipc/src/lib.rs"
git -C "$CLONE" add crates/ipc/src/lib.rs
run --if-staged >/dev/null
[ -f "$CLONE/make-ran" ] || fail "case1: guard skipped regen despite a staged DTO source"
# The regenerated fixtures must be staged (present in the index), not just
# left dirty in the worktree, so they ride the same commit.
staged_gen="$(git -C "$CLONE" diff --cached --name-only -- "$GEN/DesktopCommand.ts")"
staged_json="$(git -C "$CLONE" diff --cached --name-only -- "$JSON")"
[ -n "$staged_gen" ] || fail "case1: regenerated desktop fixture was not re-staged"
[ -n "$staged_json" ] || fail "case1: regenerated web-control fixture was not re-staged"
grep -q "GEN regenerated" "$CLONE/$GEN/DesktopCommand.ts" \
	|| fail "case1: desktop fixture was not regenerated"

# ── Case 2: --if-staged with only a non-DTO change skips the build entirely ──
t2="$WORK/t2"; mkdir -p "$t2"; setup_repo "$t2"
printf 'changed readme\n' > "$CLONE/README.md"
git -C "$CLONE" add README.md
run --if-staged >/dev/null
[ -f "$CLONE/make-ran" ] && fail "case2: guard ran the heavy build for a non-DTO change"
grep -q "GEN committed" "$CLONE/$GEN/DesktopCommand.ts" \
	|| fail "case2: fixture changed despite no DTO edit"

# ── Case 3: unstaged DTO edit does not trigger regen (only the index counts) ──
t3="$WORK/t3"; mkdir -p "$t3"; setup_repo "$t3"
printf 'unstaged dto\n' > "$CLONE/crates/core/src/lib.rs"   # written, never `git add`ed
run --if-staged >/dev/null
[ -f "$CLONE/make-ran" ] && fail "case3: guard fired on an unstaged (not-added) DTO edit"

# ── Case 4: no --if-staged always regenerates, without touching the index ──
t4="$WORK/t4"; mkdir -p "$t4"; setup_repo "$t4"
run >/dev/null
[ -f "$CLONE/make-ran" ] || fail "case4: unconditional mode did not regenerate"
# Nothing was `git add`ed, so the index stays clean in unconditional mode.
[ -z "$(git -C "$CLONE" diff --cached --name-only)" ] \
	|| fail "case4: unconditional mode should not stage anything"

# ── Case 5: a staged Cargo.lock (a fingerprint input) triggers regen ──
# `cargo update` stages ONLY Cargo.lock, but Cargo.lock feeds
# lazybox_ipc::PROTOCOL_FINGERPRINT (crates/ipc/build.rs), which is baked
# into the desktop compatibility fixture — so a lock-only bump moves a
# generated contract with no source edit. The guard must fire on it.
t5="$WORK/t5"; mkdir -p "$t5"; setup_repo "$t5"
printf 'lock bumped\n' > "$CLONE/Cargo.lock"
git -C "$CLONE" add Cargo.lock
run --if-staged >/dev/null
[ -f "$CLONE/make-ran" ] \
	|| fail "case5: guard skipped regen despite a staged Cargo.lock (fingerprint input)"

# ── Case 6: a staged DTO deletion triggers regen (removing a wire type) ──
# Deleting a DTO source moves the contract too, so --diff-filter must
# include D — ACM alone would silently skip this.
t6="$WORK/t6"; mkdir -p "$t6"; setup_repo "$t6"
git -C "$CLONE" rm -q crates/ipc/src/lib.rs
run --if-staged >/dev/null
[ -f "$CLONE/make-ran" ] \
	|| fail "case6: guard skipped regen despite a staged DTO deletion"

echo "PASS: scripts/regen-contracts_test.sh"
