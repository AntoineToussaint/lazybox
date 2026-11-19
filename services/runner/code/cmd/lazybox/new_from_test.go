package main

import (
	"context"
	"encoding/json"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// identityGitHub is just enough of GitHub to connect a personal token.
type identityGitHub struct{ github.API }

func (identityGitHub) User(context.Context, string) (github.User, error) {
	return github.User{ID: 42, Login: "fixture"}, nil
}
func (identityGitHub) UserRepositories(context.Context, string, int) ([]github.Repository, bool, error) {
	return nil, false, nil
}

// fromIssueRunner is the CLI's test Runner, connected to a fake GitHub with
// an issue in team/api, and with team/api on the Runner.
func fromIssueRunner(t *testing.T) *testRunner {
	t.Helper()
	r := newTestRunner(t)
	ctx := context.Background()
	repo := t.TempDir()
	origin := filepath.Join(t.TempDir(), "api.git")
	const remoteURL = "https://github.com/team/api.git"
	// Keep the GitHub identity used by the issue lookup, while serving Git
	// operations from a fixture with a real remote default branch.
	for _, args := range [][]string{
		{"init", "-b", "main"},
		{"-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-m", "initial"},
		{"clone", "--bare", ".", origin},
		{"remote", "add", "origin", remoteURL},
		{"config", "url." + origin + ".insteadOf", remoteURL},
	} {
		if out, err := exec.Command("git", append([]string{"-C", repo}, args...)...).CombinedOutput(); err != nil {
			t.Fatalf("git: %v %s", err, out)
		}
	}
	added, err := r.service.AddLocalRepository(ctx, &pb.AddLocalRepositoryRequest{Path: repo})
	if err != nil || added.Repository.GitIdentity != "github:team/api" {
		t.Fatal(added, err)
	}
	store, err := githubconnection.NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	r.bench.GitHub.Close()
	if r.bench.GitHub, err = githubconnection.New(store, identityGitHub{}); err != nil {
		t.Fatal(err)
	}
	r.bench.GitHubAPI = identityGitHub{}
	if _, err = r.service.ConnectGitHubPersonalToken(ctx, &pb.ConnectGitHubPersonalTokenRequest{Token: "ghp_cli_fixture"}); err != nil {
		t.Fatal(err)
	}
	fake := githubtest.New(t, "ghp_cli_fixture")
	fake.AddIssue("team/api", githubtest.Issue{Number: 812, Title: "Fix refund rounding", Body: "Refunds round down.", Author: "alice",
		Comments: []githubtest.IssueComment{{Author: "bob", Body: "Seen in production", CreatedAt: "2026-11-10T10:00:00Z"}}})
	r.bench.GitHubIssues = github.NewAt(fake.URL)
	return r
}

func TestNewFromAnIssueStartsTheWorkspaceAndBriefsTheAgent(t *testing.T) {
	r := fromIssueRunner(t)
	out := r.cli(r.owner(), "", "new", "--from", "https://github.com/team/api/issues/812", "Start with the ledger", "--json")
	if out.code != 0 {
		t.Fatalf("new --from: %+v", out)
	}
	var started struct{ ID, Name, Branch, Item, Session string }
	if err := json.Unmarshal([]byte(out.stdout), &started); err != nil {
		t.Fatal(out.stdout, err)
	}
	if started.Name != "Fix refund rounding" || started.Branch != "812-fix-refund-rounding" || started.Item != "team/api#812" || started.Session == "" {
		t.Fatalf("started %+v", started)
	}
	got, err := r.service.GetCollection(context.Background(), &pb.CollectionRequest{CollectionId: started.ID})
	if err != nil || got.Collection.Origin.GetKey() != "team/api#812" {
		t.Fatal("origin", got, err)
	}
	native := <-r.provider.opened
	if native.Open.Permission != "ask" || !strings.Contains(native.Open.Context, "This work started from team/api#812 (GitHub issue). Reference it as `Fixes #812`") {
		t.Fatalf("opened with %s:\n%s", native.Open.Permission, native.Open.Context)
	}
	var first agent.Operation
	select {
	case first = <-native.Applied:
	case <-time.After(10 * time.Second):
		t.Fatal("no first message")
	}
	if !strings.HasPrefix(first.Text, "Start with the ledger\n\n[Lazybox] External content: GitHub issue team/api#812, https://github.com/team/api/issues/812\n") || !strings.Contains(first.Text, "> Seen in production") {
		t.Fatalf("first message:\n%s", first.Text)
	}

	// Again: the issue already has a workspace, and the CLI says which.
	again := r.cli(r.owner(), "", "new", "--from", "team/api#812")
	if again.code == 0 || !strings.Contains(again.err.Error(), "team/api#812 already has a workspace, “Fix refund rounding”") || !strings.Contains(again.err.Error(), "--another") {
		t.Fatalf("again: %+v", again)
	}
	// --another starts a second one beside it, without the comments and with the default message.
	another := r.cli(r.owner(), "", "new", "--from", "team/api#812", "--another", "--no-comments", "--json")
	if another.code != 0 || !strings.Contains(another.stdout, `"branch": "812-fix-refund-rounding-2"`) {
		t.Fatalf("another: %+v", another)
	}
	native = <-r.provider.opened
	first = <-native.Applied
	if !strings.HasPrefix(first.Text, "Work on issue team/api#812: Fix refund rounding.\n\n[Lazybox] External content") || strings.Contains(first.Text, "Seen in production") {
		t.Fatalf("second first message:\n%s", first.Text)
	}

	for args, code := range map[string]int{
		"https://example.com/nope":         exitUsage,
		"team/api#812 --permission bypass": exitUsage,
	} {
		parts := append([]string{"new", "--from"}, strings.Fields(args)...)
		if out := r.cli(r.owner(), "", parts...); out.code != code {
			t.Errorf("%s: %+v", args, out)
		}
	}
}
