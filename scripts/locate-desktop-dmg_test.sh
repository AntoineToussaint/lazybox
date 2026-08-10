#!/usr/bin/env bash
#
# Regression test for scripts/locate-desktop-dmg.sh.
#
# The original workflow used `find … | head -n1`, which silently picked one
# dmg when zero or several were present. These cases pin the loud-failure
# behavior: exactly-one succeeds with the right sha, zero and many both bail
# non-zero.
#
# Run directly: `bash scripts/locate-desktop-dmg_test.sh`.

set -euo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/locate-desktop-dmg.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

sha_of() {
	if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}';
	else sha256sum "$1" | awk '{print $1}'; fi
}

# ── Case 1: exactly one dmg → success, correct copy + sha ─────────────────────
t1="$WORK/t1"; mkdir -p "$t1/dmg"
printf 'disk image bytes\n' > "$t1/dmg/lazybox_0.1.9_universal.dmg"
want="$(sha_of "$t1/dmg/lazybox_0.1.9_universal.dmg")"
out_dmg="$WORK/out1.dmg"; out_sha="$WORK/out1.sha"
if ! bash "$SCRIPT" "$t1" "$out_dmg" "$out_sha" >/dev/null 2>&1; then
	fail "case 1: script bailed on a single dmg"
fi
[ -f "$out_dmg" ] || fail "case 1: normalized dmg not written"
got="$(cat "$out_sha")"
[ "$got" = "$want" ] || fail "case 1: sha mismatch (got '$got', want '$want')"
cmp -s "$out_dmg" "$t1/dmg/lazybox_0.1.9_universal.dmg" || fail "case 1: copied bytes differ"
echo "PASS case 1: single dmg located, sha correct"

# ── Case 2: zero dmgs → non-zero exit, nothing written ────────────────────────
t2="$WORK/t2"; mkdir -p "$t2/dmg"
if bash "$SCRIPT" "$t2" "$WORK/out2.dmg" "$WORK/out2.sha" >/dev/null 2>&1; then
	fail "case 2: script should exit non-zero when no dmg is present"
fi
[ -f "$WORK/out2.dmg" ] && fail "case 2: wrote a dmg despite finding none"
echo "PASS case 2: zero dmgs bails non-zero"

# ── Case 3: multiple dmgs → non-zero exit (no silent head -n1 pick) ───────────
t3="$WORK/t3"; mkdir -p "$t3/dmg"
printf 'a\n' > "$t3/dmg/lazybox_0.1.9_universal.dmg"
printf 'b\n' > "$t3/dmg/lazybox_0.1.9_aarch64.dmg"
if bash "$SCRIPT" "$t3" "$WORK/out3.dmg" "$WORK/out3.sha" >/dev/null 2>&1; then
	fail "case 3: script should exit non-zero when several dmgs are present"
fi
[ -f "$WORK/out3.dmg" ] && fail "case 3: wrote a dmg despite an ambiguous match"
echo "PASS case 3: multiple dmgs bails non-zero"

echo "OK: locate-desktop-dmg.sh regression tests passed"
