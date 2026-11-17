// Package codecheckpoints records the files of a conversation's workspace
// around every agent turn and puts them back on request.
//
// Before a turn starts (and again when it ends) each member repository's
// working tree, uncommitted and untracked files included and ignored files
// excluded, is written as a commit under a private ref,
// refs/lazybox/checkpoints/<conversation>/…, through a temporary index. The
// person's index, HEAD, branch and stash are never touched, and nothing is
// written when nothing changed: the previous commit is reused. Across a
// multi-repository workspace a checkpoint is the set of those per-repository
// commits.
//
// Reverting to a checkpoint first saves the current files as a safety
// checkpoint, so the revert itself can be undone, then rewrites only the files
// that differ. It never moves a branch: commits made since stay, and the
// revert shows as uncommitted changes. Turns, reverts and their refs are kept
// within bounds; the oldest are pruned.
//
// This is unrelated to package checkpoints, which hands work over between
// agent executions.
package codecheckpoints
