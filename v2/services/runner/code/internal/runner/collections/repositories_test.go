package collections

import (
	"context"
	"os"
	"path/filepath"
	"sync"
	"testing"
)

func TestLocalRepositoryWithoutRemoteOrGitHubPreservesCheckoutAndSurvivesRestart(t *testing.T) {
	repo := repoTest(t)
	gitTest(t, repo.Path, "config", "credential.helper", "fixture-never-run")
	before, err := os.ReadFile(filepath.Join(repo.Path, ".git", "config"))
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(repo.Path, "source.txt"), []byte("uncommitted"), 0600); err != nil {
		t.Fatal(err)
	}
	root := t.TempDir()
	m := newTest(t, root)
	added, err := m.AddLocalRepository(context.Background(), repo.Path, "api")
	if err != nil || added.Name != "api" || added.SourceURL != "" || added.GitHubID != 0 {
		t.Fatal(added, err)
	}
	if m.RepositoryIdentity(added.ID) != added.ID || m.RepositoryIdentity("unknown") != "" {
		t.Fatal("library identity does not match canonical Git identity")
	}
	// Repeated requests and symlink aliases keep the same logical repository.
	alias := filepath.Join(t.TempDir(), "alias")
	if err = os.Symlink(repo.Path, alias); err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	for n := 0; n < 5; n++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			next, err := m.AddLocalRepository(context.Background(), alias, "")
			if err != nil || next.ID != added.ID {
				t.Error(next, err)
			}
		}()
	}
	wg.Wait()
	if len(m.Repositories()) != 1 {
		t.Fatal("duplicate entries")
	}
	workspace, err := m.Create(context.Background(), "local-workspace", Spec{Name: "my-feature", Members: []MemberSpec{{RepositoryID: added.ID}}})
	if err != nil || workspace.State != "ready" || workspace.Members[0].Name != "api" {
		t.Fatal(workspace, err)
	}
	again, err := m.AddLocalRepository(context.Background(), workspace.Members[0].Path, "")
	if err != nil || again.ID != added.ID {
		t.Fatal("linked worktree duplicated repository", again, err)
	}
	if m.RepositoryIdentity(again.ID) != m.RepositoryIdentity(added.ID) {
		t.Fatal("worktree has a different Git identity")
	}
	after, err := os.ReadFile(filepath.Join(repo.Path, ".git", "config"))
	if err != nil || string(after) != string(before) {
		t.Fatal("Git config changed", err)
	}
	contents, err := os.ReadFile(filepath.Join(repo.Path, "source.txt"))
	if err != nil || string(contents) != "uncommitted" || gitTest(t, repo.Path, "branch", "--show-current") != "main" {
		t.Fatal("checkout changed", err)
	}
	if got := gitTest(t, repo.Path, "remote"); got != "" {
		t.Fatal("added a remote")
	}
	m.Close()
	m = newTest(t, root)
	if len(m.Repositories()) != 1 || m.Repositories()[0] != added || len(m.List()) != 1 {
		t.Fatal("registration did not survive restart")
	}
}

func TestLocalRepositoryValidationAndExistingRegistration(t *testing.T) {
	repo := repoTest(t)
	m := newTest(t, t.TempDir(), repo)
	existing, err := m.AddLocalRepository(context.Background(), repo.Path, "")
	if err != nil || existing.ID != repo.ID {
		t.Fatal(existing, err)
	}
	fresh := newTest(t, t.TempDir())
	nested := filepath.Join(repo.Path, "subfolder")
	os.Mkdir(nested, 0700)
	empty := t.TempDir()
	gitTest(t, empty, "init")
	for _, path := range []string{"relative", t.TempDir(), nested, filepath.Join(repo.Path, ".git"), empty} {
		if _, err := fresh.AddLocalRepository(context.Background(), path, ""); err == nil {
			t.Fatalf("accepted invalid path %q", path)
		}
	}
	if _, err := fresh.AddLocalRepository(context.Background(), repo.Path, "../escape"); err == nil {
		t.Fatal("accepted unsafe name")
	}
	if len(fresh.Repositories()) != 0 {
		t.Fatal("invalid attempts changed library")
	}
	fresh.Close()
	if _, err := fresh.AddLocalRepository(context.Background(), repo.Path, "api"); err == nil {
		t.Fatal("accepted closed manager")
	}
}

func TestGitHubRepositoryRegistrationUnifiesClonesAndPreservesWorkspace(t *testing.T) {
	first, second := repoTest(t), repoTest(t)
	gitTest(t, first.Path, "remote", "add", "origin", "git@github.com:Team/API.git")
	gitTest(t, second.Path, "remote", "add", "origin", "https://github.com/team/api.git")
	root := t.TempDir()
	m := newTest(t, root)
	one, err := m.AddLocalRepository(context.Background(), first.Path, "")
	if err != nil || one.Name != "api" || one.SourceURL != "https://github.com/team/api" || m.RepositoryIdentity(one.ID) != "github:team/api" {
		t.Fatal(one, err)
	}
	two, err := m.AddLocalRepository(context.Background(), second.Path, "")
	if err != nil || two != one || len(m.Repositories()) != 1 {
		t.Fatal("clone registered twice", two, err)
	}
	imported, err := m.RegisterRepository(context.Background(), Repository{ID: "github-42", Name: "api", Path: second.Path, SourceURL: "https://github.com/team/api", GitHubID: 42})
	if err != nil || imported != one {
		t.Fatal("GitHub import duplicated local source", imported, err)
	}
	c, err := m.Create(context.Background(), "logical-repo-workspace", Spec{Name: "my-feature", Members: []MemberSpec{{RepositoryID: one.ID}}})
	if err != nil || c.Members[0].Name != "api" {
		t.Fatal(c, err)
	}
	m.Close()
	m = newTest(t, root)
	if len(m.Repositories()) != 1 || m.Repositories()[0] != one || len(m.List()) != 1 {
		t.Fatal("identity or workspace lost across restart")
	}
}

func TestLegacyGitHubAliasesStayResolvableButListOnce(t *testing.T) {
	first, second := repoTest(t), repoTest(t)
	first.ID, second.ID = "old-one", "old-two"
	gitTest(t, first.Path, "remote", "add", "origin", "https://github.com/team/api.git")
	gitTest(t, second.Path, "remote", "add", "origin", "git@github.com:team/api.git")
	m := newTest(t, t.TempDir(), first, second)
	if len(m.Repositories()) != 1 || m.RepositoryIdentity(first.ID) != m.RepositoryIdentity(second.ID) {
		t.Fatal("legacy aliases exposed as different repositories")
	}
	for _, r := range []Repository{first, second} {
		c, err := m.Create(context.Background(), r.ID, Spec{Name: r.ID, Members: []MemberSpec{{RepositoryID: r.ID}}})
		if err != nil || c.State != "ready" {
			t.Fatal("legacy workspace reference broken", c, err)
		}
	}
}

func TestRemoveLibraryRepositoryKeepsWorktreeAndSurvivesRestart(t *testing.T) {
	ctx := context.Background()
	repo := repoTest(t)
	root := t.TempDir()
	m := newTest(t, root, repo)
	c, err := m.Create(ctx, "existing-worktree", specTest(repo.ID))
	if err != nil {
		t.Fatal(err)
	}
	if err = m.RemoveRepository(repo.ID); err != nil {
		t.Fatal(err)
	}
	if len(m.Repositories()) != 0 {
		t.Fatal("still listed")
	}
	if _, err = os.Stat(filepath.Join(c.Members[0].Path, "source.txt")); err != nil {
		t.Fatal("worktree damaged", err)
	}
	if err = m.Close(); err != nil {
		t.Fatal(err)
	}
	m = newTest(t, root, repo)
	if len(m.Repositories()) != 0 {
		t.Fatal("configured entry reappeared")
	}
	if _, err = m.BeginDeletion(ctx, c.ID, c.Name, false, false, func(string) bool { return false }); err != nil {
		t.Fatal(err)
	}
	if _, err = m.FinishDeletion(ctx, c.ID, false, func(string) bool { return false }); err != nil {
		t.Fatal("hidden source broke cleanup", err)
	}
	if _, err = os.Stat(filepath.Join(repo.Path, "source.txt")); err != nil {
		t.Fatal("source deleted", err)
	}
	if _, err = m.RegisterRepository(ctx, repo); err != nil {
		t.Fatal("cannot explicitly re-add", err)
	}
	if len(m.Repositories()) != 1 {
		t.Fatal("re-add not listed")
	}
}
