package gitinspect

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func git(t *testing.T, root string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", root}, args...)...)
	cmd.Env = append(os.Environ(), "GIT_CONFIG_NOSYSTEM=1", "GIT_AUTHOR_NAME=Fixture", "GIT_AUTHOR_EMAIL=fixture@example.invalid", "GIT_COMMITTER_NAME=Fixture", "GIT_COMMITTER_EMAIL=fixture@example.invalid")
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, out)
	}
	return string(out)
}
func write(t *testing.T, root, name, text string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(root, name), []byte(text), 0600); err != nil {
		t.Fatal(err)
	}
}
func TestInspectionHandlesStagedUnstagedRenamesOddNamesAndRejectsEscapes(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	git(t, root, "init")
	write(t, root, "file.txt", "original\n")
	write(t, root, "rename.txt", "rename\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "initial")
	write(t, root, "file.txt", "staged\n")
	git(t, root, "add", "file.txt")
	write(t, root, "file.txt", "unstaged\n")
	git(t, root, "mv", "rename.txt", "renamed.txt")
	odd := "odd\nname [*].txt"
	write(t, root, odd, "new text\n")
	write(t, root, "binary", string([]byte{0, 1, 2}))
	write(t, root, "huge", strings.Repeat("x", outputLimit+1))
	outside := filepath.Join(t.TempDir(), "secret")
	write(t, filepath.Dir(outside), "secret", "never expose")
	if err := os.Symlink(outside, filepath.Join(root, "outside")); err != nil {
		t.Fatal(err)
	}
	before := git(t, root, "status", "--porcelain=v1", "-z")
	g := Git{}
	state, err := g.Status(ctx, root)
	if err != nil {
		t.Fatal(err)
	}
	var rename bool
	for _, f := range state.Files {
		if f.Path == "renamed.txt" {
			rename = f.OriginalPath == "rename.txt"
		}
		if f.Path == "file.txt" && (f.IndexStatus != "M" || f.WorktreeStatus != "M") {
			t.Fatal(f)
		}
	}
	if !rename || state.Head == "" {
		t.Fatal(state)
	}
	for _, test := range []struct {
		path   string
		staged bool
		want   string
	}{{"file.txt", false, "+unstaged"}, {"file.txt", true, "+staged"}, {"renamed.txt", true, "rename from"}, {odd, false, "new text"}} {
		diff, err := g.Diff(ctx, root, test.path, test.staged)
		if err != nil || !strings.Contains(diff.Text, test.want) {
			t.Fatalf("%+v: %+v %v", test, diff, err)
		}
	}
	if diff, err := g.Diff(ctx, root, "binary", false); err != nil || !diff.Binary {
		t.Fatal(diff, err)
	}
	if diff, err := g.Diff(ctx, root, "huge", false); err != nil || !diff.Truncated {
		t.Fatal(diff, err)
	}
	for _, path := range []string{"../secret", outside, "outside", ".git/config", "missing"} {
		if _, err := g.Diff(ctx, root, path, false); err == nil {
			t.Fatalf("accepted %s", path)
		}
	}
	if after := git(t, root, "status", "--porcelain=v1", "-z"); after != before {
		t.Fatal("inspection mutated index or files")
	}
}
func TestInspectionDoesNotRunConfiguredDiffToolsAndHonorsCancellation(t *testing.T) {
	root := t.TempDir()
	git(t, root, "init")
	write(t, root, "file", "one\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "initial")
	write(t, root, "file", "two\n")
	git(t, root, "config", "diff.external", "this-command-must-not-execute")
	g := Git{}
	if diff, err := g.Diff(context.Background(), root, "file", false); err != nil || !strings.Contains(diff.Text, "+two") {
		t.Fatal(diff, err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := g.Status(ctx, root); err == nil {
		t.Fatal("ignored cancellation")
	}
}
