package gitpublish

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// DefaultBranch is origin's own default branch as the clone recorded it,
// whatever its name; Contains tells a commit on a local branch from one the
// repository does not have.
func TestDefaultBranchAndContains(t *testing.T) {
	ctx := context.Background()
	_, source, one, _ := fixture(t)
	if _, err := (Git{}).DefaultBranch(ctx, one); !errors.Is(err, ErrNoDefault) {
		t.Fatalf("no origin/HEAD recorded: %v", err)
	}
	git(t, source, "fetch", "-q", "origin")
	git(t, source, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main")
	git(t, source, "branch", "-m", "main", "production")
	git(t, source, "update-ref", "refs/remotes/origin/production", "refs/remotes/origin/main")
	git(t, source, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/production")
	if branch, err := (Git{}).DefaultBranch(ctx, one); err != nil || branch != "production" {
		t.Fatalf("default branch: %q %v", branch, err)
	}
	head := git(t, one, "rev-parse", "HEAD")
	if !(Git{}).Contains(ctx, one, "feature/one", head) {
		t.Fatal("the branch's own tip is on it")
	}
	if (Git{}).Contains(ctx, one, "feature/one", strings.Repeat("ab", 20)) {
		t.Fatal("a commit the repository does not have is on the branch")
	}
	if (Git{}).Contains(ctx, one, "feature/one", "HEAD") || (Git{}).Contains(ctx, one, "-x", head) {
		t.Fatal("only a full commit id on a branch name is asked about")
	}
}

// A credentialed push runs no hook: a pre-push hook the repository (or an
// agent) installed would inherit the child's environment, where the
// credential travels.
func TestCredentialedPushRunsNoRepositoryHook(t *testing.T) {
	ctx := context.Background()
	remote, source, one, _ := fixture(t)
	git(t, source, "remote", "set-url", "origin", "https://github.com/team/api.git")
	// The repository's own config sends the GitHub URL to the local remote,
	// so the push really happens here.
	git(t, source, "config", "url."+remote+".insteadOf", "https://github.com/team/api.git")
	record := filepath.Join(t.TempDir(), "hook-env")
	hooks := filepath.Join(source, ".git", "hooks")
	write(t, filepath.Join(hooks, "pre-push"), "#!/bin/sh\nenv > '"+record+"'\n")
	if err := os.Chmod(filepath.Join(hooks, "pre-push"), 0o700); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(one, "change.txt"), "change\n")
	git(t, one, "add", ".")
	git(t, one, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-q", "-m", "change")
	if _, err := (Git{}).Push(ctx, one, "ghp_secret_fixture"); err != nil {
		t.Fatalf("push: %v", err)
	}
	if got := git(t, remote, "rev-parse", "refs/heads/feature/one"); got != git(t, one, "rev-parse", "HEAD") {
		t.Fatalf("the push did not reach the remote: %s", got)
	}
	if env, err := os.ReadFile(record); err == nil {
		leaked := strings.Contains(string(env), "x-access-token") || strings.Contains(string(env), "GIT_CONFIG_VALUE_0")
		t.Fatalf("the repository's pre-push hook ran (credential in its environment: %v)", leaked)
	}
	// Without a credential, the person's own Git setup runs their hooks.
	git(t, source, "remote", "set-url", "origin", remote)
	write(t, filepath.Join(one, "more.txt"), "more\n")
	git(t, one, "add", ".")
	git(t, one, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-q", "-m", "more")
	if _, err := (Git{}).Push(ctx, one, ""); err != nil {
		t.Fatalf("push: %v", err)
	}
	if _, err := os.Stat(record); err != nil {
		t.Fatal("an uncredentialed push skipped the person's hook")
	}
}
