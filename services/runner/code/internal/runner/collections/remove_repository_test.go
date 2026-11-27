package collections

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRemoveRepositoryMemberPreservesFilesBranchAndWorkspace(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, repo)
	c, err := m.Create(ctx, "remove", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	inactive := func(string) bool { return false }
	if _, err := m.Acquire(ctx, c.ID, "owner", inactive); err != nil {
		t.Fatal(err)
	}
	_, err = m.RemoveRepositoryMember(ctx, c.ID, member.Name, func(string) bool { return true }, nil)
	var blocker *ActiveRepositoryRemovalOwner
	if !errors.As(err, &blocker) || blocker.ExecutionID != "owner" {
		t.Fatalf("want active owner identified, got %v", err)
	}
	untracked := filepath.Join(member.Path, "local.txt")
	if err := os.WriteFile(untracked, []byte("keep"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := m.RemoveRepositoryMember(ctx, c.ID, member.Name, inactive, nil); err == nil {
		t.Fatal("removed a dirty checkout")
	}
	if content, err := os.ReadFile(untracked); err != nil || string(content) != "keep" {
		t.Fatalf("local file changed: %q, %v", content, err)
	}
	if err := os.Remove(untracked); err != nil {
		t.Fatal(err)
	}
	// A clean checkout can still have commits that only exist on its branch.
	gitTest(t, member.Path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "local commit")
	tip := gitTest(t, member.Path, "rev-parse", "HEAD")
	gitTest(t, member.Path, "checkout", "--detach")
	if _, err := m.RemoveRepositoryMember(ctx, c.ID, member.Name, inactive, nil); err == nil {
		t.Fatal("removed a detached checkout")
	}
	gitTest(t, member.Path, "checkout", member.Branch)
	writeTest(t, filepath.Join(member.Path, ".gitignore"), "node_modules/\nbuild/\n.env\n")
	gitTest(t, member.Path, "add", ".gitignore")
	gitTest(t, member.Path, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "ignore generated output")
	tip = gitTest(t, member.Path, "rev-parse", "HEAD")
	writeTest(t, filepath.Join(member.Path, "pkg/node_modules/dep/index.js"), "dependency")
	writeTest(t, filepath.Join(member.Path, "build/out"), "output")
	env := filepath.Join(member.Path, ".env")
	writeTest(t, env, "local settings")
	if _, err := m.RemoveRepositoryMember(ctx, c.ID, member.Name, inactive, nil); err == nil {
		t.Fatal("removed a checkout with local settings")
	}
	if err := os.Remove(env); err != nil {
		t.Fatal(err)
	}
	failedSave := errors.New("runtime record could not be written")
	if _, err := m.RemoveRepositoryMember(ctx, c.ID, member.Name, inactive, func() error { return failedSave }); !errors.Is(err, failedSave) {
		t.Fatalf("expected removal to report an incomplete update: %v", err)
	}
	updated, err := m.RemoveRepositoryMember(ctx, c.ID, member.Name, inactive, nil)
	if err != nil || updated.State != "ready" || len(updated.Members) != 0 {
		t.Fatal(updated, err)
	}
	if _, err := os.Stat(c.Root); err != nil {
		t.Fatal("workspace root lost", err)
	}
	if _, err := os.Stat(member.Path); !os.IsNotExist(err) {
		t.Fatal("worktree remains", err)
	}
	if strings.Contains(gitTest(t, repo.Path, "worktree", "list", "--porcelain"), member.Path) {
		t.Fatal("stale worktree registration")
	}
	if rev := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+member.Branch); rev != tip {
		t.Fatal("branch lost")
	}
	if _, err := os.Stat(filepath.Join(repo.Path, "source.txt")); err != nil {
		t.Fatal("source clone touched", err)
	}
	if err := m.Release(c.ID, "owner"); err != nil {
		t.Fatal(err)
	}
	m.Close()
	fresh := newTest(t, root, repo)
	reloaded, err := fresh.Get(c.ID)
	if err != nil || len(reloaded.Members) != 0 {
		t.Fatal("restart restored removed worktree", reloaded, err)
	}
}

func TestReaddRemovedRepositoryKeepsExistingBranch(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	c, err := m.Create(ctx, "empty", Spec{Name: "Empty"})
	if err != nil {
		t.Fatal(err)
	}
	spec := []MemberSpec{{RepositoryID: repo.ID}}
	c, err = m.AddRepositories(ctx, c.ID, "first", spec)
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0]
	if _, err := m.RemoveRepositoryMember(ctx, c.ID, member.Name, func(string) bool { return false }, nil); err != nil {
		t.Fatal(err)
	}
	c, err = m.AddRepositories(ctx, c.ID, "second", spec)
	if err != nil || len(c.Members) != 1 || c.Members[0].Branch == member.Branch {
		t.Fatal(c, err)
	}
	if rev := gitTest(t, repo.Path, "rev-parse", "refs/heads/"+member.Branch); rev != member.BaseCommit {
		t.Fatal("branch changed")
	}
}

func TestDeleteLocalBranch(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	current := gitTest(t, repo.Path, "branch", "--show-current")
	if err := m.DeleteLocalBranch(ctx, repo.ID, current); err == nil {
		t.Fatal("deleted checked-out branch")
	}
	gitTest(t, repo.Path, "branch", "feature/conflict")
	if err := m.DeleteLocalBranch(ctx, repo.ID, "feature/conflict"); err != nil {
		t.Fatal(err)
	}
	if exists, err := m.branchExists(ctx, repo.Path, "feature/conflict"); err != nil || exists {
		t.Fatalf("branch exists = %t, err = %v", exists, err)
	}
}
