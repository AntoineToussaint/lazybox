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

# `git rev-parse --git-path` answers relative to the repo, so resolve it against
# $CLONE — otherwise the test looks for the rebase state dir in its own cwd and
# the assertion passes no matter what the script did.
rebase_in_progress() {
	[ -d "$CLONE/$(git -C "$CLONE" rev-parse --git-path rebase-merge)" ]
}

OUT=""
# Run and keep the output, for assertions about what the script *claims*.
run_script_capture() { OUT="$WORK/out.txt"; run_script > "$OUT" 2>&1; }

said() { grep -q "$1" "$OUT"; }

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
rebase_in_progress && fail "case 1: rebase left in progress"
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


# ── Case 3: re-running mid-rebase rejoins it and finishes the contract half ────
# The mixed conflict from the issue: a real code conflict alongside the
# generated one. The first run bails; after hand-resolving the code half the
# script must pick the stopped rebase back up rather than trip over its
# detached HEAD.
t3="$WORK/t3"; mkdir -p "$t3"; setup_repo "$t3"
git -C "$CLONE" checkout -q -b feature
printf 'src feat\n' > "$CLONE/src.txt"
printf 'GEN feat\n' > "$CLONE/$GEN/contract.txt"
git -C "$CLONE" commit -qam feat
git -C "$CLONE" checkout -q main
printf 'src main\n' > "$CLONE/src.txt"
printf 'GEN main\n' > "$CLONE/$GEN/contract.txt"
git -C "$CLONE" commit -qam main
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature

if run_script >/dev/null 2>&1; then
	fail "case 3: first run should bail on the non-contract conflict"
fi
rebase_in_progress || \
	fail "case 3: first run should leave the rebase stopped, not abort it"
printf 'src resolved\n' > "$CLONE/src.txt"        # hand-resolve the code half
git -C "$CLONE" add src.txt
if ! run_script_capture; then
	fail "case 3: re-run should rejoin the stopped rebase and finish it"
fi
said "auto-regenerated" || fail "case 3: should report the contract regeneration"
rebase_in_progress && fail "case 3: rebase left in progress"
[ "$(git -C "$CLONE" rev-parse --abbrev-ref HEAD)" = feature ] || \
	fail "case 3: not back on the feature branch"
got="$(cat "$CLONE/$GEN/contract.txt")"
[ "$got" = "GEN merged" ] || fail "case 3: contract not regenerated (got '$got')"
[ "$(cat "$CLONE/src.txt")" = "src resolved" ] || \
	fail "case 3: hand-resolved file was not preserved"
echo "PASS case 3: re-run mid-rebase resumes and auto-resolves the contract"

# ── Case 4: re-running with every conflict already staged just continues ───────
t4="$WORK/t4"; mkdir -p "$t4"; setup_repo "$t4"
git -C "$CLONE" checkout -q -b feature
printf 'src feat\n' > "$CLONE/src.txt"
git -C "$CLONE" commit -qam feat
git -C "$CLONE" checkout -q main
printf 'src main\n' > "$CLONE/src.txt"
git -C "$CLONE" commit -qam main
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature

if run_script >/dev/null 2>&1; then
	fail "case 4: first run should bail on the code conflict"
fi
rebase_in_progress || fail "case 4: first run should leave the rebase stopped"
printf 'src resolved\n' > "$CLONE/src.txt"
git -C "$CLONE" add src.txt                      # nothing unmerged left
if ! run_script_capture; then
	fail "case 4: re-run with everything staged should continue the rebase"
fi
said "auto-regenerated" && \
	fail "case 4: claimed a contract regeneration that never happened"
rebase_in_progress && fail "case 4: rebase left in progress"
[ "$(cat "$CLONE/src.txt")" = "src resolved" ] || \
	fail "case 4: hand-resolved file was not preserved"
echo "PASS case 4: re-run with no unmerged paths continues the rebase"

# ── Case 5: a detached HEAD with no rebase in progress still refuses ───────────
t5="$WORK/t5"; mkdir -p "$t5"; setup_repo "$t5"
git -C "$CLONE" checkout -q --detach
if run_script >/dev/null 2>&1; then
	fail "case 5: detached HEAD without a rebase should still exit non-zero"
fi
echo "PASS case 5: detached HEAD without a rebase still refuses"


# ── Case 6: a rebase this script did not start is refused, not hijacked ───────
# Before the ownership guard the resume path joined *any* stopped rebase, drove
# it to completion and reported "rebased onto origin/main" — for a branch that
# was not on origin/main at all.
t6="$WORK/t6"; mkdir -p "$t6"; setup_repo "$t6"
git -C "$CLONE" checkout -q -b sidebranch
printf 'src side\n' > "$CLONE/src.txt"; git -C "$CLONE" commit -qam side
git -C "$CLONE" checkout -q main; git -C "$CLONE" checkout -q -b feature
printf 'src feat\n' > "$CLONE/src.txt"; git -C "$CLONE" commit -qam feat
git -C "$CLONE" checkout -q main
printf 'MAIN\n' > "$CLONE/moved.txt"                # origin/main is somewhere else
git -C "$CLONE" add -A; git -C "$CLONE" commit -qm mainmoved
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature
git -C "$CLONE" rebase sidebranch >/dev/null 2>&1 || true   # someone else's rebase
rebase_in_progress || fail "case 6: setup did not stop a rebase onto sidebranch"
printf 'src resolved\n' > "$CLONE/src.txt"; git -C "$CLONE" add src.txt
before="$(git -C "$CLONE" rev-parse feature)"
if run_script_capture; then
	fail "case 6: script hijacked a rebase it did not start"
fi
said "did not start" || fail "case 6: should say whose rebase it is"
rebase_in_progress || fail "case 6: script disturbed the other rebase"
[ "$(git -C "$CLONE" rev-parse feature)" = "$before" ] || \
	fail "case 6: script moved the branch of someone else's rebase"
git -C "$CLONE" rebase --abort >/dev/null 2>&1 || true
echo "PASS case 6: a foreign rebase is refused and left untouched"

# ── Case 7: an interactive rebase onto origin/main is refused too ─────────────
# Same `onto`, so only the todo distinguishes it: driving it on would run the
# remaining `reword` under GIT_EDITOR=true and silently keep the old message.
cat > "$WORK/seq-reword.sh" <<'EOS'
#!/bin/sh
awk 'NR==2 && /^pick/ { sub(/^pick/, "reword") } { print }' "$1" > "$1.new"
mv "$1.new" "$1"
EOS
chmod +x "$WORK/seq-reword.sh"
t7="$WORK/t7"; mkdir -p "$t7"; setup_repo "$t7"
git -C "$CLONE" checkout -q -b feature
printf 'src feat\n' > "$CLONE/src.txt"; git -C "$CLONE" commit -qam feat1
printf 'more\n' > "$CLONE/extra.txt"; git -C "$CLONE" add -A
git -C "$CLONE" commit -qm feat2
git -C "$CLONE" checkout -q main
printf 'src main\n' > "$CLONE/src.txt"; git -C "$CLONE" commit -qam main
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature
GIT_SEQUENCE_EDITOR="$WORK/seq-reword.sh" \
	git -C "$CLONE" rebase -i origin/main >/dev/null 2>&1 || true
rebase_in_progress || fail "case 7: setup did not stop an interactive rebase"
printf 'src resolved\n' > "$CLONE/src.txt"; git -C "$CLONE" add src.txt
if run_script_capture; then
	fail "case 7: script drove an interactive rebase with a pending reword"
fi
said "plain picks" || fail "case 7: should say why the todo disqualifies it"
git -C "$CLONE" rebase --abort >/dev/null 2>&1 || true
echo "PASS case 7: an interactive rebase onto origin/main is refused"

# ── Case 8: origin/main moving mid-resolve does not lock us out of our own ────
# The rebase's `onto` is the origin/main of when it started. A fetch landing
# while you resolve must not turn the resume into "not my rebase".
t8="$WORK/t8"; mkdir -p "$t8"; setup_repo "$t8"
git -C "$CLONE" checkout -q -b feature
printf 'src feat\n' > "$CLONE/src.txt"
printf 'GEN feat\n' > "$CLONE/$GEN/contract.txt"
git -C "$CLONE" commit -qam feat
git -C "$CLONE" checkout -q main
printf 'src main\n' > "$CLONE/src.txt"
printf 'GEN main\n' > "$CLONE/$GEN/contract.txt"
git -C "$CLONE" commit -qam main
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature
run_script >/dev/null 2>&1 && fail "case 8: first run should bail"
# a sibling checkout pushes while we resolve, then our tracking ref catches up
git clone -q "$ORIGIN" "$t8/pusher" 2>/dev/null
git -C "$t8/pusher" config user.email t@t.com; git -C "$t8/pusher" config user.name t
printf 'later\n' > "$t8/pusher/later.txt"
git -C "$t8/pusher" add -A; git -C "$t8/pusher" commit -qm later
git -C "$t8/pusher" push -q origin HEAD:main
git -C "$CLONE" fetch -q origin
printf 'src resolved\n' > "$CLONE/src.txt"; git -C "$CLONE" add src.txt
if ! run_script_capture; then
	fail "case 8: a moved origin/main locked us out of our own rebase"
fi
said "has moved on" || fail "case 8: should warn the base is stale"
rebase_in_progress && fail "case 8: rebase left in progress"
echo "PASS case 8: resume survives a moved origin/main, and says the base is stale"

# ── Case 9: --continue is never retried against unchanged state ───────────────
# Fault-injected: a git shim fails every `rebase --continue` and counts them.
# One pass regenerates the contract and continues (attempt 1); the next pass
# finds nothing unmerged and must bail, not spend the resume allowance again.
t9="$WORK/t9"; mkdir -p "$t9"; setup_repo "$t9"
REAL_GIT="$(command -v git)"
SHIM="$t9/shim"; mkdir -p "$SHIM"; COUNT="$t9/continues"; : > "$COUNT"
cat > "$SHIM/git" <<EOS
#!/bin/sh
if [ "\$1" = rebase ] && [ "\$2" = --continue ]; then
	echo attempt >> "$COUNT"
	exit 1
fi
exec "$REAL_GIT" "\$@"
EOS
chmod +x "$SHIM/git"
git -C "$CLONE" checkout -q -b feature
printf 'GEN feat\n' > "$CLONE/$GEN/contract.txt"; git -C "$CLONE" commit -qam feat
git -C "$CLONE" checkout -q main
printf 'GEN main\n' > "$CLONE/$GEN/contract.txt"; git -C "$CLONE" commit -qam main
git -C "$CLONE" push -q origin main
git -C "$CLONE" checkout -q feature
git -C "$CLONE" rebase origin/main >/dev/null 2>&1 || true   # stop, then resume
rebase_in_progress || fail "case 9: setup did not stop on the contract conflict"
( cd "$CLONE" && PATH="$SHIM:$PATH" ${TIMEOUT[@]+"${TIMEOUT[@]}"} bash "$SCRIPT" ) \
	>/dev/null 2>&1 && fail "case 9: script should bail once --continue fails"
attempts="$(wc -l < "$COUNT" | tr -d ' ')"
[ "$attempts" = 1 ] || \
	fail "case 9: --continue attempted $attempts times, expected exactly 1"
git -C "$CLONE" rebase --abort >/dev/null 2>&1 || true
echo "PASS case 9: --continue is attempted once, never retried unchanged"

echo "OK: rebase-onto-main.sh regression tests passed"
