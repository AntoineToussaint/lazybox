#!/usr/bin/env bash
#
# Register the `lazybox-contract` git merge driver in this clone's config so
# a rebase/merge that conflicts on apps/desktop/src/generated/* resolves by
# regenerating the contract instead of dropping conflict markers.
#
# A merge driver lives in *local* git config (never committed), so it has to
# be (re)registered per clone. `make setup` (scripts/bootstrap.sh) does this
# automatically; `make install-merge-driver` is the manual entry point for a
# clone that predates it. The config is written to the shared repo config, so
# one run covers every linked worktree of the clone, and the relative driver
# path resolves against each worktree's root at merge time. Idempotent.

set -euo pipefail

root="$(git rev-parse --show-toplevel)"

git -C "$root" config merge.lazybox-contract.name \
	'regenerate the desktop contract from the merged tree'
git -C "$root" config merge.lazybox-contract.driver \
	'./scripts/git-merge-contract.sh %O %A %B %P'

echo "git: registered the lazybox-contract merge driver (apps/desktop/src/generated/*)"
