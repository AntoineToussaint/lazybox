#!/usr/bin/env bash
# Fail when a Linux lazybox binary needs a shared library a stock distro does
# not ship. Only glibc's own libraries (and libgcc_s, part of every glibc
# install) are allowed; everything else must be linked in statically. #1893:
# the 0.1.17 and 0.1.18 releases needed `libunwind.so.1`, present on the build
# runner and on no stock distro, and the release smoke test ran on a runner
# that had it too.
#
# usage: scripts/check-self-contained.sh <binary>...
# A no-op on anything but an ELF binary, so callers need not filter macOS.
set -euo pipefail

allowed='^(libc\.so\.6|libm\.so\.6|libdl\.so\.2|libpthread\.so\.0|librt\.so\.1|libgcc_s\.so\.1|ld-linux-x86-64\.so\.2|ld-linux-aarch64\.so\.1)$'

status=0
for binary in "$@"; do
	if ! head -c 4 "${binary}" | grep -q 'ELF'; then
		continue
	fi
	needed="$(readelf -d "${binary}" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')"
	unexpected="$(grep -vE "${allowed}" <<<"${needed}" || true)"
	if [ -n "${unexpected}" ]; then
		echo "${binary} needs shared libraries a stock Linux does not have:" >&2
		while IFS= read -r library; do
			echo "  ${library}" >&2
		done <<<"${unexpected}"
		status=1
	else
		echo "${binary}: self-contained (needs only $(tr '\n' ' ' <<<"${needed}"))"
	fi
done
exit "${status}"
