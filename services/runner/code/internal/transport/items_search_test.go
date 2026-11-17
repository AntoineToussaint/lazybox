package transport

import (
	"context"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/repositories"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github/githubtest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// A Runner connected to a fake GitHub that knows team/api (the catalog's
// repository) and its issues.
func itemService(t *testing.T) (*Service, *githubtest.Server, *agenttest.Adapter) {
	t.Helper()
	s, agent := collectionService(t)
	ctx := context.Background()
	provider := &catalogGitHub{}
	store, err := githubconnection.NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHub.Close()
	if s.Bench.GitHub, err = githubconnection.New(store, provider); err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHubAPI = provider
	source := s.Bench.Collections.Repositories()[0].Path
	if s.Bench.RepositoryImporter, err = repositories.New(filepath.Join(t.TempDir(), "repos"), s.Bench.Collections, fixtureClone{source: source}); err != nil {
		t.Fatal(err)
	}
	fake := githubtest.New(t, "ghp_personal_fixture")
	fake.AddIssue("team/api", githubtest.Issue{Number: 812, Title: "Fix refund rounding", Body: "Refunds round down.", Author: "alice", Assignees: []string{"fixture"}, Labels: []string{"bug"},
		Comments: []githubtest.IssueComment{{Author: "bob", Body: "Seen in production", CreatedAt: "2026-11-10T10:00:00Z"}}})
	fake.AddIssue("team/api", githubtest.Issue{Number: 13, Title: "Improve startup", PullRequest: true})
	s.Bench.GitHubIssues = github.NewAt(fake.URL)
	// Not connected yet: every item read says how to connect.
	if _, err = s.SearchItems(ctx, &pb.SearchItemsRequest{}); err == nil || !strings.Contains(err.Error(), "connect GitHub") {
		t.Fatalf("unconnected search: %v", err)
	}
	if _, err = s.ConnectGitHubPersonalToken(ctx, &pb.ConnectGitHubPersonalTokenRequest{Token: "ghp_personal_fixture"}); err != nil {
		t.Fatal(err)
	}
	if _, err = s.GetRepositoryCatalog(ctx, &pb.GetRepositoryCatalogRequest{Refresh: true}); err != nil {
		t.Fatal(err)
	}
	return s, fake, agent
}

func TestItemRPCsSearchGetAndResolve(t *testing.T) {
	s, _, _ := itemService(t)
	ctx := context.Background()
	page, err := s.SearchItems(ctx, &pb.SearchItemsRequest{})
	if err != nil || len(page.Items) != 1 || page.Items[0].Ref.Key != "team/api#812" || page.Items[0].Repository != "github:team/api" {
		t.Fatal(page, err)
	}
	detail, err := s.GetItem(ctx, &pb.ItemRequest{Ref: &pb.ItemRef{Provider: "github", Key: "team/api#812"}})
	if err != nil || detail.Item.Title != "Fix refund rounding" || detail.Body != "Refunds round down." || len(detail.Comments) != 1 || detail.CommentCount != 1 {
		t.Fatal(detail, err)
	}
	if !strings.HasPrefix(detail.Context, "[Lazybox] External content: GitHub issue team/api#812, https://github.com/team/api/issues/812\n") || !strings.Contains(detail.Context, "> Seen in production") || strings.Contains(detail.ContextWithoutComments, "Seen in production") {
		t.Fatalf("context:\n%s", detail.Context)
	}
	for url, key := range map[string]string{"https://github.com/team/api/issues/812": "team/api#812", "https://github.com/team/api/pull/13": "team/api#13", "team/api#812": "team/api#812"} {
		ref, err := s.ResolveItemUrl(ctx, &pb.ResolveItemUrlRequest{Url: url})
		if err != nil || ref.Provider != "github" || ref.Key != key {
			t.Fatal(url, ref, err)
		}
	}
	if _, err = s.ResolveItemUrl(ctx, &pb.ResolveItemUrlRequest{Url: "https://example.com/x"}); err == nil {
		t.Fatal("a link that is no item resolved")
	}
	if _, err = s.GetItem(ctx, &pb.ItemRequest{Ref: &pb.ItemRef{Provider: "linear", Key: "PAY-1"}}); err == nil {
		t.Fatal("unknown provider accepted")
	}
}

func TestWorkspaceFromAnItemRecordsItsOriginAndBriefsTheAgent(t *testing.T) {
	s, _, agent := itemService(t)
	ctx := context.Background()
	progress := func(string, string) error { return nil }
	ref := &pb.ItemRef{Provider: "github", Key: "team/api#812"}
	// No repositories, no branch, no name: the item's own.
	w, err := s.Bench.PrepareWorkspaceFromItem(ctx, "from-issue", "", "", nil, ref, progress)
	if err != nil {
		t.Fatal(err)
	}
	if w.Name != "Fix refund rounding" || len(w.Members) != 1 || w.Members[0].Branch != "812-fix-refund-rounding" {
		t.Fatalf("workspace %+v", w)
	}
	if w.Origin == nil || w.Origin.Key != "team/api#812" || w.Origin.Kind != "issue" || w.Origin.ClosingReference != "Fixes #812" || w.Origin.URL != "https://github.com/team/api/issues/812" {
		t.Fatalf("origin %+v", w.Origin)
	}
	// A retry with the same key is the same workspace (its branch is not "taken").
	if again, err := s.Bench.PrepareWorkspaceFromItem(ctx, "from-issue", "", "", nil, ref, progress); err != nil || again.ID != w.ID {
		t.Fatal("retry made another workspace", err)
	}
	got, err := s.GetCollection(ctx, &pb.CollectionRequest{CollectionId: w.ID})
	if err != nil || got.Collection.Origin.GetKey() != "team/api#812" || got.Collection.Origin.GetProvider() != "github" {
		t.Fatal("origin not on the wire", got, err)
	}
	found, err := s.FindItemWorkspaces(ctx, &pb.ItemRequest{Ref: ref})
	if err != nil || len(found.Workspaces) != 1 || found.Workspaces[0].Id != w.ID {
		t.Fatal(found, err)
	}
	// Start another: a new branch beside the first.
	second, err := s.Bench.PrepareWorkspaceFromItem(ctx, "another", "", "", nil, ref, progress)
	if err != nil || second.Members[0].Branch != "812-fix-refund-rounding-2" {
		t.Fatal(second, err)
	}
	if found, _ = s.FindItemWorkspaces(ctx, &pb.ItemRequest{Ref: ref}); len(found.Workspaces) != 2 || found.Workspaces[0].Id != second.ID {
		t.Fatal("newest first", found)
	}
	if none, _ := s.FindItemWorkspaces(ctx, &pb.ItemRequest{Ref: &pb.ItemRef{Provider: "github", Key: "team/api#13"}}); len(none.Workspaces) != 0 {
		t.Fatal("another item matched")
	}
	// The agent's briefing names the origin and how to reference it.
	e, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: w.ID})
	if err != nil {
		t.Fatal(err)
	}
	if context := agent.Session.Open.Context; !strings.Contains(context, "This work started from team/api#812 (GitHub issue). Reference it as `Fixes #812` in your commits and pull request.") {
		t.Fatalf("briefing for %s:\n%s", e.Session.Id, context)
	}
	// A workspace that did not start from an item has no such line.
	plain, err := s.Bench.PrepareWorkspace(ctx, "plain", "", "Plain", "", []string{"github:team/api"}, progress)
	if err != nil || plain.Origin != nil {
		t.Fatal(plain, err)
	}
	if _, err = s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: plain.ID}); err != nil {
		t.Fatal(err)
	}
	if strings.Contains(agent.Session.Open.Context, "This work started from") {
		t.Fatal("briefing line without an origin")
	}
}

func TestWorkspaceFromAnItemGoesOnWhenTheProviderCannotBeRead(t *testing.T) {
	s, fake, _ := itemService(t)
	fake.Close()
	messages := []string{}
	w, err := s.Bench.PrepareWorkspaceFromItem(context.Background(), "offline", "Refunds", "", []string{"github:team/api"}, &pb.ItemRef{Provider: "github", Key: "team/api#812"}, func(_, m string) error { messages = append(messages, m); return nil })
	if err != nil {
		t.Fatal(err)
	}
	if w.Origin == nil || w.Origin.Key != "team/api#812" || w.Origin.Title != "" || w.Members[0].Branch != "812" || !strings.Contains(strings.Join(messages, "\n"), "Could not read team/api#812 from GitHub") {
		t.Fatalf("%+v %v", w, messages)
	}
}
