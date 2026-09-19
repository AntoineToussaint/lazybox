#!/usr/bin/env bash
# Validate the public install contract in release-note text read from stdin.

set -euo pipefail

notes="$(cat)"
supported_brew='brew tap AntoineToussaint/lazybox && brew trust AntoineToussaint/lazybox && brew install lazybox'

[[ "$notes" == *"$supported_brew"* ]] || {
	echo "error: release notes omit the supported Homebrew install command" >&2
	exit 1
}

for forbidden in \
	'brew install AntoineToussaint/lazybox/lazybox' \
	'lazybox-tui-boot' \
	'lazybox-tui-installer.sh'; do
	[[ "$notes" != *"$forbidden"* ]] || {
		echo "error: release notes contain unsupported public install identity: $forbidden" >&2
		exit 1
	}
done
