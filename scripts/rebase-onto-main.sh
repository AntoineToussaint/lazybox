#!/usr/bin/env bash
#
# Rebase the current branch onto origin/main, auto-resolving the desktop
# contract conflicts a wire-crate fingerprint bump produces on every such
# edit (apps/desktop/src/generated/*). Any OTHER conflict stops the rebase
# for manual resolution — this only automates the mechanical regenerate step,
# never a real code merge.
#
# With the `lazybox-contract` merge driver installed (make setup /
# make install-merge-driver) the rebase usually resolves those files itself
# and this script just drives fetch → rebase → done. The regenerate loop
# below is the fallback for a clone without the driver, so `make rebase-main`
# works either way.
#
# Run via `make rebase-main` (which puts pinned zig on PATH).

set -euo pipefail

CONTRACT_PREFIX='apps/desktop/src/generated/'

on_our_branch() {
	git symbolic-ref --quiet HEAD >/dev/null 2>&1
}

rebase_in_progress() {
	local gitdir
	gitdir="$(git rev-parse --git-path rebase-merge)"
	[ -d "$gitdir" ] || {
		gitdir="$(git rev-parse --git-path rebase-apply)"
		[ -d "$gitdir" ]
	}
}

if ! on_our_branch; then
	echo "✗ detached HEAD — check out a branch before rebasing." >&2
	exit 1
fi
branch="$(git rev-parse --abbrev-ref HEAD)"

echo "▸ fetching origin/main…"
git fetch origin main

echo "▸ rebasing ${branch} onto origin/main…"
if git rebase origin/main; then
	echo "✓ ${branch} rebased cleanly onto origin/main"
	exit 0
fi

# The rebase stopped. Resolve contract-only conflicts by regenerating; bail on
# anything else.
while rebase_in_progress; do
	unmerged="$(git diff --name-only --diff-filter=U)"

	if [ -n "$unmerged" ]; then
		# Every unmerged path must live under the generated contract dir.
		if printf '%s\n' "$unmerged" | grep -qv "^${CONTRACT_PREFIX}"; then
			echo "✗ conflict outside the generated contract — resolve by hand," \
				"then run 'git rebase --continue' (or 'make rebase-main' again):" >&2
			printf '%s\n' "$unmerged" | grep -v "^${CONTRACT_PREFIX}" | sed 's/^/    /' >&2
			exit 1
		fi
		echo "▸ regenerating the desktop contract to resolve:" \
			"$(printf '%s ' $unmerged)"
		make desktop-contract
		git add "$CONTRACT_PREFIX"
	fi

	# Continue; a clean finish drops out of the loop, a further conflict
	# re-enters it. GIT_EDITOR=true keeps the replayed commit messages as-is.
	if GIT_EDITOR=true git rebase --continue; then
		break
	fi
done

echo "✓ ${branch} rebased onto origin/main (contract conflicts auto-regenerated)"
