#!/usr/bin/env bash
#
# Regression test for scripts/preflight-desktop-signing.sh.
#
# `identity` cases build throwaway .p12s with openssl and assert the exact
# strings extracted (and that a non-Developer-ID cert / wrong password are
# rejected — the footguns the script exists to catch). `check` cases run the
# real script against a stubbed `gh` on PATH, so no network or real repo is
# touched.
#
# Run directly: `bash scripts/preflight-desktop-signing_test.sh`.

set -euo pipefail

SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/preflight-desktop-signing.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

# Build a .p12 whose leaf cert has the given subject.
make_p12() {
	local out="$1" subject="$2" pass="$3"
	local key="$WORK/k-$$-${RANDOM}.pem" crt="$WORK/c-$$-${RANDOM}.pem"
	openssl req -x509 -newkey rsa:2048 -keyout "$key" -out "$crt" -days 2 -nodes \
		-subj "$subject" >/dev/null 2>&1 || fail "openssl req failed for '$subject'"
	openssl pkcs12 -export -inkey "$key" -in "$crt" -out "$out" -passout "pass:$pass" \
		>/dev/null 2>&1 || fail "openssl pkcs12 export failed"
}

# Build a Keychain-style .p12: cert bag under PBE-SHA1-RC2-40, which OpenSSL 3.x
# refuses to read without -legacy. `-legacy` only exists on OpenSSL 3.x; on
# LibreSSL / 1.x RC2-40 is the native default, so fall back without the flag.
make_legacy_p12() {
	local out="$1" subject="$2" pass="$3"
	local key="$WORK/lk-$$-${RANDOM}.pem" crt="$WORK/lc-$$-${RANDOM}.pem"
	openssl req -x509 -newkey rsa:2048 -keyout "$key" -out "$crt" -days 2 -nodes \
		-subj "$subject" >/dev/null 2>&1 || fail "openssl req failed (legacy)"
	openssl pkcs12 -export -legacy -certpbe PBE-SHA1-RC2-40 -keypbe PBE-SHA1-3DES -macalg SHA1 \
		-inkey "$key" -in "$crt" -out "$out" -passout "pass:$pass" >/dev/null 2>&1 \
	|| openssl pkcs12 -export -certpbe PBE-SHA1-RC2-40 -keypbe PBE-SHA1-3DES -macalg SHA1 \
		-inkey "$key" -in "$crt" -out "$out" -passout "pass:$pass" >/dev/null 2>&1 \
	|| fail "openssl pkcs12 legacy export failed"
}

# ── Case 1: a real Developer ID Application .p12 yields the exact strings ──────
devid="$WORK/devid.p12"
make_p12 "$devid" "/CN=Developer ID Application: Jane Roe (ABCDE12345)/OU=ABCDE12345/O=Jane Roe/C=US" secret
out="$(bash "$SCRIPT" identity "$devid" secret)" || fail "case 1: identity exited non-zero"
printf '%s\n' "$out" | grep -qx 'APPLE_SIGNING_IDENTITY=Developer ID Application: Jane Roe (ABCDE12345)' \
	|| fail "case 1: wrong signing identity: $out"
printf '%s\n' "$out" | grep -qx 'APPLE_TEAM_ID=ABCDE12345' \
	|| fail "case 1: wrong team id: $out"
echo "PASS case 1: Developer ID cert yields exact identity + team id"

# ── Case 1b: a Keychain-format (RC2-40) .p12 reads via the -legacy retry ──────
# The original single-attempt read failed on exactly this, macOS's real export.
legacy="$WORK/legacy.p12"
make_legacy_p12 "$legacy" "/CN=Developer ID Application: Jane Roe (ABCDE12345)/OU=ABCDE12345/O=Jane Roe/C=US" secret
out="$(bash "$SCRIPT" identity "$legacy" secret)" || fail "case 1b: legacy .p12 could not be read"
printf '%s\n' "$out" | grep -qx 'APPLE_TEAM_ID=ABCDE12345' \
	|| fail "case 1b: wrong team id from legacy .p12: $out"
echo "PASS case 1b: Keychain-format RC2-40 .p12 is read via the legacy retry"

# ── Case 2: the wrong export password is rejected ─────────────────────────────
if bash "$SCRIPT" identity "$devid" wrongpass >/dev/null 2>&1; then
	fail "case 2: a wrong .p12 password was accepted"
fi
echo "PASS case 2: wrong password is rejected"

# ── Case 3: a non-Developer-ID cert (e.g. Apple Development) is rejected ───────
appledev="$WORK/appledev.p12"
make_p12 "$appledev" "/CN=Apple Development: Jane Roe (ABCDE12345)/OU=ABCDE12345/O=Jane Roe/C=US" secret
if err="$(bash "$SCRIPT" identity "$appledev" secret 2>&1)"; then
	fail "case 3: a non-Developer-ID cert was accepted"
fi
printf '%s' "$err" | grep -q 'Developer ID Application' \
	|| fail "case 3: rejection message did not name the required cert type: $err"
echo "PASS case 3: non-Developer-ID cert is rejected with a clear message"

# ── check: a `gh` stub on PATH, parameterized by env ──────────────────────────
stubdir="$WORK/bin"; mkdir -p "$stubdir"
cat > "$stubdir/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
# Drop a leading `--repo X` so the stub sees the same subcommand either way.
if [ "${1:-}" = "--repo" ]; then shift 2; fi
case "$1 ${2:-}" in
	"secret list")
		for n in $STUB_SECRETS; do printf '%s\t2026-01-01T00:00:00Z\n' "$n"; done ;;
	"variable list")
		# TSV: NAME<tab>VALUE<tab>UPDATED. A decoy row proves the name filter.
		printf 'UNRELATED_VAR\tnope\t2026-01-01T00:00:00Z\n'
		[ -n "${STUB_ENABLED:-}" ] \
			&& printf 'DESKTOP_RELEASE_ENABLED\t%s\t2026-01-01T00:00:00Z\n' "$STUB_ENABLED"
		exit 0 ;;
	*) echo "unexpected gh args: $*" >&2; exit 2 ;;
esac
STUB
chmod +x "$stubdir/gh"

ALL_SIX="APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID"

# ── Case 4: fully provisioned → exit 0, reports ready ─────────────────────────
if out="$(PATH="$stubdir:$PATH" STUB_SECRETS="$ALL_SIX HOMEBREW_TAP_TOKEN" STUB_ENABLED=true \
		bash "$SCRIPT" check 2>&1)"; then :; else
	fail "case 4: check exited non-zero when fully provisioned: $out"
fi
printf '%s' "$out" | grep -q 'ready:' || fail "case 4: did not report ready: $out"
echo "PASS case 4: fully provisioned repo checks out ready"

# ── Case 5: missing secrets + variable → exit 1, names what is missing ────────
if out="$(PATH="$stubdir:$PATH" STUB_SECRETS="APPLE_CERTIFICATE HOMEBREW_TAP_TOKEN" STUB_ENABLED="" \
		bash "$SCRIPT" check 2>&1)"; then
	fail "case 5: check exited 0 despite missing setup"
fi
printf '%s' "$out" | grep -q 'APPLE_ID' || fail "case 5: missing secret not named: $out"
printf '%s' "$out" | grep -q 'DESKTOP_RELEASE_ENABLED' || fail "case 5: unset variable not named: $out"
printf '%s' "$out" | grep -q 'not ready' || fail "case 5: did not report not-ready: $out"
echo "PASS case 5: incomplete setup fails and names the gaps"

echo "OK: preflight-desktop-signing.sh tests passed"
