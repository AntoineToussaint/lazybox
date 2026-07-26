#!/bin/sh

# Preserve the public installer URL after the executable moved to its boot crate.
set -eu

installer_url="https://github.com/AntoineToussaint/lazybox/releases/latest/download/lazybox-tui-boot-installer.sh"
installer_tmp="$(mktemp "${TMPDIR:-/tmp}/lazybox-installer.XXXXXX")"
trap 'rm -f -- "$installer_tmp"' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 -LsSf "$installer_url" --output "$installer_tmp"
sh "$installer_tmp" "$@"
