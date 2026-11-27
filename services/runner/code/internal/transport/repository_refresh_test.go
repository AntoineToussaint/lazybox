package transport

import (
	"context"
	"errors"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/repositories"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// replacingGitHub is GitHub where team/api can come to be another
// repository: renamed away, and a new one (a new ID, a new root commit) made
// under the name. Clones come from the local repository standing for it.
type replacingGitHub struct {
	fixtureGitHub
	mu     sync.Mutex
	named  github.Repository
	source string
}

func (f *replacingGitHub) RepositoryNamed(_ context.Context, _ string, name string) (github.Repository, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if strings.EqualFold(name, f.named.FullName) {
		return f.named, nil
	}
	return github.Repository{}, errors.New("GitHub has no repository by that name")
}

// Repository by ID: the old ID is the old repository's, renamed away.
func (f *replacingGitHub) Repository(_ context.Context, _ string, id int64) (github.Repository, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if id == f.named.ID {
		return f.named, nil
	}
	return github.Repository{ID: id, Name: "api-v1", FullName: "team/api-v1", HTMLURL: "https://github.com/team/api-v1", CloneURL: "https://github.com/team/api-v1.git"}, nil
}

func (f *replacingGitHub) Clone(ctx context.Context, _, destination, _ string) error {
	f.mu.Lock()
	source := f.source
	f.mu.Unlock()
	return exec.CommandContext(ctx, "git", "clone", "--bare", "--quiet", source, destination).Run()
}

// becomes makes team/api another repository on "GitHub": id, cloned from source.
func (f *replacingGitHub) becomes(id int64, source string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.named = github.Repository{ID: id, Name: "api", FullName: "team/api", HTMLURL: "https://github.com/team/api", CloneURL: "https://github.com/team/api.git"}
	f.source = source
}

type preparationStream struct {
	grpc.ServerStream
	ctx  context.Context
	sent []*pb.WorkspacePreparation
}

func (s *preparationStream) Context() context.Context { return s.ctx }
func (s *preparationStream) Send(p *pb.WorkspacePreparation) error {
	s.sent = append(s.sent, p)
	return nil
}

func git(t *testing.T, dir string, args ...string) string {
	t.Helper()
	out, err := exec.Command("git", append([]string{"-C", dir, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid"}, args...)...).CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v\n%s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

func history(t *testing.T, message string) string {
	t.Helper()
	path := t.TempDir()
	git(t, path, "init", "-b", "main")
	git(t, path, "commit", "--allow-empty", "-m", message)
	return path
}

func replacingService(t *testing.T) (*Service, *replacingGitHub) {
	t.Helper()
	s, _ := collectionService(t)
	provider := &replacingGitHub{}
	provider.becomes(101, history(t, "the first history"))
	store, err := githubconnection.NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHub.Close()
	if s.Bench.GitHub, err = githubconnection.New(store, provider); err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHubAPI = provider
	ctx := context.Background()
	if _, err = s.ConnectGitHubPersonalToken(ctx, &pb.ConnectGitHubPersonalTokenRequest{Token: "ghp_personal_fixture"}); err != nil {
		t.Fatal(err)
	}
	if s.Bench.RepositoryImporter, err = repositories.New(filepath.Join(t.TempDir(), "repos"), s.Bench.Collections, provider); err != nil {
		t.Fatal(err)
	}
	if _, err = s.GetRepositoryCatalog(ctx, &pb.GetRepositoryCatalogRequest{Refresh: true}); err != nil {
		t.Fatal(err)
	}
	return s, provider
}

func noProgress(string, string) error { return nil }

func rootOf(t *testing.T, dir string) string {
	t.Helper()
	return git(t, dir, "rev-list", "--max-parents=0", "HEAD")
}

// The case from the field: a repository renamed away on GitHub and a new one
// (new ID, new root commit) made under its name. Making a workspace from
// Lazybox's copy of the old one is refused, never mixed; New task gets the
// detail to offer "Replace Lazybox's copy", which re-clones; the retry then
// works from the new history, and the workspace made from the old one keeps
// its worktree.
func TestReplacingACopyWhoseRepositoryWasReplacedOnGitHub(t *testing.T) {
	s, provider := replacingService(t)
	ctx := context.Background()
	first := provider.source
	old, err := s.Bench.PrepareWorkspace(ctx, "old", "", "Old work", "", []string{"github:team/api"}, noProgress)
	if err != nil {
		t.Fatal(err)
	}
	oldTree := old.Members[0].Path
	git(t, oldTree, "commit", "--allow-empty", "-m", "work on the old history")

	replacement := history(t, "a new root")
	provider.becomes(303, replacement)

	stream := &preparationStream{ctx: ctx}
	err = s.PrepareWorkspace(&pb.PrepareWorkspaceRequest{RequestKey: "new", Name: "New work", RepositoryIdentities: []string{"github:team/api"}}, stream)
	if err == nil {
		t.Fatal("a workspace was made from the old repository's copy")
	}
	last := stream.sent[len(stream.sent)-1]
	if last.Replaced == nil || last.Replaced.Identity != "github:team/api" || !last.Replaced.HasCopy {
		t.Fatalf("the error card gets no replacement detail: %+v (%v)", last, err)
	}

	preview, err := s.RefreshRepositoryIdentity(ctx, &pb.RefreshRepositoryIdentityRequest{Identity: "github:team/api"})
	if err != nil || preview.Replaced || preview.Reason != "replaced" || !preview.HasCopy || len(preview.Workspaces) != 1 || preview.Workspaces[0] != "Old work" {
		t.Fatalf("preview = %+v, %v", preview, err)
	}
	done, err := s.RefreshRepositoryIdentity(ctx, &pb.RefreshRepositoryIdentityRequest{Identity: "github:team/api", Confirm: true})
	if err != nil || !done.Replaced || done.RepositoryId == "" {
		t.Fatalf("replace = %+v, %v", done, err)
	}

	again, err := s.Bench.PrepareWorkspace(ctx, "new", "", "New work", "", []string{"github:team/api"}, noProgress)
	if err != nil {
		t.Fatalf("retry after replacing: %v", err)
	}
	if got, want := rootOf(t, again.Members[0].Path), rootOf(t, replacement); got != want {
		t.Fatalf("new workspace starts from root %s, want the new history's %s", got, want)
	}
	// The old workspace keeps its worktree, its work, and its source copy.
	if got, want := rootOf(t, oldTree), rootOf(t, first); got != want {
		t.Fatalf("old workspace's history changed: root %s, want %s", got, want)
	}
	if !strings.Contains(git(t, oldTree, "log", "-1", "--format=%s"), "work on the old history") {
		t.Fatal("old workspace lost its work")
	}
	git(t, oldTree, "status", "--porcelain")
	if _, err = s.Bench.Collections.Acquire(ctx, old.ID, "next-conversation", func(string) bool { return false }); err != nil {
		t.Fatalf("the old workspace takes no new conversation: %v", err)
	}
}

// A force-push with a new root on the same repository (same ID): Replace makes
// a fresh copy beside the old one, new workspaces start from the new history,
// and old worktrees are unaffected.
func TestReplacingACopyAfterAForcePushWithANewRoot(t *testing.T) {
	s, provider := replacingService(t)
	ctx := context.Background()
	upstream := provider.source
	old, err := s.Bench.PrepareWorkspace(ctx, "old", "", "Old work", "", []string{"github:team/api"}, noProgress)
	if err != nil {
		t.Fatal(err)
	}
	oldRoot := rootOf(t, old.Members[0].Path)
	// The history is replaced in place: an orphan branch with a new root
	// becomes main, as `git push --force` of a fresh history leaves it.
	git(t, upstream, "checkout", "--orphan", "fresh")
	git(t, upstream, "commit", "--allow-empty", "-m", "a new root")
	git(t, upstream, "branch", "-M", "fresh", "main")
	newRoot := rootOf(t, upstream)
	if newRoot == oldRoot {
		t.Fatal("fixture did not replace the history")
	}
	preview, err := s.RefreshRepositoryIdentity(ctx, &pb.RefreshRepositoryIdentityRequest{Identity: "github:team/api"})
	if err != nil || preview.Reason != "" || !preview.HasCopy {
		t.Fatalf("preview = %+v, %v", preview, err)
	}
	done, err := s.RefreshRepositoryIdentity(ctx, &pb.RefreshRepositoryIdentityRequest{Identity: "github:team/api", Confirm: true})
	if err != nil || !done.Replaced {
		t.Fatalf("replace = %+v, %v", done, err)
	}
	fresh, err := s.Bench.PrepareWorkspace(ctx, "fresh", "", "Fresh work", "", []string{"github:team/api"}, noProgress)
	if err != nil {
		t.Fatal(err)
	}
	if got := rootOf(t, fresh.Members[0].Path); got != newRoot {
		t.Fatalf("new workspace root %s, want %s", got, newRoot)
	}
	if got := rootOf(t, old.Members[0].Path); got != oldRoot {
		t.Fatalf("old workspace root %s, want %s", got, oldRoot)
	}
	if fresh.Members[0].RepositoryID == old.Members[0].RepositoryID {
		t.Fatal("the new workspace was made from the old copy")
	}
}

// Only Lazybox's own copies are replaced: never a checkout the person registered.
func TestRefreshNeverReplacesAPersonsCheckout(t *testing.T) {
	s, _ := replacingService(t)
	ctx := context.Background()
	identity := s.Bench.Collections.RepositoryIdentity("api")
	if _, err := s.RefreshRepositoryIdentity(ctx, &pb.RefreshRepositoryIdentityRequest{Identity: identity, Confirm: true}); err == nil {
		t.Fatal("a local repository was replaced")
	}
}

// Lazybox listed the repository but never copied it, and since then the name
// came to mean a new repository: the listing's GitHub ID is the old one's,
// renamed away. That was "repository identity changed; refresh and retry",
// with no way forward. It is still refused, with the detail; using the
// repository GitHub has now copies it, and the retry works.
func TestAListedRepositoryReplacedOnGitHubIsCopiedFresh(t *testing.T) {
	s, provider := replacingService(t)
	ctx := context.Background()
	replacement := history(t, "a new root")
	provider.becomes(303, replacement)
	_, err := s.Bench.PrepareWorkspace(ctx, "new", "", "New work", "", []string{"github:team/api"}, noProgress)
	var replaced *runner.RepositoryReplaced
	if !errors.As(err, &replaced) || replaced.HasCopy || replaced.Identity != "github:team/api" {
		t.Fatalf("prepare = %v; want the repository reported replaced, with no copy", err)
	}
	preview, err := s.RefreshRepositoryIdentity(ctx, &pb.RefreshRepositoryIdentityRequest{Identity: "github:team/api"})
	if err != nil || preview.Reason != "replaced" || preview.HasCopy || len(preview.Workspaces) != 0 {
		t.Fatalf("preview = %+v, %v", preview, err)
	}
	if _, err = s.RefreshRepositoryIdentity(ctx, &pb.RefreshRepositoryIdentityRequest{Identity: "github:team/api", Confirm: true}); err != nil {
		t.Fatal(err)
	}
	created, err := s.Bench.PrepareWorkspace(ctx, "new", "", "New work", "", []string{"github:team/api"}, noProgress)
	if err != nil {
		t.Fatalf("retry: %v", err)
	}
	if got, want := rootOf(t, created.Members[0].Path), rootOf(t, replacement); got != want {
		t.Fatalf("root %s, want %s", got, want)
	}
}

func TestRequestedEmptyRepositoryCanBeRetriedAfterFirstPush(t *testing.T) {
	s, provider := replacingService(t)
	ctx := context.Background()
	origin := t.TempDir()
	git(t, origin, "init", "-b", "main")
	provider.becomes(101, origin)

	created, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: "workspace", Spec: &pb.CollectionSpec{Name: "Workspace"}})
	if err != nil {
		t.Fatal(err)
	}
	workspaceID := created.Collection.Id
	conversation, err := s.Bench.OpenCollection("claude-default", workspaceID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	ask, err := s.RequestRepository(ctx, &pb.RequestRepositoryRequest{SessionId: conversation.ID, Query: "team/api"})
	if err != nil || len(ask.GetCandidates()) == 0 {
		t.Fatal(ask, err)
	}
	request := &pb.PrepareWorkspaceRequest{
		CollectionId: workspaceID, RepositoryRequestId: ask.Id, RequestKey: "add-api",
		RepositoryIdentities: []string{"github:team/api"},
	}
	stream := &preparationStream{ctx: ctx}
	err = s.PrepareWorkspace(request, stream)
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(status.Convert(err).Message(), "create an initial commit") {
		t.Fatalf("wanted actionable empty repository error, got %v", err)
	}
	for _, update := range stream.sent {
		if strings.Contains(update.Message, "started from") || update.Collection != nil {
			t.Fatalf("preparation reported a start despite missing a commit: %v", update)
		}
	}
	workspace, err := s.ListWorkspaceRepositories(ctx, &pb.ListWorkspaceRepositoriesRequest{CollectionId: workspaceID})
	if err != nil || len(workspace.GetRepositories()) != 0 || len(workspace.GetRequests()) != 1 || workspace.Requests[0].State != "pending" {
		t.Fatalf("failed preparation altered the workspace or request: %v, %v", workspace, err)
	}

	git(t, origin, "commit", "--allow-empty", "-m", "first commit")
	first := git(t, origin, "rev-parse", "HEAD")
	stream = &preparationStream{ctx: ctx}
	if err := s.PrepareWorkspace(request, stream); err != nil {
		t.Fatal("retry after first push:", err)
	}
	ready := stream.sent[len(stream.sent)-1].GetCollection()
	if len(ready.GetMembers()) != 1 || ready.Members[0].BaseCommit != first {
		t.Fatalf("retry did not attach the new commit: %v", ready)
	}
	answer, err := s.WaitRepositoryRequest(ctx, &pb.WaitRepositoryRequestRequest{RequestId: ask.Id})
	if err != nil || answer.GetState() != "added" {
		t.Fatal(answer, err)
	}
}

func TestDeleteConflictingBranchAndRetryWorkspace(t *testing.T) {
	s, _ := replacingService(t)
	ctx := context.Background()
	ids := []string{"github:team/api"}
	first, err := s.Bench.PrepareWorkspace(ctx, "first", "", "First", "", ids, noProgress)
	if err != nil {
		t.Fatal(err)
	}
	const branch = "feature/conflict"
	member := first.Members[0]
	git(t, member.Path, "branch", branch)
	request := &pb.PrepareWorkspaceRequest{RequestKey: "retry", Name: "Second", RepositoryIdentities: ids, Branch: branch}
	stream := &preparationStream{ctx: ctx}
	if err := s.PrepareWorkspace(request, stream); err == nil || len(stream.sent) == 0 {
		t.Fatalf("missing branch conflict: %v", err)
	}
	conflict := stream.sent[len(stream.sent)-1]
	if conflict.ConflictingRepositoryId != member.RepositoryID {
		t.Fatalf("unexpected conflicting repository: %v", conflict)
	}
	if _, err := s.DeleteLocalBranch(ctx, &pb.DeleteLocalBranchRequest{RepositoryId: conflict.ConflictingRepositoryId, Branch: branch}); err != nil {
		t.Fatal(err)
	}
	if err := s.PrepareWorkspace(request, stream); err != nil || stream.sent[len(stream.sent)-1].Collection == nil {
		t.Fatalf("retry did not create workspace: %v", err)
	}
}
