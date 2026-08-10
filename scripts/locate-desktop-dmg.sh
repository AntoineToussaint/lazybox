#!/usr/bin/env bash
#
# Locate the single .dmg the desktop release build produced, copy it to a
# normalized name, and write its sha256. Extracted from
# .github/workflows/release-desktop.yml so locate-desktop-dmg_test.sh can prove
# it FAILS LOUDLY on zero or multiple dmgs rather than silently picking one
# with `find … | head -n1` — a stale or extra bundle would otherwise ship the
# wrong file under the right name, with a matching sha, undetected.
#
# Usage: locate-desktop-dmg.sh BUNDLE_DIR OUT_DMG OUT_SHA
#   BUNDLE_DIR  the tauri bundle dir (its `dmg/` subdir is searched)
#   OUT_DMG     path to copy the located dmg to (the normalized release name)
#   OUT_SHA     path to write the dmg's lowercase hex sha256 to

set -euo pipefail

bundle_dir="${1:?usage: $0 BUNDLE_DIR OUT_DMG OUT_SHA}"
out_dmg="${2:?usage: $0 BUNDLE_DIR OUT_DMG OUT_SHA}"
out_sha="${3:?usage: $0 BUNDLE_DIR OUT_DMG OUT_SHA}"

# bash 3.2 (the macOS system bash CI runs) has no `mapfile`; read into an array.
dmgs=()
while IFS= read -r f; do
	dmgs+=("$f")
done < <(find "$bundle_dir/dmg" -maxdepth 1 -type f -name '*.dmg' 2>/dev/null | sort)

case "${#dmgs[@]}" in
	0)
		echo "::error::no .dmg found under ${bundle_dir}/dmg" >&2
		find "$bundle_dir" -maxdepth 2 -print >&2 2>/dev/null || true
		exit 1
		;;
	1) : ;;
	*)
		echo "::error::expected exactly one .dmg under ${bundle_dir}/dmg, found ${#dmgs[@]}: ${dmgs[*]}" >&2
		exit 1
		;;
esac

cp "${dmgs[0]}" "$out_dmg"

if command -v shasum >/dev/null 2>&1; then
	sha="$(shasum -a 256 "$out_dmg" | awk '{print $1}')"
else
	sha="$(sha256sum "$out_dmg" | awk '{print $1}')"
fi
printf '%s\n' "$sha" > "$out_sha"
echo "dmg=${out_dmg} sha256=${sha}"
