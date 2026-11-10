package transport

import (
	"context"
	"encoding/json"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/repositories"
	"github.com/AntoineToussaint/lazybox/v2/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

type fixtureGitHub struct {
	github.API
	itemsCalls    int
	personalCalls int
}

func (*fixtureGitHub) User(context.Context, string) (github.User, error) {
	return github.User{ID: 42, Login: "fixture"}, nil
}
func (f *fixtureGitHub) UserRepositories(_ context.Context, token string, _ int) ([]github.Repository, bool, error) {
	f.personalCalls++
	if token != "ghp_personal_fixture" {
		return nil, false, github.ErrUnauthorized
	}
	return []github.Repository{githubRepo()}, false, nil
}

func (*fixtureGitHub) Installations(context.Context, string, int) ([]github.Installation, bool, error) {
	return []github.Installation{{ID: 7, Account: github.User{Login: "team"}}}, false, nil
}
func (*fixtureGitHub) Repositories(context.Context, string, int64, int) ([]github.Repository, bool, error) {
	return []github.Repository{githubRepo()}, false, nil
}
func (*fixtureGitHub) Repository(context.Context, string, int64) (github.Repository, error) {
	return githubRepo(), nil
}
func (f *fixtureGitHub) Items(context.Context, string, string, int) ([]github.Item, bool, error) {
	f.itemsCalls++
	var items []github.Item
	json.Unmarshal([]byte(`[{"id":1,"number":12,"title":"Fix startup"},{"id":2,"number":13,"title":"Improve startup","pull_request":{"url":"https://api.github.com/repos/team/api/pulls/13"}}]`), &items)
	return items, false, nil
}
func (*fixtureGitHub) Begin(context.Context, string) (github.Device, error) {
	return github.Device{DeviceCode: "private-code", UserCode: "TEST-CODE", VerificationURI: "https://github.com/login/device", ExpiresIn: 900, Interval: 5}, nil
}
func (*fixtureGitHub) Exchange(context.Context, string, string) (github.Token, error) {
	return github.Token{Error: "authorization_pending"}, nil
}
func githubRepo() github.Repository {
	return github.Repository{ID: 101, Name: "api", FullName: "team/api", HTMLURL: "https://github.com/team/api", CloneURL: "https://github.com/team/api.git"}
}

type fixtureClone struct{ source string }

func (f fixtureClone) Clone(ctx context.Context, _, destination, _ string) error {
	return exec.CommandContext(ctx, "git", "clone", "--bare", f.source, destination).Run()
}
func TestGitHubRPCEnrollmentImportAndCollection(t *testing.T) {
	s, agent := collectionService(t)
	ctx := context.Background()
	provider := &fixtureGitHub{}
	s.Bench.GitHub.Close()
	store, err := githubconnection.NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	connection, err := githubconnection.New(store, provider)
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHub = connection
	s.Bench.GitHubAPI = provider
	state, err := s.GetGitHubConnection(ctx, &pb.GetGitHubConnectionRequest{})
	if err != nil || state.State != "setup_required" {
		t.Fatal(state, err)
	}
	_, err = s.ConfigureGitHubApp(ctx, &pb.ConfigureGitHubAppRequest{ClientId: "Iv.fixture", Slug: "fixture-app"})
	if err != nil {
		t.Fatal(err)
	}
	flow, err := s.BeginGitHubSignIn(ctx, &pb.BeginGitHubSignInRequest{RequestKey: "same"})
	if err != nil {
		t.Fatal(err)
	}
	state, _ = s.GetGitHubConnection(ctx, &pb.GetGitHubConnectionRequest{})
	if state.PendingSignIn == nil || state.PendingSignIn.Id != flow.Id {
		t.Fatal("did not retain pending sign-in")
	}
	read, err := s.GetGitHubSignIn(ctx, &pb.GitHubSignInRequest{Id: flow.Id})
	if err != nil || read.UserCode != "TEST-CODE" {
		t.Fatal(read, err)
	}
	cancelled, err := s.CancelGitHubSignIn(ctx, &pb.GitHubSignInRequest{Id: flow.Id})
	if err != nil || cancelled.State != "cancelled" {
		t.Fatal(cancelled, err)
	}
	// Seed a private fixture authorization after separately testing the complete
	// device/refresh lifecycle in githubconnection. No real GitHub account is used.
	connection.Close()
	data, _ := json.Marshal(map[string]any{"version": 1, "app": githubconnection.App{ClientID: "Iv.fixture", Slug: "fixture-app"}, "user": github.User{ID: 42, Login: "fixture"}, "token": github.Token{AccessToken: "ghu_fixture", RefreshToken: "ghr_fixture"}, "ExpiresAt": time.Now().Add(time.Hour).Unix(), "RefreshExpiresAt": time.Now().Add(24 * time.Hour).Unix()})
	if err = store.Save(data); err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHub, err = githubconnection.New(store, provider)
	if err != nil {
		t.Fatal(err)
	}
	installs, err := s.ListGitHubInstallations(ctx, &pb.ListGitHubInstallationsRequest{})
	if err != nil || installs.Installations[0].Account != "team" {
		t.Fatal(installs, err)
	}
	remote, err := s.ListGitHubRepositories(ctx, &pb.ListGitHubRepositoriesRequest{InstallationId: 7})
	if err != nil || remote.Repositories[0].RegisteredId != "" {
		t.Fatal(remote, err)
	}
	if _, err = s.ListGitHubItems(ctx, &pb.ListGitHubItemsRequest{RepositoryId: 101}); err == nil || provider.itemsCalls != 0 {
		t.Fatal("unselected repository was ingested")
	}
	source := s.Bench.Collections.Repositories()[0].Path
	s.Bench.RepositoryImporter, err = repositories.New(filepath.Join(t.TempDir(), "repos"), s.Bench.Collections, fixtureClone{source: source})
	if err != nil {
		t.Fatal(err)
	}
	imported, err := s.ImportGitHubRepository(ctx, &pb.ImportGitHubRepositoryRequest{RepositoryId: 101})
	if err != nil || imported.Repository.Name != "api" {
		t.Fatal(imported, err)
	}
	remote, err = s.ListGitHubRepositories(ctx, &pb.ListGitHubRepositoriesRequest{InstallationId: 7})
	if err != nil || remote.Repositories[0].RegisteredId != imported.Repository.Id {
		t.Fatal(remote, err)
	}
	items, err := s.ListGitHubItems(ctx, &pb.ListGitHubItemsRequest{RepositoryId: 101})
	if err != nil || len(items.Items) != 2 || items.Items[0].Kind != "issue" || items.Items[1].Kind != "pull_request" {
		t.Fatal(items, err)
	}
	created, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: "github-work", Spec: &pb.CollectionSpec{Name: "my-feature", Members: []*pb.WorktreeSpec{{RepositoryId: imported.Repository.Id}}}})
	if err != nil {
		t.Fatal(err)
	}
	if created.Collection.Members[0].Spec.Name != "api" {
		t.Fatal("repo folder default lost")
	}
	session, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: created.Collection.Id})
	if err != nil || session.Session.CollectionId != created.Collection.Id {
		t.Fatal(session, err)
	}
	_ = agent
	state, err = s.DisconnectGitHub(ctx, &pb.GetGitHubConnectionRequest{})
	if err != nil || state.State != "not_connected" {
		t.Fatal(state, err)
	}
	list, err := s.ListRepositories(ctx, &pb.ListRepositoriesRequest{})
	if err != nil || len(list.Repositories) != 2 {
		t.Fatal("disconnect deleted repositories", err)
	}
	if _, err = s.ListGitHubInstallations(ctx, &pb.ListGitHubInstallationsRequest{}); err == nil {
		t.Fatal("disconnected auth still works")
	}
}

func TestPersonalGitHubRPCWithoutAppImportsAndStartsWorkspace(t *testing.T) {
	s, _ := collectionService(t)
	ctx := context.Background()
	provider := &fixtureGitHub{}
	s.Bench.GitHub.Close()
	store, err := githubconnection.NewFileStore(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHub, err = githubconnection.New(store, provider)
	if err != nil {
		t.Fatal(err)
	}
	s.Bench.GitHubAPI = provider
	state, err := s.ConnectGitHubPersonalToken(ctx, &pb.ConnectGitHubPersonalTokenRequest{Token: "ghp_personal_fixture"})
	if err != nil || state.State != "connected" || state.AuthMethod != "personal_token" || state.ClientId != "" {
		t.Fatal(state, err)
	}
	if _, err := s.ListGitHubInstallations(ctx, &pb.ListGitHubInstallationsRequest{}); err == nil {
		t.Fatal("personal token used installation API")
	}
	if _, err := s.ListGitHubRepositories(ctx, &pb.ListGitHubRepositoriesRequest{InstallationId: 7}); err == nil {
		t.Fatal("personal token accepted installation selection")
	}
	remote, err := s.ListGitHubRepositories(ctx, &pb.ListGitHubRepositoriesRequest{})
	if err != nil || len(remote.Repositories) != 1 || provider.personalCalls != 1 {
		t.Fatal(remote, err)
	}
	source := s.Bench.Collections.Repositories()[0].Path
	s.Bench.RepositoryImporter, err = repositories.New(filepath.Join(t.TempDir(), "repos"), s.Bench.Collections, fixtureClone{source: source})
	if err != nil {
		t.Fatal(err)
	}
	imported, err := s.ImportGitHubRepository(ctx, &pb.ImportGitHubRepositoryRequest{RepositoryId: 101})
	if err != nil {
		t.Fatal(err)
	}
	items, err := s.ListGitHubItems(ctx, &pb.ListGitHubItemsRequest{RepositoryId: 101})
	if err != nil || len(items.Items) != 2 {
		t.Fatal(items, err)
	}
	workspace, err := s.CreateCollection(ctx, &pb.CreateCollectionRequest{RequestKey: "personal-work", Spec: &pb.CollectionSpec{Name: "personal-work", Members: []*pb.WorktreeSpec{{RepositoryId: imported.Repository.Id}}}})
	if err != nil {
		t.Fatal(err)
	}
	session, err := s.OpenSession(ctx, &pb.OpenSessionRequest{ProfileId: "claude-default", CollectionId: workspace.Collection.Id})
	if err != nil || session.Session.CollectionId != workspace.Collection.Id {
		t.Fatal(session, err)
	}
	state, err = s.DisconnectGitHub(ctx, &pb.GetGitHubConnectionRequest{})
	if err != nil || state.State != "setup_required" {
		t.Fatal(state, err)
	}
	list, err := s.ListRepositories(ctx, &pb.ListRepositoriesRequest{})
	if err != nil || len(list.Repositories) != 2 {
		t.Fatal("disconnect lost repositories", err)
	}
}
