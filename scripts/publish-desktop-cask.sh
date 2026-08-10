#!/usr/bin/env bash
#
# Render the lazybox-desktop Homebrew cask from its template and publish it to
# the tap. Extracted from .github/workflows/release-desktop.yml so
# publish-desktop-cask_test.sh covers the git control flow that a workflow
# `run:` block can't.
#
# The bug this guards: on the FIRST release the cask does not yet exist in the
# tap, so it is untracked. `git diff --quiet <path>` ignores untracked files
# and reports "no change", which would silently skip the very first publish —
# the DMG ships but `brew install --cask lazybox-desktop` finds no cask. We
# stage the file BEFORE diffing the index, so a brand-new cask is a real change.
# A push that loses a race with a concurrent release is rebased and retried
# rather than left as a failed job.
#
# Usage: publish-desktop-cask.sh TEMPLATE VERSION SHA256 TAP_URL [CASK_SUBPATH]

set -euo pipefail

template="${1:?usage: $0 TEMPLATE VERSION SHA256 TAP_URL [CASK_SUBPATH]}"
version="${2:?usage: $0 TEMPLATE VERSION SHA256 TAP_URL [CASK_SUBPATH]}"
sha256="${3:?usage: $0 TEMPLATE VERSION SHA256 TAP_URL [CASK_SUBPATH]}"
tap_url="${4:?usage: $0 TEMPLATE VERSION SHA256 TAP_URL [CASK_SUBPATH]}"
cask_subpath="${5:-Casks/lazybox-desktop.rb}"

[ -f "$template" ] || { echo "::error::template not found: $template" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

rendered="$work/cask.rb"
sed -e "s/__VERSION__/${version}/g" -e "s/__SHA256__/${sha256}/g" "$template" > "$rendered"
echo "----- rendered cask -----"
cat "$rendered"
echo "-------------------------"

tapdir="$work/tap"
git clone "$tap_url" "$tapdir"
git -C "$tapdir" config user.name "lazybox-release-bot"
git -C "$tapdir" config user.email "release-bot@lazybox.ai"

mkdir -p "$(dirname "$tapdir/$cask_subpath")"
cp "$rendered" "$tapdir/$cask_subpath"

# Stage BEFORE diffing so a brand-new (untracked) cask counts as a change.
git -C "$tapdir" add "$cask_subpath"
if git -C "$tapdir" diff --cached --quiet; then
	echo "Cask already at ${version}; nothing to publish."
	exit 0
fi
git -C "$tapdir" commit -m "lazybox-desktop ${version}"

# A concurrent release may have pushed between our clone and push; rebase our
# single cask commit onto the advanced tap and retry rather than failing.
for attempt in 1 2 3; do
	if git -C "$tapdir" push; then
		echo "Published cask for ${version}."
		exit 0
	fi
	echo "push rejected (attempt ${attempt}/3); rebasing on the remote and retrying…" >&2
	git -C "$tapdir" pull --rebase --no-edit
done

echo "::error::could not push cask for ${version} after 3 attempts" >&2
exit 1
