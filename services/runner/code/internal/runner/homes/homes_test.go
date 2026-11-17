package homes

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func run(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"}, args...)...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// source builds what the Runner keeps for a GitHub repository: a bare copy
// whose origin is the real remote, with the remote's view in
// refs/remotes/origin/* and an agent's workspace branch among its own heads.
func source(t *testing.T) (bare, upstreamHead string) {
	t.Helper()
	root := t.TempDir()
	upstream := filepath.Join(root, "upstream")
	run(t, root, "init", "-b", "main", upstream)
	if err := os.WriteFile(filepath.Join(upstream, "README.md"), []byte("api\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, upstream, "add", ".")
	run(t, upstream, "commit", "-m", "initial")
	bare = filepath.Join(root, "source.git")
	run(t, root, "clone", "--bare", upstream, bare)
	run(t, bare, "config", "remote.origin.url", "https://github.com/acme/api.git")
	run(t, bare, "config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
	// The remote moved on since the copy's own main: origin/main is newer.
	if err := os.WriteFile(filepath.Join(upstream, "CHANGELOG.md"), []byte("v2\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, upstream, "add", ".")
	run(t, upstream, "commit", "-m", "second")
	run(t, bare, "fetch", upstream, "+refs/heads/main:refs/remotes/origin/main")
	upstreamHead = run(t, upstream, "rev-parse", "HEAD")
	run(t, bare, "branch", "feature/gently-bright-otter", "main")
	return bare, upstreamHead
}

func TestCreateMakesAnIndependentCheckoutOfTheRemotesDefaultBranch(t *testing.T) {
	ctx := context.Background()
	bare, head := source(t)
	dest := filepath.Join(t.TempDir(), "repos", "acme", "api")
	if err := Create(ctx, bare, dest); err != nil {
		t.Fatal(err)
	}
	if got := run(t, dest, "rev-parse", "--abbrev-ref", "HEAD"); got != "main" {
		t.Fatalf("branch %q", got)
	}
	if got := run(t, dest, "rev-parse", "HEAD"); got != head {
		t.Fatal("the home starts at the remote's main, not the source copy's older one")
	}
	if got := run(t, dest, "rev-parse", "--abbrev-ref", "main@{upstream}"); got != "origin/main" {
		t.Fatalf("upstream %q", got)
	}
	if got := run(t, dest, "config", "--get", "remote.origin.url"); got != "https://github.com/acme/api.git" {
		t.Fatalf("origin is the real remote, not the private copy: %q", got)
	}
	remotes := strings.Fields(run(t, dest, "for-each-ref", "--format=%(refname)", "refs/remotes/origin/"))
	if slices.Contains(remotes, "refs/remotes/origin/feature/gently-bright-otter") || !slices.Contains(remotes, "refs/remotes/origin/main") {
		t.Fatalf("an agent's workspace branch must not look like a remote branch: %v", remotes)
	}
	if got := run(t, dest, "branch", "--format=%(refname:short)"); got != "main" {
		t.Fatalf("local branches %q", got)
	}
	if got := run(t, dest, "rev-parse", "--git-common-dir"); got != ".git" {
		t.Fatalf("a home is a regular clone, not a worktree: %q", got)
	}
	if _, err := os.Stat(filepath.Join(dest, "CHANGELOG.md")); err != nil {
		t.Fatal("the working tree is checked out")
	}
	if s := Inspect(ctx, dest); !s.Exists || s.Branch != "main" || s.Dirty != 0 || s.Head != head[:7] {
		t.Fatalf("inspect: %+v", s)
	}
	// Nothing borrowed: the home survives its source copy.
	if err := os.RemoveAll(bare); err != nil {
		t.Fatal(err)
	}
	run(t, dest, "fsck", "--no-dangling")
	if leftovers, _ := filepath.Glob(filepath.Join(filepath.Dir(dest), ".lazybox-home-*")); len(leftovers) != 0 {
		t.Fatalf("staging left behind: %v", leftovers)
	}
}

func TestCreateNeverOverwritesAFolder(t *testing.T) {
	bare, _ := source(t)
	dest := t.TempDir()
	marker := filepath.Join(dest, "mine.txt")
	if err := os.WriteFile(marker, []byte("keep"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := Create(context.Background(), bare, dest); err == nil || !strings.Contains(err.Error(), "already exists") {
		t.Fatalf("an existing folder must be refused: %v", err)
	}
	if data, err := os.ReadFile(marker); err != nil || string(data) != "keep" {
		t.Fatal("the existing folder was touched")
	}
}

func TestInspectCountsChangesWithoutTouchingTheIndex(t *testing.T) {
	ctx := context.Background()
	bare, _ := source(t)
	dest := filepath.Join(t.TempDir(), "api")
	if err := Create(ctx, bare, dest); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dest, "README.md"), []byte("edited\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dest, "notes.txt"), []byte("new\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	index := filepath.Join(dest, ".git", "index")
	before, _ := os.Stat(index)
	if s := Inspect(ctx, dest); s.Dirty != 2 || s.Branch != "main" {
		t.Fatalf("inspect: %+v", s)
	}
	if after, _ := os.Stat(index); !after.ModTime().Equal(before.ModTime()) {
		t.Fatal("inspecting a home must not write the person's index")
	}
	run(t, dest, "checkout", "--quiet", "--detach")
	if s := Inspect(ctx, dest); !s.Detached || s.Branch != "" {
		t.Fatalf("detached: %+v", s)
	}
	if s := Inspect(ctx, filepath.Join(dest, "missing")); s.Exists || s.Error == "" {
		t.Fatalf("missing: %+v", s)
	}
	if s := Inspect(ctx, t.TempDir()); !s.Exists || s.Error != "Not a Git checkout" {
		t.Fatalf("not git: %+v", s)
	}
}

func TestIdentityNamesTheRepositoryOfAnyCheckout(t *testing.T) {
	ctx := context.Background()
	bare, _ := source(t)
	dest := filepath.Join(t.TempDir(), "api")
	if err := Create(ctx, bare, dest); err != nil {
		t.Fatal(err)
	}
	sub := filepath.Join(dest, "docs")
	if err := os.Mkdir(sub, 0o755); err != nil {
		t.Fatal(err)
	}
	identity, top, err := Identity(ctx, sub)
	resolved, _ := filepath.EvalSymlinks(dest)
	if err != nil || identity != "github:acme/api" || top != resolved {
		t.Fatalf("identity %q top %q err %v", identity, top, err)
	}
	if _, _, err = Identity(ctx, t.TempDir()); err == nil {
		t.Fatal("a plain folder has no identity")
	}
}

func TestSuggestAndOpenCommands(t *testing.T) {
	if got := Suggest("/u/lazybox/repos", "https://github.com/Acme/API", "API"); got != "/u/lazybox/repos/acme/api" {
		t.Fatal(got)
	}
	if got := Suggest("/u/lazybox/repos", "", "tools"); got != "/u/lazybox/repos/tools" {
		t.Fatal(got)
	}
	cases := []struct {
		goos, app string
		want      []string
	}{
		{"darwin", "finder", []string{"open", "/h"}},
		{"darwin", "vscode", []string{"open", "-a", "Visual Studio Code", "/h"}},
		{"linux", "finder", []string{"xdg-open", "/h"}},
		{"linux", "zed", []string{"zed", "/h"}},
	}
	for _, c := range cases {
		got, err := OpenCommand(c.goos, "/h", c.app)
		if err != nil || !slices.Equal(got, c.want) {
			t.Fatalf("%s %s: %v %v", c.goos, c.app, got, err)
		}
	}
	if _, err := OpenCommand("darwin", "/h", "sh -c 'rm -rf ~'"); err == nil {
		t.Fatal("only known apps open")
	}
}

// Update to latest main: fetch, then fast-forward only a clean default
// branch; anything else is said and left exactly as it was.
func TestUpdateFastForwardsOnlyACleanDefaultBranch(t *testing.T) {
	ctx := context.Background()
	bare, _ := source(t)
	upstream := filepath.Join(filepath.Dir(bare), "upstream")
	dest := filepath.Join(t.TempDir(), "api")
	if err := Create(ctx, bare, dest); err != nil {
		t.Fatal(err)
	}
	// Point origin at the local upstream so the test fetches offline.
	run(t, dest, "remote", "set-url", "origin", upstream)
	commit := func(name string) string {
		if err := os.WriteFile(filepath.Join(upstream, name), []byte(name+"\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		run(t, upstream, "add", ".")
		run(t, upstream, "commit", "-m", name)
		return run(t, upstream, "rev-parse", "HEAD")
	}
	if updated, message, err := Update(ctx, dest); err != nil || updated || !strings.HasPrefix(message, "Already at the latest origin/main") {
		t.Fatalf("current: %v %q %v", updated, message, err)
	}
	head := commit("third.txt")
	updated, message, err := Update(ctx, dest)
	if err != nil || !updated || !strings.HasPrefix(message, "Updated main to the latest origin/main") || run(t, dest, "rev-parse", "HEAD") != head {
		t.Fatalf("behind: %v %q %v", updated, message, err)
	}

	// A change of the person's: fetched (Behind says so), never touched.
	commit("fourth.txt")
	if err = os.WriteFile(filepath.Join(dest, "README.md"), []byte("mine\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if updated, message, err = Update(ctx, dest); err != nil || updated || !strings.Contains(message, "uncommitted changes") {
		t.Fatalf("dirty: %v %q %v", updated, message, err)
	}
	if data, _ := os.ReadFile(filepath.Join(dest, "README.md")); string(data) != "mine\n" {
		t.Fatal("the person's change was discarded")
	}
	if s := Inspect(ctx, dest); s.Behind != 1 || s.DefaultBranch != "main" {
		t.Fatalf("behind after fetch: %+v", s)
	}
	run(t, dest, "checkout", "--quiet", "--", "README.md")

	// Another branch: not switched.
	run(t, dest, "checkout", "--quiet", "-b", "experiment")
	if updated, message, err = Update(ctx, dest); err != nil || updated || !strings.Contains(message, "on experiment, not main") {
		t.Fatalf("other branch: %v %q %v", updated, message, err)
	}
	if got := run(t, dest, "rev-parse", "--abbrev-ref", "HEAD"); got != "experiment" {
		t.Fatal("the branch was switched")
	}
	run(t, dest, "checkout", "--quiet", "main")

	// Commits of the person's own on main: not merged, not reset.
	if err = os.WriteFile(filepath.Join(dest, "local.txt"), []byte("x\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	run(t, dest, "add", ".")
	run(t, dest, "commit", "-m", "mine")
	mine := run(t, dest, "rev-parse", "HEAD")
	if updated, message, err = Update(ctx, dest); err != nil || updated || !strings.Contains(message, "1 commit(s) that are not on origin/main") || run(t, dest, "rev-parse", "HEAD") != mine {
		t.Fatalf("diverged: %v %q %v", updated, message, err)
	}
}
