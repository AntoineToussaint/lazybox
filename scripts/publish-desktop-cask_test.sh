#!/usr/bin/env bash
#
# Regression test for scripts/publish-desktop-cask.sh.
#
# Runs the real script against throwaway bare "tap" repos. The headline case is
# the first publish: the original workflow's `git diff --quiet` on an untracked
# cask reported "no change" and skipped the push entirely, so a fresh tap never
# received the cask. Case 1 fails if that regresses. Later cases pin
# idempotency (no dup commit), real updates, and the rebase-on-race retry.
#
# Run directly: `bash scripts/publish-desktop-cask_test.sh`.

set -euo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/publish-desktop-cask.sh"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMPL="$ROOT/apps/desktop/packaging/lazybox-desktop.rb.tmpl"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# A bare repo with one initial commit (README) on the default branch, so the
# clone the script makes has a tracking upstream to push to.
make_tap() {
	local bare="$1" seed="$WORK/seed-$$-${RANDOM}"
	git init -q --bare "$bare"
	git clone -q "$bare" "$seed" 2>/dev/null
	git -C "$seed" config user.email t@t.com
	git -C "$seed" config user.name t
	git -C "$seed" checkout -q -B main
	printf '# tap\n' > "$seed/README.md"
	git -C "$seed" add -A
	git -C "$seed" commit -qm seed
	git -C "$seed" push -q origin main
	rm -rf "$seed"
}

# Read the committed cask back out of the bare repo (empty if absent).
cask_in_tap() {
	git -C "$1" show "main:Casks/lazybox-desktop.rb" 2>/dev/null || true
}
commits_in_tap() { git -C "$1" rev-list --count main 2>/dev/null || echo 0; }

# ── Case 1: FIRST publish — untracked cask must still be committed + pushed ────
bare1="$WORK/tap1.git"; make_tap "$bare1"
bash "$SCRIPT" "$TMPL" "0.1.9" "deadbeefsha" "$bare1" >/dev/null 2>&1 \
	|| fail "case 1: first publish exited non-zero"
body="$(cask_in_tap "$bare1")"
[ -n "$body" ] || fail "case 1: cask was NOT pushed on the first release (the original bug)"
printf '%s' "$body" | grep -q 'version "0.1.9"' || fail "case 1: version not substituted"
printf '%s' "$body" | grep -q 'sha256 "deadbeefsha"' || fail "case 1: sha not substituted"
printf '%s' "$body" | grep -q 'lazybox_#{version}_universal.dmg' \
	|| fail "case 1: Ruby #{version} interpolation was clobbered"
echo "PASS case 1: first publish commits + pushes a brand-new cask"

# ── Case 2: re-publish the SAME version+sha — no new commit ───────────────────
before="$(commits_in_tap "$bare1")"
bash "$SCRIPT" "$TMPL" "0.1.9" "deadbeefsha" "$bare1" >/dev/null 2>&1 \
	|| fail "case 2: idempotent re-publish exited non-zero"
after="$(commits_in_tap "$bare1")"
[ "$before" = "$after" ] || fail "case 2: an unchanged cask created a new commit ($before -> $after)"
echo "PASS case 2: unchanged re-publish is a no-op"

# ── Case 3: a new version publishes a fresh commit ────────────────────────────
bash "$SCRIPT" "$TMPL" "0.2.0" "cafef00d" "$bare1" >/dev/null 2>&1 \
	|| fail "case 3: version bump publish exited non-zero"
[ "$(commits_in_tap "$bare1")" -gt "$after" ] || fail "case 3: version bump did not commit"
cask_in_tap "$bare1" | grep -q 'version "0.2.0"' || fail "case 3: new version not published"
echo "PASS case 3: version bump publishes a new commit"

# ── Case 4: the tap advanced under us — first push is rejected, rebase + retry ─
# A pre-receive hook fires the race deterministically: on the first push it
# advances main to a pre-staged concurrent commit and rejects, so the script
# must `pull --rebase` and push again. The second push is accepted.
bare4="$WORK/tap4.git"; make_tap "$bare4"
side="$WORK/side"; git clone -q "$bare4" "$side"
git -C "$side" config user.email s@s.com; git -C "$side" config user.name s
printf 'unrelated\n' > "$side/OTHER.md"
git -C "$side" add -A; git -C "$side" commit -qm concurrent
concurrent_sha="$(git -C "$side" rev-parse HEAD)"
# Transfer the concurrent commit's objects into the bare repo without moving
# main (the hook will move it during the rejected push).
git -C "$side" push -q origin "HEAD:refs/hidden/concurrent"
cat > "$bare4/hooks/pre-receive" <<HOOK
#!/bin/sh
flag="\$GIT_DIR/.rejected_once"
if [ ! -e "\$flag" ]; then
	: > "\$flag"
	# git update-ref is blocked by receive's quarantine; write the loose ref
	# file directly. The object already exists (pushed to refs/hidden above).
	printf '%s\n' "${concurrent_sha}" > "\$GIT_DIR/refs/heads/main"
	echo "simulated concurrent advance" >&2
	exit 1
fi
exit 0
HOOK
chmod +x "$bare4/hooks/pre-receive"
bash "$SCRIPT" "$TMPL" "0.3.0" "beadfeed" "$bare4" >/dev/null 2>&1 \
	|| fail "case 4: publish did not recover from a rejected first push"
[ -e "$bare4/.rejected_once" ] || fail "case 4: the race hook never fired (test is not exercising retry)"
cask_in_tap "$bare4" | grep -q 'version "0.3.0"' || fail "case 4: cask missing after retry"
git -C "$bare4" show "main:OTHER.md" >/dev/null 2>&1 || fail "case 4: rebase dropped the concurrent commit"
echo "PASS case 4: rejected first push is rebased and retried, keeping both changes"

echo "OK: publish-desktop-cask.sh regression tests passed"
