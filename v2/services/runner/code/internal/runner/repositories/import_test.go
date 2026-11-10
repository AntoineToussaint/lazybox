package repositories

import (
	"context"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/v2/internal/sources/github"
)

type localClone struct {
	source string
	calls  int
	fail   bool
}

func (c *localClone) Clone(ctx context.Context, source, destination, token string) error {
	c.calls++
	if c.fail {
		return errors.New("fixture clone failure")
	}
	if token != "fake-token" {
		return errors.New("missing scoped credential")
	}
	return exec.CommandContext(ctx, "git", "clone", "--bare", c.source, destination).Run()
}
func gitTest(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git: %s %v", out, err)
	}
	return strings.TrimSpace(string(out))
}
func repoFixture() github.Repository {
	return github.Repository{ID: 42, Name: "api", FullName: "team/api", CloneURL: "https://github.com/team/api.git", HTMLURL: "https://github.com/team/api", DefaultBranch: "main"}
}
func TestImportRetryRestartAndCollection(t *testing.T) {
	source := t.TempDir()
	gitTest(t, source, "init", "-b", "main")
	gitTest(t, source, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "--allow-empty", "-m", "initial")
	root := t.TempDir()
	library, err := collections.New(root, nil)
	if err != nil {
		t.Fatal(err)
	}
	cloner := &localClone{source: source}
	imports := t.TempDir()
	manager, err := New(imports, library, cloner)
	if err != nil {
		t.Fatal(err)
	}
	first, err := manager.Import(context.Background(), repoFixture(), "fake-token")
	if err != nil {
		t.Fatal(err)
	}
	second, err := manager.Import(context.Background(), repoFixture(), "fake-token")
	if err != nil || first != second || cloner.calls != 1 {
		t.Fatal("retry cloned again", err)
	}
	library.Close()
	library, err = collections.New(root, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer library.Close()
	if len(library.Repositories()) != 1 || library.Repositories()[0].Name != "api" {
		t.Fatal("library not restored")
	}
	collection, err := library.Create(context.Background(), "work", collections.Spec{Name: "my-feature", Members: []collections.MemberSpec{{RepositoryID: first.ID}}})
	if err != nil {
		t.Fatal(err)
	}
	if collection.Members[0].Name != "api" || collection.Members[0].Branch != "feature/"+collection.Slug {
		t.Fatal(collection)
	}
	if gitTest(t, source, "branch", "--show-current") != "main" {
		t.Fatal("source branch changed")
	}
	config, err := os.ReadFile(filepath.Join(first.Path, "config"))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(config), "fake-token") {
		t.Fatal("credential in repository config")
	}
}
func TestImportFailureAndUnknownDestinationPreserved(t *testing.T) {
	library, err := collections.New(t.TempDir(), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer library.Close()
	root := t.TempDir()
	clone := &localClone{fail: true}
	manager, _ := New(root, library, clone)
	if _, err = manager.Import(context.Background(), repoFixture(), "fake-token"); err == nil {
		t.Fatal("expected failure")
	}
	if len(library.Repositories()) != 0 {
		t.Fatal("registered failed clone")
	}
	destination := filepath.Join(root, "github-42")
	os.Mkdir(destination, 0700)
	os.WriteFile(filepath.Join(destination, "keep"), []byte("original"), 0600)
	if _, err = manager.Import(context.Background(), repoFixture(), "fake-token"); err == nil {
		t.Fatal("adopted unknown directory")
	}
	data, _ := os.ReadFile(filepath.Join(destination, "keep"))
	if string(data) != "original" {
		t.Fatal("overwrote existing data")
	}
	repo := repoFixture()
	repo.CloneURL = "https://evil.invalid/api.git"
	if err = Validate(repo); err == nil {
		t.Fatal("accepted foreign clone origin")
	}
}

func TestImportReusesRegisteredGitHubOriginWithoutCloning(t *testing.T) {
	source := t.TempDir()
	gitTest(t, source, "init", "-b", "main")
	gitTest(t, source, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "--allow-empty", "-m", "initial")
	gitTest(t, source, "remote", "add", "origin", "git@github.com:Team/API.git")
	library, err := collections.New(t.TempDir(), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer library.Close()
	local, err := library.AddLocalRepository(context.Background(), source, "")
	if err != nil {
		t.Fatal(err)
	}
	cloner := &localClone{fail: true}
	manager, err := New(t.TempDir(), library, cloner)
	if err != nil {
		t.Fatal(err)
	}
	got, err := manager.Import(context.Background(), repoFixture(), "fake-token")
	if err != nil || got != local || cloner.calls != 0 {
		t.Fatal("registered GitHub repository cloned again", got, err)
	}
}
func TestGitCloneDoesNotUseAmbientAuthOrPersistTokens(t *testing.T) {
	dir := t.TempDir()
	script := filepath.Join(dir, "git")
	report := filepath.Join(dir, "report")
	// This fake executable only checks how the real launcher scopes authentication.
	body := `#!/bin/sh
[ "$GIT_CONFIG_GLOBAL" = /dev/null ] || exit 1
[ "$GIT_CONFIG_NOSYSTEM" = 1 ] || exit 2
[ "$GIT_TERMINAL_PROMPT" = 0 ] || exit 3
[ -z "$GH_TOKEN" ] && [ -z "$GITHUB_TOKEN" ] && [ -z "$GIT_DIR" ] || exit 4
[ "$GIT_CONFIG_KEY_0" = http.https://github.com/.extraHeader ] || exit 5
printf '%s\n' "$@" > "$REPORT"
`
	os.WriteFile(script, []byte(body), 0700)
	t.Setenv("PATH", dir+":"+os.Getenv("PATH"))
	t.Setenv("REPORT", report)
	t.Setenv("GH_TOKEN", "ambient")
	t.Setenv("GITHUB_TOKEN", "ambient")
	t.Setenv("GIT_DIR", "wrong")
	if err := (Git{}).Clone(context.Background(), "https://github.com/team/api.git", filepath.Join(dir, "copy"), "private-token"); err != nil {
		t.Fatal(err)
	}
	args, _ := os.ReadFile(report)
	if strings.Contains(string(args), "private-token") || !strings.Contains(string(args), "credential.helper=") {
		t.Fatal("unsafe clone args")
	}
}

type failRegistration struct {
	Library
	fail bool
}

func (f *failRegistration) RegisterRepository(ctx context.Context, r collections.Repository) (collections.Repository, error) {
	if f.fail {
		return collections.Repository{}, errors.New("fixture registry failure")
	}
	return f.Library.RegisterRepository(ctx, r)
}
func TestCompletedCloneCanBeRegisteredAfterInterruptedCommit(t *testing.T) {
	source := t.TempDir()
	gitTest(t, source, "init", "-b", "main")
	gitTest(t, source, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "--allow-empty", "-m", "initial")
	library, err := collections.New(t.TempDir(), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer library.Close()
	failing := &failRegistration{Library: library, fail: true}
	clone := &localClone{source: source}
	root := t.TempDir()
	importer, _ := New(root, failing, clone)
	if _, err = importer.Import(context.Background(), repoFixture(), "fake-token"); err == nil {
		t.Fatal("expected registration failure")
	}
	failing.fail = false
	restarted, _ := New(root, failing, clone)
	if _, err = restarted.Import(context.Background(), repoFixture(), "fake-token"); err != nil {
		t.Fatal(err)
	}
	if clone.calls != 1 {
		t.Fatal("recloned retained source")
	}
}
