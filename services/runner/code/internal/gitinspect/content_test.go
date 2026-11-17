package gitinspect

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestContentReadsHeadCommitsAndTheWorkingTreeWithinTheRepository(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	git(t, root, "init")
	if _, err := (Git{}).Head(ctx, root); err == nil {
		t.Fatal("a repository without commits has a head")
	}
	write(t, root, "file.txt", "first\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "initial")
	head, err := Git{}.Head(ctx, root)
	if err != nil || head != strings.TrimSpace(git(t, root, "rev-parse", "HEAD")) {
		t.Fatal(head, err)
	}
	write(t, root, "file.txt", "second\n")
	content, exists, err := Git{}.ShowAt(ctx, root, head, "file.txt")
	if err != nil || !exists || string(content) != "first\n" {
		t.Fatalf("at commit: %q %v %v", content, exists, err)
	}
	if _, exists, err = (Git{}).ShowAt(ctx, root, head, "missing.txt"); err != nil || exists {
		t.Fatalf("missing at commit: %v %v", exists, err)
	}
	for _, bad := range [][2]string{{head, "../x"}, {"--output=x", "file.txt"}, {"HEAD:file.txt", "file.txt"}} {
		if _, _, err = (Git{}).ShowAt(ctx, root, bad[0], bad[1]); err == nil {
			t.Errorf("ShowAt(%q, %q) accepted", bad[0], bad[1])
		}
	}
	if content, exists, err = ReadWorking(root, "file.txt"); err != nil || !exists || string(content) != "second\n" {
		t.Fatalf("working: %q %v %v", content, exists, err)
	}
	if _, exists, err = ReadWorking(root, "gone.txt"); err != nil || exists {
		t.Fatalf("missing working file: %v %v", exists, err)
	}
	outside := filepath.Join(t.TempDir(), "secret")
	write(t, filepath.Dir(outside), "secret", "x")
	if err = os.Symlink(outside, filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}
	if _, _, err = ReadWorking(root, "link"); err == nil {
		t.Fatal("a symlink out of the worktree was followed")
	}
	if _, _, err = ReadWorking(root, "../file.txt"); err == nil {
		t.Fatal("an escaping path was read")
	}
	write(t, root, "binary.bin", "a\x00b")
	if _, exists, err = ReadWorking(root, "binary.bin"); err == nil || !exists {
		t.Fatal("binary content read as text")
	}
}
