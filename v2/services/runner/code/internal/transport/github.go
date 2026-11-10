package transport

import (
	"context"
	"errors"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/runner/githubconnection"
	"github.com/AntoineToussaint/lazybox/v2/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

func wireGitHubFlow(f githubconnection.Enrollment) *pb.GitHubSignIn {
	return &pb.GitHubSignIn{Id: f.ID, State: f.State, UserCode: f.UserCode, SignInUrl: f.SignInURL, ExpiresAt: f.ExpiresAt, Error: f.Error}
}
func (s *Service) GetGitHubConnection(context.Context, *pb.GetGitHubConnectionRequest) (*pb.GitHubConnection, error) {
	state := s.Bench.GitHub.Status()
	out := &pb.GitHubConnection{State: state.State, AuthMethod: state.AuthMethod, ClientId: state.App.ClientID, AppSlug: state.App.Slug, Login: state.Login, UserId: state.UserID, ExpiresAt: state.ExpiresAt, Error: state.Error}
	if flow, err := s.Bench.GitHub.Enrollment(""); err == nil && flow.State == "awaiting_login" {
		out.PendingSignIn = wireGitHubFlow(flow)
	}
	return out, nil
}
func (s *Service) ConnectGitHubPersonalToken(ctx context.Context, r *pb.ConnectGitHubPersonalTokenRequest) (*pb.GitHubConnection, error) {
	if _, err := s.Bench.GitHub.ConnectPersonalToken(ctx, r.Token); err != nil {
		return nil, rpcError(err)
	}
	return s.GetGitHubConnection(ctx, &pb.GetGitHubConnectionRequest{})
}
func (s *Service) ConfigureGitHubApp(ctx context.Context, r *pb.ConfigureGitHubAppRequest) (*pb.GitHubConnection, error) {
	if _, err := s.Bench.GitHub.Configure(githubconnection.App{ClientID: r.ClientId, Slug: r.Slug}); err != nil {
		return nil, rpcError(err)
	}
	return s.GetGitHubConnection(ctx, &pb.GetGitHubConnectionRequest{})
}
func (s *Service) BeginGitHubSignIn(ctx context.Context, r *pb.BeginGitHubSignInRequest) (*pb.GitHubSignIn, error) {
	f, err := s.Bench.GitHub.Begin(ctx, r.RequestKey)
	return wireGitHubFlow(f), rpcError(err)
}
func (s *Service) GetGitHubSignIn(ctx context.Context, r *pb.GitHubSignInRequest) (*pb.GitHubSignIn, error) {
	f, err := s.Bench.GitHub.Enrollment(r.Id)
	return wireGitHubFlow(f), rpcError(err)
}
func (s *Service) CancelGitHubSignIn(ctx context.Context, r *pb.GitHubSignInRequest) (*pb.GitHubSignIn, error) {
	f, err := s.Bench.GitHub.Cancel(r.Id)
	return wireGitHubFlow(f), rpcError(err)
}
func (s *Service) DisconnectGitHub(ctx context.Context, r *pb.GetGitHubConnectionRequest) (*pb.GitHubConnection, error) {
	if err := s.Bench.GitHub.Disconnect(); err != nil {
		return nil, rpcError(err)
	}
	return s.GetGitHubConnection(ctx, r)
}
func pageNumber(page uint32) int {
	if page == 0 {
		return 1
	}
	return int(page)
}
func (s *Service) ListGitHubInstallations(ctx context.Context, r *pb.ListGitHubInstallationsRequest) (*pb.ListGitHubInstallationsResponse, error) {
	out := &pb.ListGitHubInstallationsResponse{}
	err := s.Bench.GitHub.WithAuthorization(ctx, func(token, method string) error {
		if method == "personal_token" {
			return errors.New("personal-token connections discover repositories directly, without installations")
		}
		items, more, err := s.Bench.GitHubAPI.Installations(ctx, token, pageNumber(r.Page))
		out.HasMore = more
		for _, item := range items {
			out.Installations = append(out.Installations, &pb.GitHubInstallation{Id: item.ID, Account: item.Account.Login, RepositorySelection: item.RepositorySelection})
		}
		return err
	})
	return out, rpcError(err)
}
func (s *Service) ListGitHubRepositories(ctx context.Context, r *pb.ListGitHubRepositoriesRequest) (*pb.ListGitHubRepositoriesResponse, error) {
	out := &pb.ListGitHubRepositoriesResponse{}
	registered := map[int64]string{}
	if s.Bench.Collections != nil {
		for _, repo := range s.Bench.Collections.Repositories() {
			registered[repo.GitHubID] = repo.ID
		}
	}
	err := s.Bench.GitHub.WithAuthorization(ctx, func(token, method string) error {
		var items []github.Repository
		var more bool
		var err error
		if method == "personal_token" {
			if r.InstallationId != 0 {
				return errors.New("personal-token discovery does not accept an installation")
			}
			items, more, err = s.Bench.GitHubAPI.UserRepositories(ctx, token, pageNumber(r.Page))
		} else {
			items, more, err = s.Bench.GitHubAPI.Repositories(ctx, token, r.InstallationId, pageNumber(r.Page))
		}
		out.HasMore = more
		for _, item := range items {
			out.Repositories = append(out.Repositories, &pb.GitHubRepository{Id: item.ID, Name: item.Name, FullName: item.FullName, Private: item.Private, DefaultBranch: item.DefaultBranch, HtmlUrl: item.HTMLURL, Archived: item.Archived, RegisteredId: registered[item.ID]})
		}
		return err
	})
	return out, rpcError(err)
}
func (s *Service) ImportGitHubRepository(ctx context.Context, r *pb.ImportGitHubRepositoryRequest) (*pb.ImportGitHubRepositoryResponse, error) {
	if s.Bench.RepositoryImporter == nil {
		return nil, rpcError(errors.New("repository import requires a configured Runner worktree root"))
	}
	ctx, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()
	out := &pb.ImportGitHubRepositoryResponse{}
	err := s.Bench.GitHub.WithCredential(ctx, func(token string) error {
		repo, err := s.Bench.GitHubAPI.Repository(ctx, token, r.RepositoryId)
		if err != nil {
			return err
		}
		if repo.ID != r.RepositoryId {
			return errors.New("GitHub repository identity mismatch")
		}
		local, err := s.Bench.RepositoryImporter.Import(ctx, repo, token)
		if err == nil {
			out.Repository = wireRepository(local, s.Bench.Collections.RepositoryIdentity(local.ID))
		}
		return err
	})
	return out, rpcError(err)
}
func (s *Service) ListGitHubItems(ctx context.Context, r *pb.ListGitHubItemsRequest) (*pb.ListGitHubItemsResponse, error) {
	enrolled := false
	if s.Bench.Collections != nil {
		for _, repo := range s.Bench.Collections.Repositories() {
			if repo.GitHubID == r.RepositoryId && r.RepositoryId > 0 {
				enrolled = true
			}
		}
	}
	if !enrolled {
		return nil, rpcError(errors.New("add this repository to the library before reading its issues and pull requests"))
	}
	out := &pb.ListGitHubItemsResponse{}
	err := s.Bench.GitHub.WithCredential(ctx, func(token string) error {
		repo, err := s.Bench.GitHubAPI.Repository(ctx, token, r.RepositoryId)
		if err != nil {
			return err
		}
		if repo.ID != r.RepositoryId {
			return errors.New("GitHub repository identity mismatch")
		}
		items, more, err := s.Bench.GitHubAPI.Items(ctx, token, repo.FullName, pageNumber(r.Page))
		out.HasMore = more
		for _, item := range items {
			kind := "issue"
			if item.PullRequest != nil {
				kind = "pull_request"
			}
			out.Items = append(out.Items, &pb.GitHubItem{Id: item.ID, Number: item.Number, Kind: kind, Title: item.Title, State: item.State, HtmlUrl: item.HTMLURL, Author: item.User.Login, UpdatedAt: item.UpdatedAt})
		}
		return err
	})
	return out, rpcError(err)
}
