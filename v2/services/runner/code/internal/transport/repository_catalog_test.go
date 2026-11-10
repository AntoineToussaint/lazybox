package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/repositories"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/repositorycatalog"
	"github.com/AntoineToussaint/lazybox/v2/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"path/filepath"
	"testing"
)

type catalogGitHub struct {
	fixtureGitHub
	pages []int
	fail  bool
}

func (f *catalogGitHub) UserRepositories(_ context.Context, _ string, page int) ([]github.Repository, bool, error) {
	f.pages = append(f.pages, page)
	if f.fail && page == 2 {
		return nil, false, errors.New("provider unavailable")
	}
	if page == 1 {
		return []github.Repository{{ID: 202, Name: "other", FullName: "org/other"}}, true, nil
	}
	return []github.Repository{githubRepo()}, false, nil
}
func TestUnifiedCatalogToWorkspaceWithoutRegistrationStep(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	provider := &catalogGitHub{}
	store, err := githubconnection.NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHub.Close()
	s.Bench.GitHub, err = githubconnection.New(store, provider)
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHubAPI = provider
	if _, err = s.ConnectGitHubPersonalToken(ctx, &pb.ConnectGitHubPersonalTokenRequest{Token: "ghp_personal_fixture"}); err != nil {
		t.Fatal(err)
	}
	source := s.Bench.Collections.Repositories()[0].Path
	s.Bench.RepositoryImporter, err = repositories.New(filepath.Join(t.TempDir(), "repos"), s.Bench.Collections, fixtureClone{source: source})
	if err != nil {
		t.Fatal(err)
	}
	before := len(s.Bench.Collections.Repositories())
	catalog, err := s.GetRepositoryCatalog(ctx, &pb.GetRepositoryCatalogRequest{Refresh: true})
	if err != nil || len(catalog.Entries) != 3 || len(provider.pages) != 2 {
		t.Fatal(catalog, provider.pages, err)
	}
	if len(s.Bench.Collections.Repositories()) != before {
		t.Fatal("discovery prepared a checkout")
	}
	// Recreate the cache from disk: discovery survives both picker and Runner lifetimes.
	s.Bench.RepositoryCatalog, err = repositorycatalog.New(repositorycatalog.FileStore{Path: filepath.Join(s.Bench.Config.State, "repository-catalog.json")})
	if err != nil {
		t.Fatal(err)
	}
	cached, err := s.GetRepositoryCatalog(ctx, &pb.GetRepositoryCatalogRequest{})
	if err != nil || len(cached.Entries) != 3 || len(provider.pages) != 2 {
		t.Fatal("cached catalog missing", err)
	}
	updates := []string{}
	progress := func(_, message string) error { updates = append(updates, message); return nil }
	workspace, err := s.Bench.PrepareWorkspace(ctx, "catalog-create", "", "Feature", []string{"github:team/api"}, progress)
	if err != nil || len(workspace.Members) != 1 {
		t.Fatal(workspace, err)
	}
	again, err := s.Bench.PrepareWorkspace(ctx, "catalog-create", "", "Feature", []string{"github:team/api"}, progress)
	if err != nil || again.ID != workspace.ID || len(s.Bench.Collections.Repositories()) != before+1 {
		t.Fatal("retry duplicated resources", again, err)
	}
	localIdentity := s.Bench.Collections.RepositoryIdentity("api")
	extended, err := s.Bench.PrepareWorkspace(ctx, "catalog-add", workspace.ID, "", []string{localIdentity}, progress)
	if err != nil || len(extended.Members) != 2 {
		t.Fatal(extended, err)
	}
	if len(updates) < 4 {
		t.Fatal("missing progress")
	}
	provider.fail = true
	failed, err := s.GetRepositoryCatalog(ctx, &pb.GetRepositoryCatalogRequest{Refresh: true})
	if err != nil || len(failed.Entries) != 3 || failed.Sources[0].State != "error" {
		t.Fatal("failed refresh lost results", failed, err)
	}
	if _, err = s.Bench.PrepareWorkspace(ctx, "invalid", "", "Bad", []string{"github:not/discovered"}, progress); err == nil {
		t.Fatal("unknown identity accepted")
	}
}
