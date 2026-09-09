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
# Re-runnable mid-rebase: when it finds a rebase already in progress that it
# could have started itself (you hand-resolved the conflict it bailed on), it
# skips the fetch/start and rejoins the resolve loop instead of tripping over
# the detached HEAD a stopped rebase leaves behind. Any other rebase in flight
# is refused untouched — see ours_to_resume.
#
# Run via `make rebase-main` (which puts pinned zig on PATH).

set -euo pipefail

CONTRACT_PREFIX='apps/desktop/src/generated/'

on_our_branch() {
	git symbolic-ref --quiet HEAD >/dev/null 2>&1
}

# Where a stopped rebase keeps its state, or non-zero when none is in progress.
rebase_state_dir() {
	local dir
	for dir in "$(git rev-parse --git-path rebase-merge)" \
		"$(git rev-parse --git-path rebase-apply)"; do
		if [ -d "$dir" ]; then
			printf '%s\n' "$dir"
			return 0
		fi
	done
	return 1
}

rebase_in_progress() {
	rebase_state_dir >/dev/null
}

# Whether a stopped rebase is one this script could have started, and so may be
# driven to completion under a "rebased onto origin/main" banner. Two things
# must hold, and both fail closed:
#
#   * it replays onto origin/main — or onto a commit origin/main has since moved
#     past, which its reflog still remembers, so a fetch racing our resolve does
#     not lock us out of our own rebase;
#   * every step left to replay, including the one we are stopped on (the last
#     line of `done`), is a plain pick — `git rebase --continue` runs the rest of
#     the todo, where GIT_EDITOR=true would silently accept the prefilled message
#     of a `reword`/`squash` and an `exec` would just run.
#
# Anything else is someone else's operation: finishing it would replay commits
# the caller never asked about and report a base they are not on.
ours_to_resume() {
	local dir="$1" onto steps
	onto="$(cat "$dir/onto" 2>/dev/null || true)"
	[ -n "$onto" ] || return 1
	git rev-parse --verify --quiet origin/main >/dev/null || return 1
	if [ "$onto" != "$(git rev-parse origin/main)" ] &&
		! grep -qxF "$onto" <<<"$(git reflog show origin/main --format=%H 2>/dev/null)"
	then
		return 1
	fi
	steps="$( { tail -n 1 "$dir/done" 2>/dev/null || true
		cat "$dir/git-rebase-todo" 2>/dev/null || true
	} | sed -e 's/#.*//' -e '/^[[:space:]]*$/d' )"
	[ -n "$steps" ] || return 1
	! grep -qvE '^(pick|p) ' <<<"$steps"
}

resumed=false

if rebase_in_progress; then
	state="$(rebase_state_dir)"
	if ! ours_to_resume "$state"; then
		echo "✗ a rebase is in progress that this script did not start — it does" \
			"not replay onto origin/main, or has more than plain picks left." \
			"Finish it with 'git rebase --continue' or drop it with" \
			"'git rebase --abort', then re-run:" >&2
		git status --short >&2
		exit 1
	fi
	resumed=true
	echo "▸ rejoining the in-progress rebase onto origin/main…"
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
#
# bare_continue is a one-shot: only the first pass of a resumed run may find
# nothing left to resolve and still run --continue. It is consumed at the top of
# every pass, so once any pass has run --continue a later stop cannot retry it
# against unchanged state.
bare_continue=$resumed
regenerated=false

while rebase_in_progress; do
	unmerged="$(git diff --name-only --diff-filter=U)"
	may_bare_continue=$bare_continue
	bare_continue=false

	# Nothing conflicts, yet the rebase is still stopped: this isn't a contract
	# conflict we can auto-resolve (e.g. a commit that emptied on replay and
	# needs --skip, a failing hook, or unstaged changes blocking --continue).
	# Bail instead of looping — every iteration must resolve real conflicts or
	# stop, so `git rebase --continue` is never retried against unchanged state.
	if [ -z "$unmerged" ]; then
		# Unless we joined a rebase that had already been resolved by hand and
		# this run has not run --continue yet: that is not a retry against
		# unchanged state.
		if [ "$may_bare_continue" = true ]; then
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
	regenerated=true

	# Continue; a clean finish drops out of the loop, a further conflict
	# re-enters it. GIT_EDITOR=true keeps the replayed commit messages as-is.
	if GIT_EDITOR=true git rebase --continue; then
		break
	fi
done

# HEAD is reattached now the rebase is done, so this names the branch on both
# the resumed path (where we never had it) and the one that started the rebase.
branch="$(git rev-parse --abbrev-ref HEAD)"
if [ "$regenerated" = true ]; then
	echo "✓ ${branch} rebased onto origin/main (contract conflicts auto-regenerated)"
else
	echo "✓ ${branch} rebased onto origin/main"
fi

# A resumed rebase replays onto the origin/main of whenever it started, and even
# a fresh one can be overtaken while the contract builds.
if ! git merge-base --is-ancestor origin/main HEAD; then
	echo "! origin/main has moved on since this rebase started —" \
		"re-run 'make rebase-main' to catch up." >&2
fi
