package transport

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/devices"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// A workspace row's git state follows the worktree through edit, commit,
// push and a pull request, from local Git and the pull request cache alone.
func TestWorkspaceGitSummariesFollowTheBranch(t *testing.T) {
	t.Setenv("GIT_CONFIG_GLOBAL", os.DevNull)
	t.Setenv("GIT_CONFIG_NOSYSTEM", "1")
	s, _ := collectionService(t)
	ctx := context.Background()
	source := s.Bench.Collections.Repositories()[0].Path
	remote := filepath.Join(t.TempDir(), "remote.git")
	runGit(t, source, "init", "--bare", "-b", "main", remote)
	runGit(t, source, "config", "user.name", "Fixture")
	runGit(t, source, "config", "user.email", "fixture@example.invalid")
	runGit(t, source, "remote", "add", "origin", remote)
	runGit(t, source, "push", "origin", "main")
	w, err := s.Bench.Collections.Create(ctx, "rows", collections.Spec{Name: "Rows", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	worktree := w.Members[0].Path
	// A new workspace knows when it was made, and says so on the wire.
	if age := time.Since(time.UnixMilli(wireCollection(w).CreatedAtUnixMillis)); age < 0 || age > time.Minute {
		t.Fatalf("created at: %d", wireCollection(w).CreatedAtUnixMillis)
	}
	summary := func() *pb.RepositoryGitSummary {
		t.Helper()
		list, err := s.ListWorkspaceGitSummaries(ctx, &pb.WorkspaceGitSummariesRequest{CollectionIds: []string{w.ID, "deleted-meanwhile", w.ID}})
		if err != nil || len(list.Workspaces) != 1 || list.Workspaces[0].CollectionId != w.ID || len(list.Workspaces[0].Repositories) != 1 {
			t.Fatal(list, err)
		}
		return list.Workspaces[0].Repositories[0]
	}
	row := summary()
	branch := row.Branch
	if branch == "" || row.Head == "" || row.Error != "" || row.Dirty != 0 || row.Upstream != "" || row.Ahead != 0 || row.DefaultBranch != "origin/main" || row.AheadOfDefault != 0 || row.BehindDefault != 0 || row.PullRequest != nil {
		t.Fatalf("fresh workspace: %+v", row)
	}

	// Two edits, one of them a new file: two uncommitted files.
	if err = os.WriteFile(filepath.Join(worktree, "handler.go"), []byte("package api\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(worktree, "notes.md"), []byte("notes\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if row = summary(); row.Dirty != 2 || row.Changes != nil {
		t.Fatalf("after edits (lines are counted only when asked): %+v", row)
	}
	// Asked for one workspace (the sidebar's hover card): its lines, against main.
	counted, err := s.ListWorkspaceGitSummaries(ctx, &pb.WorkspaceGitSummariesRequest{CollectionIds: []string{w.ID}, Changes: true})
	if err != nil || len(counted.Workspaces) != 1 {
		t.Fatal(counted, err)
	}
	if c := counted.Workspaces[0].Repositories[0].Changes; c == nil || c.Base != "origin/main" || c.Files != 2 || c.Additions != 0 || c.Deletions != 0 {
		t.Fatalf("two untracked files: %+v", c)
	}
	if _, err = s.CommitWorkspaceChanges(ctx, &pb.CommitWorkspaceChangesRequest{CollectionId: w.ID, Member: "api", Message: "Add the handler", Paths: []string{"handler.go"}}); err != nil {
		t.Fatal(err)
	}
	// Never pushed: ahead counts the commits since the workspace started.
	if row = summary(); row.Dirty != 1 || row.Ahead != 1 || row.Upstream != "" || row.AheadOfDefault != 1 {
		t.Fatalf("after commit: %+v", row)
	}
	// Committed and uncommitted together, like the pull request would show.
	if counted, err = s.ListWorkspaceGitSummaries(ctx, &pb.WorkspaceGitSummariesRequest{CollectionIds: []string{w.ID}, Changes: true}); err != nil {
		t.Fatal(err)
	}
	if c := counted.Workspaces[0].Repositories[0].Changes; c == nil || c.Files != 2 || c.Additions != 1 {
		t.Fatalf("after commit: %+v", c)
	}
	if _, err = s.PushWorkspaceBranch(ctx, &pb.PushWorkspaceBranchRequest{CollectionId: w.ID, Member: "api"}); err != nil {
		t.Fatal(err)
	}
	if row = summary(); row.Upstream != "origin/"+branch || row.Ahead != 0 || row.Behind != 0 || row.AheadOfDefault != 1 {
		t.Fatalf("after push: %+v", row)
	}
	// The default branch moves on: the workspace is behind it.
	other := t.TempDir()
	runGit(t, other, "clone", "-q", remote, ".")
	runGit(t, other, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-q", "--allow-empty", "-m", "elsewhere")
	runGit(t, other, "push", "-q", "origin", "main")
	runGit(t, worktree, "fetch", "-q", "origin")
	if row = summary(); row.BehindDefault != 1 || row.AheadOfDefault != 1 {
		t.Fatalf("after main moved: %+v", row)
	}

	// With GitHub connected, a pushed branch's pull request is found by a
	// tracking pass in the background and appears on a later poll; the
	// request path never asks GitHub.
	runGit(t, source, "remote", "set-url", "origin", "https://github.com/team/api.git")
	runGit(t, source, "config", "url."+remote+".insteadOf", "https://github.com/team/api.git")
	s.Bench.GitHub.Close()
	store, err := githubconnection.NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	if s.Bench.GitHub, err = githubconnection.New(store, &fixtureGitHub{}); err != nil {
		t.Fatal(err)
	}
	if _, err = s.ConnectGitHubPersonalToken(ctx, &pb.ConnectGitHubPersonalTokenRequest{Token: "ghp_personal_fixture"}); err != nil {
		t.Fatal(err)
	}
	fake := githubtest.New(t, "ghp_personal_fixture")
	fake.Repos["team/api"] = "main"
	s.Bench.GitHubPulls = github.NewAt(fake.URL)
	head := runGit(t, worktree, "rev-parse", "HEAD")
	fake.SetHead("team/api", branch, head)
	asked := func() (n int) { fake.Update(func(f *githubtest.Server) { n = len(f.Requests) }); return }
	if row = summary(); row.GithubRepository != "team/api" || row.PullRequest != nil {
		t.Fatalf("before any pull request: %+v", row)
	}
	if asked() != 0 {
		t.Fatal("the summary asked GitHub on its request path")
	}
	// Reads recorded by the store look ten minutes old, so every pass reads again.
	s.Bench.WorkItems().Now = func() time.Time { return time.Now().Add(-10 * time.Minute) }
	s.Bench.TrackItems(ctx)
	if asked() == 0 {
		t.Fatal("the pushed branch was never asked about")
	}
	created, err := s.CreatePullRequest(ctx, &pb.CreatePullRequestRequest{CollectionId: w.ID, Member: "api", Title: "Add the handler"})
	if err != nil {
		t.Fatal(err)
	}
	// Creating it records it: the next poll shows it without asking GitHub.
	requests := asked()
	if row = summary(); row.PullRequest == nil || row.PullRequest.Number != created.Number || row.PullRequest.State != "open" || row.PullRequest.Title != "Add the handler" {
		t.Fatalf("after create: %+v", row)
	}
	// Its checks fail: the summary shows what tracking last read, until the
	// next pass reads it again.
	fake.Update(func(f *githubtest.Server) {
		f.Checks[head] = []githubtest.Check{{Name: "test", Status: "completed", Conclusion: "failure"}}
	})
	if row = summary(); row.PullRequest == nil || row.PullRequest.ChecksState == "failure" || asked() != requests {
		t.Fatalf("the summary never reads GitHub: %+v", row)
	}
	s.Bench.TrackItems(ctx)
	if row = summary(); row.PullRequest == nil || row.PullRequest.ChecksState != "failure" {
		t.Fatalf("after the tracking pass: %+v", row)
	}
	// Another branch checked out: the old branch's pull request is not shown.
	runGit(t, worktree, "checkout", "-q", "-b", "elsewhere")
	if row = summary(); row.Branch != "elsewhere" || row.PullRequest != nil {
		t.Fatalf("after switching branch: %+v", row)
	}
	// Every workspace when none is named.
	all, err := s.ListWorkspaceGitSummaries(ctx, &pb.WorkspaceGitSummariesRequest{})
	if err != nil || len(all.Workspaces) != 1 {
		t.Fatal(all, err)
	}
}

// A detached HEAD, and a member that is not ready, are states, not failures.
func TestWorkspaceGitSummaryReportsDetachedAndUnreadyMembers(t *testing.T) {
	t.Setenv("GIT_CONFIG_GLOBAL", os.DevNull)
	t.Setenv("GIT_CONFIG_NOSYSTEM", "1")
	s, _ := collectionService(t)
	ctx := context.Background()
	w, err := s.Bench.Collections.Create(ctx, "detached", collections.Spec{Name: "Detached", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	runGit(t, w.Members[0].Path, "checkout", "-q", "--detach")
	list, err := s.ListWorkspaceGitSummaries(ctx, &pb.WorkspaceGitSummariesRequest{CollectionIds: []string{w.ID}})
	if err != nil {
		t.Fatal(err)
	}
	row := list.Workspaces[0].Repositories[0]
	if row.Branch != "" || row.Head == "" || row.Error != "" || row.Upstream != "" || row.PullRequest != nil {
		t.Fatalf("detached: %+v", row)
	}
	if err = os.RemoveAll(w.Members[0].Path); err != nil {
		t.Fatal(err)
	}
	list, err = s.ListWorkspaceGitSummaries(ctx, &pb.WorkspaceGitSummariesRequest{CollectionIds: []string{w.ID}})
	if err != nil || list.Workspaces[0].Repositories[0].Error == "" {
		t.Fatalf("missing worktree: %+v %v", list, err)
	}
}

// The summary names branches, commits and pull requests of every workspace:
// the owner's alone. No agent, terminal or paired device may read it.
func TestWorkspaceGitSummariesAreTheOwnersAlone(t *testing.T) {
	f := newDeviceFixture(t)
	ctx := timeout(t)
	_, _, token := f.open(t)
	request := &pb.WorkspaceGitSummariesRequest{}
	if _, err := f.authorityFixture.client.ListWorkspaceGitSummaries(bearer(ctx, token), request); status.Code(err) != codes.PermissionDenied {
		t.Errorf("agent credential got %v, want PermissionDenied", err)
	}
	if _, err := f.authorityFixture.client.ListWorkspaceGitSummaries(ctx, request); status.Code(err) != codes.Unauthenticated {
		t.Errorf("no credential got %v, want Unauthenticated", err)
	}
	// The owner reaches the RPC (this Runner has no worktree root, so it answers
	// with that precondition rather than an authority error).
	if _, err := f.authorityFixture.client.ListWorkspaceGitSummaries(bearer(ctx, ownerToken), request); status.Code(err) == codes.PermissionDenied || status.Code(err) == codes.Unauthenticated {
		t.Errorf("owner got %v", err)
	}
	_, device := f.pair(t, devices.GrantRead, devices.GrantRespond, devices.GrantStart)
	if _, err := f.client.ListWorkspaceGitSummaries(bearer(ctx, device), request); status.Code(err) != codes.PermissionDenied {
		t.Errorf("device credential got %v, want PermissionDenied", err)
	}
}
