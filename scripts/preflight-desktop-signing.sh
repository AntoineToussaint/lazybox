#!/usr/bin/env bash
#
# Preflight helper for the desktop release signing setup.
#
# release-desktop.yml needs six APPLE_* values that originate from a human's
# Apple Developer account (membership, an exported .p12, an app-specific
# password) and cannot be produced by CI. This script removes the two
# error-prone steps around them:
#
#   identity <cert.p12> [password]
#       Print the exact APPLE_SIGNING_IDENTITY and APPLE_TEAM_ID for a
#       Developer ID Application .p12. `tauri build` signs nothing unless
#       APPLE_SIGNING_IDENTITY matches the certificate's Common Name character
#       for character, so deriving it from the .p12 beats hand-typing it. Fails
#       loudly if the .p12 is not a Developer ID Application certificate (e.g. a
#       plain "Apple Development" cert, which Gatekeeper rejects for a cask).
#
#   check [--repo OWNER/REPO]
#       Audit whether the repo is fully provisioned: all six APPLE_* secrets
#       set and the DESKTOP_RELEASE_ENABLED variable true. Reports exactly what
#       is missing and exits non-zero until the setup is complete.
#
# Secret values are never printed (the signing identity and team id are not
# secret — they are embedded in every signed binary). Run directly:
#   bash scripts/preflight-desktop-signing.sh identity cert.p12
#   bash scripts/preflight-desktop-signing.sh check

set -euo pipefail

SECRETS=(APPLE_CERTIFICATE APPLE_CERTIFICATE_PASSWORD APPLE_SIGNING_IDENTITY \
         APPLE_ID APPLE_PASSWORD APPLE_TEAM_ID)

die() { echo "error: $*" >&2; exit 1; }

cmd_identity() {
	local p12="${1:-}" pass="${2:-}"
	[ -n "$p12" ] || die "usage: $0 identity <cert.p12> [password]"
	[ -f "$p12" ] || die "no such file: $p12"
	command -v openssl >/dev/null || die "openssl is required"

	# macOS Keychain encrypts the certificate bag with PBE-SHA1-RC2-40, which
	# OpenSSL 3.x moved to the legacy provider — so a valid Keychain export
	# won't open without -legacy. Try the modern path, then legacy, before
	# blaming the password. (-legacy only exists on OpenSSL 3.x; on LibreSSL /
	# 1.x the first attempt already reads RC2-40 natively, so the retry is
	# skipped.) `-nameopt multiline` prints one RDN per line, so a comma inside
	# the CN (part of the account name) can't be mistaken for a field separator.
	local subject="" prov
	for prov in "" "-legacy"; do
		if subject="$(openssl pkcs12 $prov -in "$p12" -passin "pass:$pass" -nokeys -clcerts 2>/dev/null \
			| openssl x509 -noout -subject -nameopt multiline 2>/dev/null)" && [ -n "$subject" ]; then
			break
		fi
		subject=""
	done
	[ -n "$subject" ] || die "could not read '$p12' — wrong password, or not a .p12 (pass the export password as arg 2)"

	local cn ou
	cn="$(printf '%s\n' "$subject" | sed -n 's/^ *commonName *= //p' | head -n1)"
	ou="$(printf '%s\n' "$subject" | sed -n 's/^ *organizationalUnitName *= //p' | head -n1)"

	[ -n "$cn" ] || die "certificate has no Common Name; is this a code-signing cert?"
	case "$cn" in
		"Developer ID Application: "*) ;;
		*) die "'$cn' is not a Developer ID Application certificate. A Homebrew cask must be signed with a Developer ID Application cert (Apple Development / Mac App Distribution certs are Gatekeeper-rejected outside the App Store)." ;;
	esac

	# The CN ends in "(TEAMID)" and the OU is the same team id; require them to
	# agree so an odd export can't yield a plausible-but-wrong team id.
	local team_from_cn
	team_from_cn="$(printf '%s' "$cn" | sed -n 's/.*(\([A-Z0-9]*\))$/\1/p')"
	[ -n "$team_from_cn" ] || die "CN is missing the trailing (TEAMID): '$cn'"
	if [ -n "$ou" ] && [ "$ou" != "$team_from_cn" ]; then
		die "team id mismatch: OU='$ou' but CN team='$team_from_cn'"
	fi

	echo "APPLE_SIGNING_IDENTITY=$cn"
	echo "APPLE_TEAM_ID=${ou:-$team_from_cn}"
}

cmd_check() {
	local repo_args=()
	while [ $# -gt 0 ]; do
		case "$1" in
			--repo) repo_args+=(--repo "${2:?--repo needs a value}"); shift 2 ;;
			*) die "unknown argument: $1" ;;
		esac
	done
	command -v gh >/dev/null || die "gh (GitHub CLI) is required for 'check'"

	local present
	# bash 3.2 (macOS /bin/bash) errors on "${empty[@]}" under `set -u`, so guard
	# the expansion — repo_args is empty whenever --repo is omitted.
	present="$(gh secret list ${repo_args[@]+"${repo_args[@]}"} 2>/dev/null | awk 'NF{print $1}')" \
		|| die "could not list repo secrets — is gh authenticated with admin access?"

	local missing=() name
	for name in "${SECRETS[@]}"; do
		printf '%s\n' "$present" | grep -qx "$name" || missing+=("$name")
	done

	# Parse `gh variable list` (TSV: NAME<tab>VALUE<tab>UPDATED) rather than
	# `gh variable get`, which only exists on gh >= 2.40 and would otherwise
	# report an already-set variable as unset on older CLIs. Empty on absent.
	local enabled
	enabled="$(gh variable list ${repo_args[@]+"${repo_args[@]}"} 2>/dev/null \
		| awk -F'\t' '$1=="DESKTOP_RELEASE_ENABLED"{print $2; exit}')" || true

	local ok=0
	if [ "${#missing[@]}" -eq 0 ]; then
		echo "secrets:  all six APPLE_* secrets set"
	else
		ok=1
		echo "secrets:  MISSING ${missing[*]}"
	fi
	if [ "$enabled" = "true" ]; then
		echo "variable: DESKTOP_RELEASE_ENABLED=true"
	else
		ok=1
		echo "variable: DESKTOP_RELEASE_ENABLED is '${enabled:-unset}' (must be 'true')"
	fi

	if [ "$ok" -eq 0 ]; then
		echo "ready: tagging v<x.y.z> will build, sign, notarize, and ship the desktop cask."
	else
		echo "not ready: resolve the items above, then re-run 'check'." >&2
	fi
	return "$ok"
}

sub="${1:-}"
shift || true
case "$sub" in
	identity) cmd_identity "$@" ;;
	check)    cmd_check "$@" ;;
	*) die "usage: $0 {identity <cert.p12> [password] | check [--repo OWNER/REPO]}" ;;
esac
