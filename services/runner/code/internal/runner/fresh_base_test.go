package runner

import (
	"context"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

func gitIn(t *testing.T, dir string, args ...string) string {
	t.Helper()
	out, err := exec.Command("git", append([]string{"-C", dir, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"}, args...)...).CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v\n%s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// upstreamOn is a repository standing in for origin, its default branch named
// branch (never assumed to be main).
func upstreamOn(t *testing.T, branch string) string {
	t.Helper()
	path := t.TempDir()
	gitIn(t, path, "init", "-b", branch)
	gitIn(t, path, "commit", "--allow-empty", "-m", "initial")
	return path
}

func advance(t *testing.T, upstream, message string) string {
	t.Helper()
	gitIn(t, upstream, "commit", "--allow-empty", "-m", message)
	return gitIn(t, upstream, "rev-parse", "HEAD")
}

// sourceCopy is what the importer makes: a bare clone, here of a local origin.
func sourceCopy(t *testing.T, upstream string) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "source.git")
	if out, err := exec.Command("git", "clone", "--bare", "--quiet", upstream, path).CombinedOutput(); err != nil {
		t.Fatal(err, string(out))
	}
	return path
}

type progressLog struct {
	mu    sync.Mutex
	lines []string
}

func (p *progressLog) record(_ string, message string) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.lines = append(p.lines, message)
	return nil
}

func (p *progressLog) text() string {
	p.mu.Lock()
	defer p.mu.Unlock()
	return strings.Join(p.lines, "\n")
}

func freshBench(t *testing.T, repos ...collections.Repository) *Bench {
	t.Helper()
	t.Setenv("CLAUDE_CONFIG_DIR", t.TempDir())
	b, err := New(Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), WorktreeRoot: t.TempDir(), Repositories: repos})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	return b
}

func startsAt(t *testing.T, ws collections.Collection, name, commit string) {
	t.Helper()
	for _, m := range ws.Members {
		if m.Name != name {
			continue
		}
		if m.BaseCommit != commit {
			t.Fatalf("%s pinned base %s, want %s", name, m.BaseCommit, commit)
		}
		if head := gitIn(t, m.Path, "rev-parse", "HEAD"); head != commit {
			t.Fatalf("%s worktree starts at %s, want %s", name, head, commit)
		}
		return
	}
	t.Fatalf("workspace has no member %s: %+v", name, ws.Members)
}

// A source copy's default branch is stale and origin has moved on; the
// person's own checkout has a feature branch out. A new workspace, and a
// repository added to it later, start from origin's default branch as it is
// now, found from origin's HEAD (here "trunk", not main).
func TestNewWorkspaceStartsFromOriginsCurrentDefaultBranch(t *testing.T) {
	upstream := upstreamOn(t, "trunk")
	source := sourceCopy(t, upstream)
	checkout := t.TempDir()
	if out, err := exec.Command("git", "clone", "--quiet", upstream, checkout).CombinedOutput(); err != nil {
		t.Fatal(err, string(out))
	}
	gitIn(t, checkout, "switch", "--quiet", "-c", "feature/old-work")
	gitIn(t, checkout, "commit", "--allow-empty", "-m", "unrelated work")
	tools := sourceCopy(t, upstream)
	stale := gitIn(t, source, "rev-parse", "refs/heads/trunk")
	fresh := advance(t, upstream, "origin moved on")
	if fresh == stale {
		t.Fatal("fixture: origin did not advance")
	}

	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source}, collections.Repository{ID: "web", Name: "web", Path: checkout}, collections.Repository{ID: "tools", Name: "tools", Path: tools})
	var p progressLog
	ws, err := b.PrepareWorkspace(context.Background(), "fresh", "", "Fresh", "", []string{b.Collections.RepositoryIdentity("api"), b.Collections.RepositoryIdentity("web")}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api", fresh)
	startsAt(t, ws, "web", fresh)
	if strings.Contains(p.text(), "Couldn't update") {
		t.Fatalf("origin was reachable, yet: %s", p.text())
	}
	// The person's checkout keeps its branch; only origin's refs moved.
	if branch := gitIn(t, checkout, "symbolic-ref", "--short", "HEAD"); branch != "feature/old-work" {
		t.Fatalf("checkout switched to %s", branch)
	}

	// Add repository to the existing workspace after origin moved again.
	later := advance(t, upstream, "and again")
	ws, err = b.PrepareWorkspace(context.Background(), "add-tools", ws.ID, "", "", []string{b.Collections.RepositoryIdentity("tools")}, p.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "tools", later)
	startsAt(t, ws, "api", fresh) // existing worktrees are never moved
}

// Offline: the workspace is still created, from the last known default
// branch, and progress says so once. A copy fetched before uses that fetch
// (newer than its own clone-time branch); one never fetched uses its clone.
func TestOfflineWorkspaceStartsFromLastKnownDefaultBranch(t *testing.T) {
	upstream := upstreamOn(t, "main")
	source := sourceCopy(t, upstream)
	cloned := gitIn(t, source, "rev-parse", "refs/heads/main")
	never := sourceCopy(t, upstream)
	fetched := advance(t, upstream, "fetched once")

	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source}, collections.Repository{ID: "never", Name: "never", Path: never})
	var online progressLog
	ws, err := b.PrepareWorkspace(context.Background(), "online", "", "Online", "", []string{b.Collections.RepositoryIdentity("api")}, online.record)
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "api", fetched)

	advance(t, upstream, "not seen: offline")
	missing := filepath.Join(t.TempDir(), "gone.git")
	gitIn(t, source, "remote", "set-url", "origin", missing)
	gitIn(t, never, "remote", "set-url", "origin", missing)

	var offline progressLog
	ws, err = b.PrepareWorkspace(context.Background(), "offline", "", "Offline", "", []string{b.Collections.RepositoryIdentity("api"), b.Collections.RepositoryIdentity("never")}, offline.record)
	if err != nil {
		t.Fatalf("offline creation failed: %v", err)
	}
	startsAt(t, ws, "api", fetched)
	startsAt(t, ws, "never", cloned)
	for _, name := range []string{"api", "never"} {
		if want := "Couldn't update " + name + "; started from main as of "; strings.Count(offline.text(), want) != 1 {
			t.Fatalf("want one %q notice, got:\n%s", want, offline.text())
		}
	}
}

// An explicit base (a branch to continue, a PR head, an orchestrator's parent
// branch) is used as given, not replaced by origin's default branch.
func TestExplicitBaseIsHonoured(t *testing.T) {
	upstream := upstreamOn(t, "main")
	checkout := t.TempDir()
	if out, err := exec.Command("git", "clone", "--quiet", upstream, checkout).CombinedOutput(); err != nil {
		t.Fatal(err, string(out))
	}
	gitIn(t, checkout, "switch", "--quiet", "-c", "feature/continue")
	feature := advance(t, checkout, "work to continue")
	gitIn(t, checkout, "switch", "--quiet", "main")
	advance(t, upstream, "origin moved on")

	b := freshBench(t, collections.Repository{ID: "web", Name: "web", Path: checkout})
	spec := []collections.MemberSpec{{Name: "web", RepositoryID: "web", BaseRef: "feature/continue"}}
	b.RefreshBases(context.Background(), spec)
	ws, err := b.Collections.Create(context.Background(), "explicit", collections.Spec{Name: "Continue", Members: spec})
	if err != nil {
		t.Fatal(err)
	}
	startsAt(t, ws, "web", feature)
}
