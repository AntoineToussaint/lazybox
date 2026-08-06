#!/usr/bin/env bash
#
# Git merge driver for apps/desktop/src/generated/* — registered as
# `lazybox-contract` (see .gitattributes and scripts/install-merge-driver.sh).
#
# Those files are generated from the Rust desktop DTOs and carry a protocol
# fingerprint hashed over the wire crates (crates/ipc/build.rs hashes
# crates/{ipc,core}/src + Cargo.lock). So *any* edit under those crates
# rewrites the fingerprint, and a rebase across such an edit conflicts on the
# generated files every single time — a fingerprint line that differs on both
# sides. Neither side is "right": the merged tree's fingerprint is a third
# value only the generator can produce. This driver produces it, instead of
# leaving conflict markers a human has to hand-regenerate.
#
# Git invokes it once per conflicting file with (from .gitattributes):
#     %O %A %B %P
#   $1 = ancestor blob   (unused)
#   $2 = ours/current    — the driver MUST write the merged result here
#   $3 = theirs blob     (unused)
#   $4 = pathname        — the conflicting file's path in the working tree
#
# Exit 0 → git accepts $2 as the resolved content. Non-zero → the conflict
# stands for manual resolution; that is the correct fallback when the tree
# can't be regenerated (e.g. a Rust file ALSO conflicted, so the build fails).
#
# The driver regenerates the whole contract from the already-merged working
# tree — at merge time git has applied every non-conflicting change, so the
# wire crates reflect the merged state and the generator yields the right
# fingerprint. `make desktop-contract` owns the pinned-zig PATH and both
# generation steps; cargo caches the build across the two per-file invocations
# a two-file conflict makes, so only the first pays the compile cost.

set -euo pipefail

current="$2"
path="$4"

if ! make desktop-contract >/dev/null 2>&1; then
	echo "lazybox-contract: 'make desktop-contract' failed — leaving ${path} conflicted." \
		"Resolve the underlying build breakage, run 'make desktop-contract', then 'git add' it." >&2
	exit 1
fi

# Hand git the freshly generated version of this file.
cp -- "$path" "$current"
