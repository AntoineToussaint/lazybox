package transport

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func runGit(t *testing.T, dir string, args ...string) string {
	t.Helper()
	cmd := exec.Command("git", append([]string{"-C", dir}, args...)...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

// The whole loop through the Runner's RPCs: commit and push to a local bare
// remote (no network), then, with origin naming a GitHub repository, open,
// read and merge its pull request against a fake GitHub API.
func TestPullRequestLoopThroughTheRunner(t *testing.T) {
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
	w, err := s.Bench.Collections.Create(ctx, "ship", collections.Spec{Name: "Ship", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	worktree := w.Members[0].Path
	changes := func() *pb.RepositoryChanges {
		t.Helper()
		list, err := s.ListWorkspaceChanges(ctx, &pb.WorkspaceChangesRequest{CollectionId: w.ID})
		if err != nil || len(list.Repositories) != 1 {
			t.Fatal(list, err)
		}
		return list.Repositories[0]
	}
	branch := changes().Branch
	if row := changes(); row.Upstream != "" || row.Ahead != 0 || row.GithubRepository != "" {
		t.Fatalf("fresh workspace: %+v", row)
	}

	if _, err = s.CommitWorkspaceChanges(ctx, &pb.CommitWorkspaceChangesRequest{CollectionId: w.ID, Member: "api", Message: "Nothing"}); status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "nothing to commit") {
		t.Fatalf("empty commit: %v", err)
	}
	if err = os.WriteFile(filepath.Join(worktree, "handler.go"), []byte("package api\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	committed, err := s.CommitWorkspaceChanges(ctx, &pb.CommitWorkspaceChangesRequest{CollectionId: w.ID, Member: "api", Message: "Add the handler"})
	if err != nil || committed.Subject != "Add the handler" || committed.Files != 1 || committed.Branch != branch {
		t.Fatal(committed, err)
	}
	if _, err = s.CommitWorkspaceChanges(ctx, &pb.CommitWorkspaceChangesRequest{CollectionId: w.ID, Member: "../api", Message: "x"}); err == nil {
		t.Fatal("arbitrary member accepted")
	}
	if row := changes(); row.Ahead != 1 || row.Upstream != "" || len(row.Files) != 0 {
		t.Fatalf("after commit: %+v", row)
	}
	pushed, err := s.PushWorkspaceBranch(ctx, &pb.PushWorkspaceBranchRequest{CollectionId: w.ID, Member: "api"})
	if err != nil || pushed.Branch != branch || pushed.Head != committed.Commit || pushed.GithubRepository != "" {
		t.Fatal(pushed, err)
	}
	if runGit(t, remote, "rev-parse", "refs/heads/"+branch) != committed.Commit {
		t.Fatal("branch not on the remote")
	}
	if row := changes(); row.Ahead != 0 || row.Upstream != "origin/"+branch {
		t.Fatalf("after push: %+v", row)
	}
	pr, err := s.GetPullRequest(ctx, &pb.GetPullRequestRequest{CollectionId: w.ID, Member: "api"})
	if err != nil || !strings.Contains(pr.Unavailable, "not a GitHub repository") {
		t.Fatal(pr, err)
	}

	// origin now names a GitHub repository. The repository's own insteadOf
	// keeps the push on the local remote, so nothing leaves this machine.
	runGit(t, source, "remote", "set-url", "origin", "https://github.com/team/api.git")
	runGit(t, source, "config", "url."+remote+".insteadOf", "https://github.com/team/api.git")
	if pr, err = s.GetPullRequest(ctx, &pb.GetPullRequestRequest{CollectionId: w.ID, Member: "api"}); err != nil || pr.Unavailable != connectForPullRequests || pr.GithubRepository != "team/api" {
		t.Fatal(pr, err)
	}
	if _, err = s.PushWorkspaceBranch(ctx, &pb.PushWorkspaceBranchRequest{CollectionId: w.ID, Member: "api"}); err == nil || !strings.Contains(err.Error(), "connect GitHub") {
		t.Fatalf("push without a connection: %v", err)
	}
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
	if changes().GithubRepository != "team/api" {
		t.Fatal("GitHub origin not reported")
	}

	if err = os.WriteFile(filepath.Join(worktree, "handler_test.go"), []byte("package api\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	second, err := s.CommitWorkspaceChanges(ctx, &pb.CommitWorkspaceChangesRequest{CollectionId: w.ID, Member: "api", Message: "Test the handler", Paths: []string{"handler_test.go"}})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = s.CreatePullRequest(ctx, &pb.CreatePullRequestRequest{CollectionId: w.ID, Member: "api", Title: "Add the handler"}); err == nil || !strings.Contains(err.Error(), "not pushed yet") {
		t.Fatalf("pull request with unpushed commits: %v", err)
	}
	if pushed, err = s.PushWorkspaceBranch(ctx, &pb.PushWorkspaceBranchRequest{CollectionId: w.ID, Member: "api"}); err != nil || pushed.Head != second.Commit || pushed.GithubRepository != "team/api" {
		t.Fatal(pushed, err)
	}
	if config, _ := os.ReadFile(filepath.Join(source, ".git", "config")); strings.Contains(string(config), "ghp_personal_fixture") {
		t.Fatal("credential written to Git config")
	}
	fake.SetHead("team/api", branch, second.Commit)

	pr, err = s.CreatePullRequest(ctx, &pb.CreatePullRequestRequest{CollectionId: w.ID, Member: "api", Title: "Add the handler", Body: "Adds it.", Draft: false})
	if err != nil || !pr.Found || pr.Number != 1 || pr.State != "open" || pr.Base != "main" || pr.HeadSha != second.Commit || pr.Branch != branch {
		t.Fatal(pr, err)
	}
	fake.Update(func(f *githubtest.Server) {
		f.Pulls["team/api"][0].MergeableState = "unstable"
		f.Checks[second.Commit] = []githubtest.Check{{Name: "test", Status: "completed", Conclusion: "failure", URL: "https://github.com/team/api/actions/runs/9"}, {Name: "build", Status: "completed", Conclusion: "success"}}
	})
	pr, err = s.GetPullRequest(ctx, &pb.GetPullRequestRequest{CollectionId: w.ID, Member: "api"})
	if err != nil || pr.ChecksState != "failure" || len(pr.Checks) != 2 || pr.Checks[0].Name != "test" || pr.Checks[0].Url == "" {
		t.Fatal(pr, err)
	}
	if _, err = s.MergePullRequest(ctx, &pb.MergePullRequestRequest{CollectionId: w.ID, Member: "api", Number: 7, Method: "squash", ExpectedHead: second.Commit}); err == nil || !strings.Contains(err.Error(), "not this branch's") {
		t.Fatalf("merged another pull request: %v", err)
	}
	if _, err = s.MergePullRequest(ctx, &pb.MergePullRequestRequest{CollectionId: w.ID, Member: "api", Number: 1, Method: "squash", ExpectedHead: committed.Commit}); err == nil || !strings.Contains(err.Error(), "Head branch was modified") {
		t.Fatalf("merged an unreviewed head: %v", err)
	}
	pr, err = s.MergePullRequest(ctx, &pb.MergePullRequestRequest{CollectionId: w.ID, Member: "api", Number: 1, Method: "squash", ExpectedHead: second.Commit})
	if err != nil || pr.State != "merged" || pr.MergeCommit == "" {
		t.Fatal(pr, err)
	}
	if merged := fake.Pull("team/api", 1); merged.MergeMethod != "squash" {
		t.Fatal(merged)
	}
	if fake.Unauthorized != 0 {
		t.Fatal("a GitHub request went out without the connection's credential")
	}
}

// Agents run inside the workspace and hold a scoped credential; none of the
// PR loop's writes or reads is theirs.
func TestAgentCredentialCannotCommitPushOrMerge(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	_, _, token := f.open(t)
	agent := bearer(ctx, token)
	calls := map[string]error{}
	_, calls["CommitWorkspaceChanges"] = f.client.CommitWorkspaceChanges(agent, &pb.CommitWorkspaceChangesRequest{CollectionId: "w", Member: "api", Message: "x"})
	_, calls["PushWorkspaceBranch"] = f.client.PushWorkspaceBranch(agent, &pb.PushWorkspaceBranchRequest{CollectionId: "w", Member: "api"})
	_, calls["CreatePullRequest"] = f.client.CreatePullRequest(agent, &pb.CreatePullRequestRequest{CollectionId: "w", Member: "api", Title: "x"})
	_, calls["GetPullRequest"] = f.client.GetPullRequest(agent, &pb.GetPullRequestRequest{CollectionId: "w", Member: "api"})
	_, calls["MergePullRequest"] = f.client.MergePullRequest(agent, &pb.MergePullRequestRequest{CollectionId: "w", Member: "api", Number: 1, Method: "squash", ExpectedHead: "x"})
	for name, err := range calls {
		if status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s: agent credential got %v, want PermissionDenied", name, err)
		}
	}
	if _, err := f.client.GetPullRequest(ctx, &pb.GetPullRequestRequest{CollectionId: "w", Member: "api"}); status.Code(err) != codes.Unauthenticated {
		t.Errorf("no credential got %v", err)
	}
}
