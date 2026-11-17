package gitpublish

import (
	"context"
	"encoding/base64"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

func git(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	cmd.Env = append(os.Environ(), "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// fixture is a source repository with origin at a local bare remote and two
// linked worktrees on their own branches, as a workspace creates them.
func fixture(t *testing.T) (remote, source, one, two string) {
	t.Helper()
	t.Setenv("GIT_CONFIG_GLOBAL", os.DevNull)
	t.Setenv("GIT_CONFIG_NOSYSTEM", "1")
	root := t.TempDir()
	remote, source = filepath.Join(root, "remote.git"), filepath.Join(root, "source")
	git(t, root, "init", "--bare", "-b", "main", remote)
	git(t, root, "init", "-b", "main", source)
	for _, kv := range [][2]string{{"user.name", "Fixture"}, {"user.email", "fixture@example.invalid"}} {
		git(t, source, "config", kv[0], kv[1])
	}
	if err := os.WriteFile(filepath.Join(source, "README.md"), []byte("hello\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	git(t, source, "add", ".")
	git(t, source, "commit", "-m", "initial")
	git(t, source, "remote", "add", "origin", remote)
	git(t, source, "push", "origin", "main")
	one, two = filepath.Join(root, "one"), filepath.Join(root, "two")
	git(t, source, "worktree", "add", "-b", "feature/one", one)
	git(t, source, "worktree", "add", "-b", "feature/two", two)
	return remote, source, one, two
}

func write(t *testing.T, path, text string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
		t.Fatal(err)
	}
}

func TestCommitStagesOnlyThisWorktreeAndRefusesEmptyCommits(t *testing.T) {
	ctx := context.Background()
	_, _, one, two := fixture(t)
	base := git(t, one, "rev-parse", "HEAD")
	write(t, filepath.Join(one, "new.txt"), "new\n")
	write(t, filepath.Join(one, "README.md"), "changed\n")
	write(t, filepath.Join(two, "other.txt"), "other worktree\n")
	g := Git{}
	if _, err := g.Commit(ctx, one, "   ", nil); err == nil {
		t.Fatal("empty message accepted")
	}
	if _, err := g.Commit(ctx, one, "Add", []string{"../two/other.txt"}); err == nil {
		t.Fatal("path outside the worktree accepted")
	}
	result, err := g.Commit(ctx, one, "Add the new file\n\nBecause.", nil)
	if err != nil {
		t.Fatal(err)
	}
	if result.Subject != "Add the new file" || result.Files != 2 || result.Branch != "feature/one" || result.Commit == base {
		t.Fatalf("%+v", result)
	}
	if body := git(t, one, "log", "-1", "--format=%B"); body != "Add the new file\n\nBecause." {
		t.Fatalf("message %q", body)
	}
	if status := git(t, one, "status", "--porcelain"); status != "" {
		t.Fatalf("worktree not clean: %q", status)
	}
	if status := git(t, two, "status", "--porcelain"); status != "?? other.txt" {
		t.Fatalf("the other worktree changed: %q", status)
	}
	if head := git(t, two, "rev-parse", "HEAD"); head != base {
		t.Fatal("the other worktree's branch moved")
	}
	if _, err = g.Commit(ctx, one, "Again", nil); !errors.Is(err, ErrNothingToCommit) {
		t.Fatalf("empty commit: %v", err)
	}
}

func TestCommitSelectedPathsLeavesTheRestUncommitted(t *testing.T) {
	ctx := context.Background()
	_, _, one, _ := fixture(t)
	write(t, filepath.Join(one, "a.txt"), "a\n")
	write(t, filepath.Join(one, "dir/b.txt"), "b\n")
	write(t, filepath.Join(one, "c.txt"), "c\n")
	git(t, one, "add", "c.txt") // staged by someone else; not selected
	if err := os.Remove(filepath.Join(one, "README.md")); err != nil {
		t.Fatal(err)
	}
	result, err := Git{}.Commit(ctx, one, "Pick two", []string{"a.txt", "README.md"})
	if err != nil {
		t.Fatal(err)
	}
	if result.Files != 2 {
		t.Fatalf("%+v", result)
	}
	if files := git(t, one, "show", "--name-status", "--format=", "HEAD"); files != "D\tREADME.md\nA\ta.txt" {
		t.Fatalf("committed %q", files)
	}
	if status := git(t, one, "status", "--porcelain"); status != "A  c.txt\n?? dir/" {
		t.Fatalf("unselected changes disturbed: %q", status)
	}
	if _, err = (Git{}).Commit(ctx, one, "Nothing", []string{"a.txt"}); !errors.Is(err, ErrNothingToCommit) {
		t.Fatalf("committed an unchanged path: %v", err)
	}
}

func TestCommitExplainsMissingIdentity(t *testing.T) {
	ctx := context.Background()
	_, source, one, _ := fixture(t)
	git(t, source, "config", "--unset", "user.name")
	git(t, source, "config", "--unset", "user.email")
	t.Setenv("EMAIL", "")
	t.Setenv("GIT_AUTHOR_NAME", "")
	write(t, filepath.Join(one, "a.txt"), "a\n")
	// An empty passwd gecos/hostname may still yield an identity; only check
	// that a refusal, when it happens, is explained.
	if _, err := (Git{}).Commit(ctx, one, "Add", nil); err != nil && !strings.Contains(err.Error(), "user.name") {
		t.Fatalf("identity failure not explained: %v", err)
	}
}

func TestPushSetsUpstreamTracksAheadAndNeverForces(t *testing.T) {
	ctx := context.Background()
	remote, source, one, two := fixture(t)
	g := Git{}
	base := git(t, one, "rev-parse", "HEAD")
	tracking, err := g.Track(ctx, one, base)
	if err != nil || tracking.Upstream != "" || tracking.Ahead != 0 || tracking.Branch != "feature/one" {
		t.Fatalf("%+v %v", tracking, err)
	}
	write(t, filepath.Join(one, "a.txt"), "a\n")
	if _, err = g.Commit(ctx, one, "One", nil); err != nil {
		t.Fatal(err)
	}
	if tracking, _ = g.Track(ctx, one, base); tracking.Ahead != 1 || tracking.Upstream != "" {
		t.Fatalf("never-pushed branch: %+v", tracking)
	}
	pushed, err := g.Push(ctx, one, "")
	if err != nil {
		t.Fatal(err)
	}
	if pushed.Branch != "feature/one" || pushed.Upstream != "origin/feature/one" || pushed.Head != git(t, remote, "rev-parse", "refs/heads/feature/one") {
		t.Fatalf("%+v", pushed)
	}
	if merge := git(t, source, "config", "branch.feature/one.merge"); merge != "refs/heads/feature/one" {
		t.Fatalf("upstream not set: %q", merge)
	}
	if tracking, _ = g.Track(ctx, one, base); tracking.Ahead != 0 || tracking.Upstream != "origin/feature/one" {
		t.Fatalf("after push: %+v", tracking)
	}
	if _, err = (Git{}).Origin(ctx, one); err != nil {
		t.Fatal(err)
	}
	// Only feature/one reached origin.
	if refs := git(t, remote, "for-each-ref", "--format=%(refname)"); refs != "refs/heads/feature/one\nrefs/heads/main" {
		t.Fatalf("remote refs %q", refs)
	}
	// Someone else pushes to the branch; a diverged push is refused, not forced.
	write(t, filepath.Join(two, "b.txt"), "b\n")
	git(t, two, "add", ".")
	git(t, two, "commit", "-m", "Other")
	git(t, two, "push", "origin", "HEAD:refs/heads/feature/one", "--force")
	theirs := git(t, remote, "rev-parse", "refs/heads/feature/one")
	write(t, filepath.Join(one, "c.txt"), "c\n")
	if _, err = g.Commit(ctx, one, "Mine", nil); err != nil {
		t.Fatal(err)
	}
	if _, err = g.Push(ctx, one, ""); err == nil || !strings.Contains(err.Error(), "never force-pushes") {
		t.Fatalf("diverged push: %v", err)
	}
	if git(t, remote, "rev-parse", "refs/heads/feature/one") != theirs {
		t.Fatal("the remote branch was overwritten")
	}
}

func TestPushRefusesDetachedHead(t *testing.T) {
	_, _, one, _ := fixture(t)
	git(t, one, "checkout", "--detach")
	if _, err := (Git{}).Push(context.Background(), one, ""); !errors.Is(err, ErrDetached) {
		t.Fatal(err)
	}
	if _, err := (Git{}).Commit(context.Background(), one, "x", nil); !errors.Is(err, ErrDetached) {
		t.Fatal(err)
	}
}

// A GitHub HTTPS origin gets the connection's credential as an HTTP header in
// the child's environment only: never in argv, config, the remote URL, or the
// error text. A fake git on PATH records what it was given.
func TestPushCredentialStaysInTheChildEnvironment(t *testing.T) {
	ctx := context.Background()
	_, source, one, _ := fixture(t)
	git(t, source, "remote", "set-url", "origin", "https://github.com/team/api.git")
	origin, err := (Git{}).Origin(ctx, one)
	if err != nil || origin.GitHub != "team/api" || !origin.HTTPS {
		t.Fatalf("%+v %v", origin, err)
	}
	if _, err = (Git{}).Push(ctx, one, ""); err == nil || !strings.Contains(err.Error(), "connect GitHub") {
		t.Fatalf("push without a connection: %v", err)
	}
	real, err := exec.LookPath("git")
	if err != nil {
		t.Fatal(err)
	}
	bin, record := t.TempDir(), filepath.Join(t.TempDir(), "record")
	script := `#!/bin/sh
for a in "$@"; do if [ "$a" = push ]; then
  printf '%s\n' "$*" > "` + record + `.argv"
  env > "` + record + `.env"
  echo "remote: rejected for ghp_secret_fixture" >&2
  exit 1
fi; done
exec "` + real + `" "$@"
`
	write(t, filepath.Join(bin, "git"), script)
	if err = os.Chmod(filepath.Join(bin, "git"), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))
	t.Setenv("GH_TOKEN", "ambient-should-not-leak")
	_, err = (Git{}).Push(ctx, one, "ghp_secret_fixture")
	if err == nil || strings.Contains(err.Error(), "ghp_secret_fixture") || !strings.Contains(err.Error(), "[redacted]") {
		t.Fatalf("push error must redact the credential: %v", err)
	}
	argv, _ := os.ReadFile(record + ".argv")
	env, _ := os.ReadFile(record + ".env")
	if strings.Contains(string(argv), "ghp_secret_fixture") || !strings.Contains(string(argv), "credential.helper=") {
		t.Fatalf("argv %q", argv)
	}
	header := "GIT_CONFIG_VALUE_0=Authorization: Basic " + base64.StdEncoding.EncodeToString([]byte("x-access-token:ghp_secret_fixture"))
	for _, want := range []string{header, "GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader", "GIT_CONFIG_GLOBAL=" + os.DevNull, "GIT_TERMINAL_PROMPT=0"} {
		if !strings.Contains(string(env), want+"\n") {
			t.Errorf("child environment lacks %q", want)
		}
	}
	if strings.Contains(string(env), "ambient-should-not-leak") {
		t.Error("ambient GitHub token reached git")
	}
	if config, _ := os.ReadFile(filepath.Join(source, ".git", "config")); strings.Contains(string(config), "ghp_secret_fixture") || strings.Contains(string(config), "extraHeader") {
		t.Fatal("credential written to Git config")
	}
}
