package gitinspect

import (
	"context"
	"testing"
)

// A branch's lines are counted like its pull request: since it left the
// default branch, committed and uncommitted together, whatever main did
// meanwhile; untracked and binary files count as files without lines.
func TestChangesCountTheBranchAgainstWhereItLeftTheDefaultBranch(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	git(t, root, "init", "-b", "main")
	write(t, root, "shared.txt", "one\ntwo\nthree\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "initial")
	start := git(t, root, "rev-parse", "HEAD")[:40]
	git(t, root, "switch", "-c", "feature")

	got, err := Git{}.Changes(ctx, root, "main", start)
	if err != nil || got != (Changes{Base: "main"}) {
		t.Fatalf("a fresh branch is clean: %v %+v", err, got)
	}

	// A commit: one line changed, two added.
	write(t, root, "shared.txt", "one\nTWO\nthree\nfour\nfive\n")
	git(t, root, "commit", "-qam", "edit")
	// Main moves on: it is not the branch's change.
	git(t, root, "switch", "-q", "main")
	write(t, root, "main-only.txt", "a\nb\nc\nd\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-qm", "main moves on")
	git(t, root, "switch", "-q", "feature")
	// Uncommitted: a deleted line, a binary file and an untracked file.
	write(t, root, "shared.txt", "one\nTWO\nfour\nfive\n")
	write(t, root, "logo.bin", "\x00\x01\x02")
	git(t, root, "add", "logo.bin")
	write(t, root, "notes.md", "untracked\nlines\n")

	got, err = Git{}.Changes(ctx, root, "main", start)
	want := Changes{Base: "main", Files: 3, Additions: 3, Deletions: 2}
	if err != nil || got != want {
		t.Fatalf("got %+v %v, want %+v", got, err, want)
	}
	// Without a default branch: against the workspace's start.
	if got, err = (Git{}).Changes(ctx, root, "", start); err != nil || got != (Changes{Files: 3, Additions: 3, Deletions: 2}) {
		t.Fatalf("against the start: %+v %v", got, err)
	}
	// An unknown default branch falls back to the start, never an error.
	if got, err = (Git{}).Changes(ctx, root, "origin/gone", start); err != nil || got.Base != "" || got.Files != 3 {
		t.Fatalf("unknown default: %+v %v", got, err)
	}
	// Neither: what is not committed yet.
	if got, err = (Git{}).Changes(ctx, root, "", ""); err != nil || got != (Changes{Files: 3, Additions: 0, Deletions: 1}) {
		t.Fatalf("against HEAD: %+v %v", got, err)
	}
	// What Git would read as an option is never passed to it.
	if got, err = (Git{}).Changes(ctx, root, "--output=/tmp/x", "-p"); err != nil || got.Base != "" {
		t.Fatalf("an option-like ref: %+v %v", got, err)
	}
}
