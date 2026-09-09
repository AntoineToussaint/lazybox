#!/usr/bin/env bash
#
# Rebase the current branch onto origin/main, auto-resolving the desktop
# contract conflicts a wire-crate fingerprint bump produces on every such
# edit (apps/desktop/src/generated/*). Any OTHER conflict stops the rebase
# for manual resolution — this only automates the mechanical regenerate step,
# never a real code merge.
#
# It regenerates *after* git stops on the conflict, not during the merge: at a
# conflict stop git has already checked the fully-merged tree into the working
# directory (only the generated files carry markers), so `make desktop-contract`
# compiles the merged wire crates and emits the correct fingerprint. A git merge
# driver cannot do this — it runs mid-merge, before the merged source reaches the
# working tree, so it would regenerate from the un-merged (ours) side.
#
# Re-runnable mid-rebase: when it finds a rebase already in progress (you
# hand-resolved the conflict it bailed on), it skips the fetch/start and rejoins
# the resolve loop instead of tripping over the detached HEAD a stopped rebase
# leaves behind.
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

# The branch a stopped rebase will return to. HEAD is detached while the rebase
# runs, so the name only lives in the rebase state dir.
rebase_branch() {
	local dir name
	for dir in "$(git rev-parse --git-path rebase-merge)" \
		"$(git rev-parse --git-path rebase-apply)"; do
		[ -f "$dir/head-name" ] || continue
		name="$(cat "$dir/head-name")"
		printf '%s\n' "${name#refs/heads/}"
		return
	done
	printf 'HEAD\n'
}

# True when we joined a rebase that was already stopped; consumed by the first
# pass of the resolve loop, which may legitimately find nothing left to resolve.
resumed=false

if rebase_in_progress; then
	resumed=true
	branch="$(rebase_branch)"
	echo "▸ rejoining the rebase of ${branch} already in progress…"
else
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
fi

# The rebase stopped. Resolve contract-only conflicts by regenerating; bail on
# anything else.
while rebase_in_progress; do
	unmerged="$(git diff --name-only --diff-filter=U)"

	# Nothing conflicts, yet the rebase is still stopped: this isn't a contract
	# conflict we can auto-resolve (e.g. a commit that emptied on replay and
	# needs --skip, a failing hook, or unstaged changes blocking --continue).
	# Bail instead of looping — every iteration must resolve real conflicts or
	# stop, so `git rebase --continue` is never retried against unchanged state.
	if [ -z "$unmerged" ]; then
		# Unless we joined a rebase that had already been resolved by hand: this
		# run has not tried --continue yet, so that is not a retry against
		# unchanged state. Consume the flag so only one such attempt is made.
		if $resumed; then
			resumed=false
			if GIT_EDITOR=true git rebase --continue; then
				break
			fi
			continue
		fi
		echo "✗ rebase stopped without a contract conflict to auto-resolve." \
			"Sort it out by hand, then 'git rebase --continue' (or --skip / --abort):" >&2
		git status --short >&2
		exit 1
	fi

	# Every unmerged path must live under the generated contract dir.
	if printf '%s\n' "$unmerged" | grep -qv "^${CONTRACT_PREFIX}"; then
		echo "✗ conflict outside the generated contract — resolve by hand and" \
			"'git add' them, then run 'make rebase-main' again (it picks the" \
			"stopped rebase back up) or 'git rebase --continue' yourself:" >&2
		printf '%s\n' "$unmerged" | grep -v "^${CONTRACT_PREFIX}" | sed 's/^/    /' >&2
		exit 1
	fi
	echo "▸ regenerating the desktop contract to resolve:" \
		"$(printf '%s ' $unmerged)"
	make desktop-contract
	git add "$CONTRACT_PREFIX"

	# Continue; a clean finish drops out of the loop, a further conflict
	# re-enters it. GIT_EDITOR=true keeps the replayed commit messages as-is.
	if GIT_EDITOR=true git rebase --continue; then
		break
	fi
done

echo "✓ ${branch} rebased onto origin/main (contract conflicts auto-regenerated)"
