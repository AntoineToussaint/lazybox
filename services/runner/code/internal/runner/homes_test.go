package runner

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

func homeGit(t *testing.T, dir string, args ...string) string {
	t.Helper()
	out, err := exec.Command("git", append([]string{"-C", dir, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"}, args...)...).CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// homeBench is a Runner with a GitHub-style repository: a bare source copy
// whose origin is the real remote, as an import leaves it.
func homeBench(t *testing.T) (*Bench, collections.Repository) {
	t.Helper()
	t.Setenv("SHELL", "/bin/sh")
	root := t.TempDir()
	upstream := filepath.Join(root, "upstream")
	homeGit(t, root, "init", "-b", "main", upstream)
	if err := os.WriteFile(filepath.Join(upstream, "Makefile"), []byte("playground:\n\techo up\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	homeGit(t, upstream, "add", ".")
	homeGit(t, upstream, "commit", "-m", "initial")
	bare := filepath.Join(root, "source.git")
	homeGit(t, root, "clone", "--bare", upstream, bare)
	homeGit(t, bare, "config", "remote.origin.url", "https://github.com/acme/api.git")
	b, err := New(Config{State: t.TempDir(), WorktreeRoot: t.TempDir(), HomeCheckoutRoot: filepath.Join(t.TempDir(), "repos")})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	repo, err := b.Collections.RegisterRepository(context.Background(), collections.Repository{ID: "github-7", Name: "api", Path: bare, SourceURL: "https://github.com/acme/api", GitHubID: 7})
	if err != nil {
		t.Fatal(err)
	}
	return b, repo
}

func TestAHomeIsCreatedWhereSuggestedAndSurvivesARestart(t *testing.T) {
	ctx := context.Background()
	b, repo := homeBench(t)
	list, err := b.RepositoryHomes(ctx, nil)
	if err != nil || len(list) != 1 || list[0].Path != "" || list[0].Name != "acme/api" {
		t.Fatalf("a bare source is nobody's home: %+v %v", list, err)
	}
	want := filepath.Join(b.Config.HomeCheckoutRoot, "acme", "api")
	if list[0].Suggested != want {
		t.Fatalf("suggested %q", list[0].Suggested)
	}
	home, err := b.SetRepositoryHome(ctx, repo.ID, "create", "")
	if err != nil {
		t.Fatal(err)
	}
	resolved, _ := filepath.EvalSymlinks(want)
	if home.Path != resolved || home.Kind != collections.HomeConfigured || !home.Exists || home.Branch != "main" || home.Dirty != 0 {
		t.Fatalf("created: %+v", home)
	}
	// The library keeps it: a new Runner on the same folders knows the home.
	cfg := b.Config
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	again, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if path, kind := again.Collections.Home(repo.ID); path != resolved || kind != collections.HomeConfigured {
		t.Fatalf("after restart: %q %q", path, kind)
	}
	// Forgetting it changes no file.
	if _, err = again.SetRepositoryHome(ctx, repo.ID, "clear", ""); err != nil {
		t.Fatal(err)
	}
	if path, _ := again.Collections.Home(repo.ID); path != "" {
		t.Fatal("forgotten")
	}
	if _, err = os.Stat(filepath.Join(resolved, "Makefile")); err != nil {
		t.Fatal("forgetting a home must leave its files")
	}
}

func TestAdoptingAFolderChecksItIsACheckoutOfThisRepository(t *testing.T) {
	ctx := context.Background()
	b, repo := homeBench(t)
	mine := filepath.Join(t.TempDir(), "my-api")
	homeGit(t, filepath.Dir(mine), "clone", "--quiet", repo.Path, mine)
	homeGit(t, mine, "remote", "set-url", "origin", "git@github.com:acme/api.git")
	other := t.TempDir()
	homeGit(t, other, "init", "-b", "main")
	homeGit(t, other, "remote", "add", "origin", "https://github.com/acme/web.git")
	if _, err := b.SetRepositoryHome(ctx, repo.ID, "adopt", other); err == nil || !strings.Contains(err.Error(), "checkout of acme/web, not acme/api") {
		t.Fatalf("another repository's checkout: %v", err)
	}
	if err := os.Mkdir(filepath.Join(mine, "docs"), 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := b.SetRepositoryHome(ctx, repo.ID, "adopt", filepath.Join(mine, "docs")); err == nil || !strings.Contains(err.Error(), "top folder") {
		t.Fatalf("a subfolder: %v", err)
	}
	inside := filepath.Join(b.Collections.WorktreeRoot(), "sneaky")
	if _, err := b.SetRepositoryHome(ctx, repo.ID, "create", inside); err == nil || !strings.Contains(err.Error(), "Lazybox's own folders") {
		t.Fatalf("a home inside the workspaces folder: %v", err)
	}
	if _, err := b.SetRepositoryHome(ctx, repo.ID, "create", mine); err == nil || !strings.Contains(err.Error(), "already exists") {
		t.Fatalf("create never overwrites: %v", err)
	}
	home, err := b.SetRepositoryHome(ctx, repo.ID, "adopt", mine)
	resolved, _ := filepath.EvalSymlinks(mine)
	if err != nil || home.Path != resolved || home.Branch != "main" {
		t.Fatalf("adopted: %+v %v", home, err)
	}
}

// A local repository registered from the person's own checkout already has
// a home: that checkout.
func TestARegisteredCheckoutIsItsOwnHome(t *testing.T) {
	t.Setenv("SHELL", "/bin/sh")
	mine := t.TempDir()
	homeGit(t, mine, "init", "-b", "main")
	homeGit(t, mine, "commit", "--allow-empty", "-m", "initial")
	b, err := New(Config{State: t.TempDir(), WorktreeRoot: t.TempDir(), HomeCheckoutRoot: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	repo, err := b.Collections.AddLocalRepository(context.Background(), mine, "tools")
	if err != nil {
		t.Fatal(err)
	}
	path, kind := b.Collections.Home(repo.ID)
	if path != repo.Path || kind != collections.HomeSource {
		t.Fatalf("home %q %q", path, kind)
	}
}

// The person's checkout is theirs: creating a workspace from the repository,
// committing in it and deleting it leave the home's branch, HEAD, files and
// refs exactly as they were.
func TestAWorkspaceNeverTouchesTheHome(t *testing.T) {
	ctx := context.Background()
	b, repo := homeBench(t)
	home, err := b.SetRepositoryHome(ctx, repo.ID, "create", "")
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(home.Path, "local.env"), []byte("TOKEN=mine\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	snapshot := func() string {
		return strings.Join([]string{
			homeGit(t, home.Path, "rev-parse", "--abbrev-ref", "HEAD"),
			homeGit(t, home.Path, "rev-parse", "HEAD"),
			homeGit(t, home.Path, "status", "--porcelain"),
			homeGit(t, home.Path, "for-each-ref"),
			homeGit(t, home.Path, "worktree", "list", "--porcelain"),
		}, "\n")
	}
	before := snapshot()
	c, err := b.Collections.Create(ctx, "home-untouched", collections.Spec{Name: "Login fix", Members: []collections.MemberSpec{{Name: "api", RepositoryID: repo.ID}}})
	if err != nil {
		t.Fatal(err)
	}
	member := c.Members[0].Path
	if strings.HasPrefix(member, home.Path) {
		t.Fatal("a workspace member is never the home")
	}
	if err = os.WriteFile(filepath.Join(member, "fix.go"), []byte("package api\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	homeGit(t, member, "add", ".")
	homeGit(t, member, "commit", "-m", "agent work")
	// A conversation started in the workspace is told where the home is.
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenCollection("codex-default", c.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	if context := fake.Session.Open.Context; !strings.Contains(context, "Home checkouts (the person's own clones") || !strings.Contains(context, "- api: "+home.Path) {
		t.Fatalf("the conversation's context lacks the home:\n%s", context)
	}
	if err = b.StopExecution(ctx, e.ID); err != nil {
		t.Fatal(err)
	}
	members := b.workspaceMembers(c)
	if len(members) != 1 || members[0].Home != home.Path || members[0].RepositoryID != repo.ID {
		t.Fatalf("lazybox workspace repos names the home: %+v", members)
	}
	ctx2, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	if _, err = b.DeleteWorkspace(ctx2, c.ID, c.Name, true, true, false); err != nil {
		t.Fatal(err)
	}
	if after := snapshot(); after != before {
		t.Fatalf("the workspace changed the home:\nbefore %s\nafter  %s", before, after)
	}
	if data, err := os.ReadFile(filepath.Join(home.Path, "local.env")); err != nil || string(data) != "TOKEN=mine\n" {
		t.Fatal("the home's own files are untouched")
	}
}

func TestATerminalOpensInTheHomeOrTheHomeFolder(t *testing.T) {
	ctx := context.Background()
	b, repo := homeBench(t)
	t.Setenv("HOME", t.TempDir())
	c, err := b.Collections.Create(ctx, "home-terminal", collections.Spec{Name: "Shell"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = b.OpenPlaceTerminal(c.ID, repo.ID, false, 80, 24, ""); err == nil || !strings.Contains(err.Error(), "no home checkout yet") {
		t.Fatalf("no home yet: %v", err)
	}
	home, err := b.SetRepositoryHome(ctx, repo.ID, "create", "")
	if err != nil {
		t.Fatal(err)
	}
	term, err := b.OpenPlaceTerminal(c.ID, repo.ID, false, 80, 24, "")
	if err != nil {
		t.Fatal(err)
	}
	s := term.Status()
	if s.Spec.Cwd != home.Path || s.Spec.Place != "repository-home" || s.Spec.RepositoryID != repo.ID || s.Spec.Title != "api home" || s.Spec.CollectionID != c.ID {
		t.Fatalf("home terminal: %+v", s.Spec)
	}
	user, err := b.OpenPlaceTerminal(c.ID, "", true, 80, 24, "")
	if err != nil {
		t.Fatal(err)
	}
	if got := user.Status().Spec; got.Cwd != os.Getenv("HOME") || got.Place != "user-home" {
		t.Fatalf("home folder terminal: %+v", got)
	}
	if _, err = b.OpenPlaceTerminal(c.ID, repo.ID, true, 80, 24, ""); err == nil {
		t.Fatal("one place at a time")
	}
}

func TestTheBriefingSaysAHomeIsThePersons(t *testing.T) {
	for _, want := range []string{"home checkout", "read-only for you", "never edit files, commit, reset, stash, pull, switch branches", "run in the home checkout of <repository>: `<command>`", "run in your home folder: `<command>`", "lazybox repo home <name>", "do not make temporary clones", "it has no TTY", "use the home-checkout or home-folder handoff below", "`env -u VAR …`", "--update`"} {
		if !strings.Contains(commandBriefing, want) {
			t.Fatalf("the briefing lacks %q", want)
		}
	}
}
