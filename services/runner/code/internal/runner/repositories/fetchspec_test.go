package repositories

import (
	"context"
	"os"
	"path/filepath"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

func commitAll(t *testing.T, dir, message string) {
	t.Helper()
	gitTest(t, dir, "-c", "user.name=Agent", "-c", "user.email=agent@example.test", "commit", "-q", "--allow-empty", "-m", message)
}

// importFixture imports a local repository standing in for GitHub (origin is
// its path, so a push really lands there) and returns the library.
func importFixture(t *testing.T) (*Importer, *collections.Manager, collections.Repository, string) {
	t.Helper()
	t.Setenv("GIT_CONFIG_GLOBAL", os.DevNull)
	t.Setenv("GIT_CONFIG_NOSYSTEM", "1")
	remote := t.TempDir()
	gitTest(t, remote, "init", "-q", "-b", "main")
	commitAll(t, remote, "initial")
	library, err := collections.New(t.TempDir(), nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { library.Close() })
	importer, err := New(t.TempDir(), library, &localClone{source: remote})
	if err != nil {
		t.Fatal(err)
	}
	source, err := importer.Import(context.Background(), repoFixture(), "fake-token")
	if err != nil {
		t.Fatal(err)
	}
	return importer, library, source, remote
}

// An agent's own `git push -u` from a workspace worktree records the branch
// as pushed, so the sidebar's summary and the PR loop see it.
func TestAgentPushFromAWorkspaceRecordsItsRemoteTrackingRef(t *testing.T) {
	ctx := context.Background()
	_, library, source, remote := importFixture(t)
	if got := gitTest(t, source.Path, "config", "--get-all", "remote.origin.fetch"); got != OriginFetch {
		t.Fatalf("imported origin fetch refspec: %q", got)
	}
	if out := gitTest(t, source.Path, "for-each-ref", "refs/remotes"); out != "" {
		t.Fatalf("importing fetched: %s", out)
	}
	workspace, err := library.Create(ctx, "work", collections.Spec{Name: "hyena", Members: []collections.MemberSpec{{RepositoryID: source.ID}}})
	if err != nil {
		t.Fatal(err)
	}
	member := workspace.Members[0]
	commitAll(t, member.Path, "the agent's change")
	gitTest(t, member.Path, "push", "-q", "-u", "origin", member.Branch)
	if gitTest(t, remote, "rev-parse", "refs/heads/"+member.Branch) != gitTest(t, member.Path, "rev-parse", "HEAD") {
		t.Fatal("the push did not land")
	}
	gitTest(t, member.Path, "rev-parse", "--verify", "refs/remotes/origin/"+member.Branch)

	summary, err := gitinspect.Git{}.Summary(ctx, member.Path, member.BaseCommit)
	if err != nil || summary.Upstream != "origin/"+member.Branch || summary.Ahead != 0 || summary.Behind != 0 {
		t.Fatalf("summary: %+v %v", summary, err)
	}
	tracking, err := gitpublish.Git{}.Track(ctx, member.Path, member.BaseCommit)
	if err != nil || tracking.Upstream != "origin/"+member.Branch || tracking.Ahead != 0 {
		t.Fatalf("PR loop: %+v %v", tracking, err)
	}
	// A commit not pushed yet counts as ahead, as for any clone.
	commitAll(t, member.Path, "more")
	if summary, _ = (gitinspect.Git{}).Summary(ctx, member.Path, member.BaseCommit); summary.Ahead != 1 {
		t.Fatalf("ahead: %+v", summary)
	}
	if tracking, _ = (gitpublish.Git{}).Track(ctx, member.Path, member.BaseCommit); tracking.Ahead != 1 {
		t.Fatalf("PR loop ahead: %+v", tracking)
	}
}

// Source copies imported before keep everything and gain only the refspec,
// once, with nothing fetched; nothing else is touched.
func TestStartMigratesSourceCopiesWithoutFetching(t *testing.T) {
	ctx := context.Background()
	importer, library, source, remote := importFixture(t)
	// As imported before: no refspec.
	gitTest(t, source.Path, "config", "--unset-all", "remote.origin.fetch")
	before, err := os.ReadFile(filepath.Join(source.Path, "config"))
	if err != nil {
		t.Fatal(err)
	}
	// A branch pushed meanwhile on origin is not fetched by the migration.
	gitTest(t, remote, "branch", "elsewhere")

	// The person's own checkout, registered by path, is never touched.
	own := t.TempDir()
	gitTest(t, own, "init", "-q", "-b", "main")
	commitAll(t, own, "mine")
	gitTest(t, own, "remote", "add", "origin", remote)
	gitTest(t, own, "config", "--unset-all", "remote.origin.fetch")
	if _, err = library.RegisterRepository(ctx, collections.Repository{ID: "mine", Name: "mine", Path: own}); err != nil {
		t.Fatal(err)
	}
	changed, err := importer.MigrateOriginFetch(ctx)
	if err != nil || changed != 1 {
		t.Fatalf("migration: %d %v", changed, err)
	}
	after, _ := os.ReadFile(filepath.Join(source.Path, "config"))
	if want := string(before) + "\tfetch = " + OriginFetch + "\n"; string(after) != want {
		t.Fatalf("config changed beyond the refspec:\n%s\nwant:\n%s", after, want)
	}
	if out := gitTest(t, source.Path, "for-each-ref", "refs/remotes"); out != "" {
		t.Fatalf("the migration fetched: %s", out)
	}
	if _, err = os.Stat(filepath.Join(source.Path, "FETCH_HEAD")); err == nil {
		t.Fatal("the migration fetched")
	}
	if out, _ := localGit(ctx, filepath.Join(own, ".git"), "config", "--get-all", "remote.origin.fetch"); out != "" {
		t.Fatalf("the person's checkout was changed: %q", out)
	}
	// Idempotent.
	if changed, err = importer.MigrateOriginFetch(ctx); err != nil || changed != 0 {
		t.Fatalf("second migration: %d %v", changed, err)
	}
	if again, _ := os.ReadFile(filepath.Join(source.Path, "config")); string(again) != string(after) {
		t.Fatal("the second migration changed the config")
	}
	// A custom refspec is kept as it is.
	gitTest(t, source.Path, "config", "--replace-all", "remote.origin.fetch", "+refs/heads/main:refs/remotes/origin/main")
	if added, err := EnsureOriginFetch(ctx, source.Path); err != nil || added {
		t.Fatalf("custom refspec: %v %v", added, err)
	}
	if got := gitTest(t, source.Path, "config", "--get-all", "remote.origin.fetch"); got != "+refs/heads/main:refs/remotes/origin/main" {
		t.Fatalf("custom refspec replaced: %q", got)
	}
	// No origin: nothing to do.
	bare := filepath.Join(t.TempDir(), "plain.git")
	gitTest(t, t.TempDir(), "init", "-q", "--bare", bare)
	if added, err := EnsureOriginFetch(ctx, bare); err != nil || added {
		t.Fatalf("no origin: %v %v", added, err)
	}
}
