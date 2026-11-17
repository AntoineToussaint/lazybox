package gitinspect

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
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

func TestCommittedChangesSinceBaseIncludeRenamesCommitsAndDiffs(t *testing.T) {
	root := t.TempDir()
	git(t, root, "init")
	for name, text := range map[string]string{"keep.txt": "same\n", "edit.txt": "one\n", "move.txt": "moved content\n"} {
		if err := os.WriteFile(filepath.Join(root, name), []byte(text), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "initial")
	base := strings.TrimSpace(git(t, root, "rev-parse", "HEAD"))
	if err := os.WriteFile(filepath.Join(root, "edit.txt"), []byte("two\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "new.txt"), []byte("added\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "agent edits")
	git(t, root, "mv", "move.txt", "moved.txt")
	git(t, root, "commit", "-m", "agent renames")

	got, err := Git{}.Committed(context.Background(), root, base)
	if err != nil {
		t.Fatal(err)
	}
	files := map[string]File{}
	for _, f := range got.Files {
		files[f.Path] = f
	}
	if files["edit.txt"].IndexStatus != "M" || files["new.txt"].IndexStatus != "A" || files["moved.txt"].IndexStatus != "R" || files["moved.txt"].OriginalPath != "move.txt" || len(files) != 3 {
		t.Fatalf("committed files: %+v", got.Files)
	}
	if len(got.Commits) != 2 || got.Commits[0].Subject != "agent renames" || got.Commits[1].Subject != "agent edits" || got.Commits[0].Time == 0 {
		t.Fatalf("commits: %+v", got.Commits)
	}
	diff, err := Git{}.CommittedDiff(context.Background(), root, base, "edit.txt")
	if err != nil || !strings.Contains(diff.Text, "-one") || !strings.Contains(diff.Text, "+two") {
		t.Fatalf("%v %q", err, diff.Text)
	}
	if _, err = (Git{}).CommittedDiff(context.Background(), root, base, "keep.txt"); err == nil {
		t.Fatal("an unchanged file has no branch diff")
	}
	if _, err = (Git{}).Committed(context.Background(), root, ""); err == nil {
		t.Fatal("no recorded base must be reported, not guessed")
	}
	if _, err = (Git{}).Committed(context.Background(), root, "--output=/tmp/x"); err == nil {
		t.Fatal("a base must never be read as an option")
	}
}

func TestBranchesAndComparingWithAnotherBranch(t *testing.T) {
	root := t.TempDir()
	git(t, root, "init", "-b", "main")
	write(t, root, "shared.txt", "base\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "initial")
	git(t, root, "switch", "-c", "feature")
	write(t, root, "feature.txt", "feature work\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "feature work")
	git(t, root, "switch", "main")
	write(t, root, "main-only.txt", "later on main\n")
	git(t, root, "add", ".")
	git(t, root, "commit", "-m", "main moves on")
	git(t, root, "switch", "feature")

	branches, def, err := Git{}.Branches(context.Background(), root)
	if err != nil || def != "main" || !slices.Contains(branches, "feature") || !slices.Contains(branches, "main") {
		t.Fatalf("%v %q %v", err, def, branches)
	}
	// Like a pull request: only what the branch did since it left main.
	got, err := Git{}.Committed(context.Background(), root, "main")
	if err != nil || len(got.Files) != 1 || got.Files[0].Path != "feature.txt" || len(got.Commits) != 1 {
		t.Fatalf("%v %+v", err, got)
	}
	if _, err = (Git{}).Committed(context.Background(), root, "no-such-branch"); err == nil {
		t.Fatal("an unknown branch must be reported")
	}
}
