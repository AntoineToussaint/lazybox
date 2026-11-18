package transport

import (
	"context"
	"errors"
	"slices"
	"strings"
	"sync"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

type releaseWatchSource interface {
	ReleaseWatch(context.Context, string, string) ([]github.ReleaseNotice, error)
}

func (s *Service) ListReleaseWatch(ctx context.Context, _ *pb.ListReleaseWatchRequest) (*pb.ListReleaseWatchResponse, error) {
	source, ok := s.Bench.GitHubAPI.(releaseWatchSource)
	if !ok || s.Bench.GitHub == nil || s.Bench.Collections == nil {
		return nil, rpcError(errors.New("GitHub Release Watch is unavailable on this Runner"))
	}
	byRepository := s.githubWorkspaceIDs()
	repositories := make([]string, 0, len(byRepository))
	for repository := range byRepository {
		repositories = append(repositories, repository)
	}
	slices.Sort(repositories)
	const maxRepositories = 8
	out := &pb.ListReleaseWatchResponse{Truncated: len(repositories) > maxRepositories}
	repositories = repositories[:min(len(repositories), maxRepositories)]
	err := s.Bench.GitHub.WithCredential(ctx, func(token string) error {
		var errorsByRepo []error
		var mu sync.Mutex
		var wait sync.WaitGroup
		for _, repository := range repositories {
			wait.Go(func() {
				rows, err := source.ReleaseWatch(ctx, token, repository)
				mu.Lock()
				defer mu.Unlock()
				if err != nil {
					errorsByRepo = append(errorsByRepo, err)
				}
				for _, row := range rows {
					out.Items = append(out.Items, &pb.ReleaseWatchItem{
						Id: repository + ":" + row.ID, Repository: repository, Environment: row.Environment,
						Reason: row.Reason, Url: row.URL, UpdatedAt: row.UpdatedAt, WorkspaceIds: byRepository[repository],
					})
				}
			})
		}
		wait.Wait()
		if len(out.Items) == 0 && len(errorsByRepo) == len(repositories) && len(repositories) > 0 {
			return errorsByRepo[0]
		}
		out.Truncated = out.Truncated || len(errorsByRepo) > 0
		return nil
	})
	if err != nil {
		return nil, rpcError(err)
	}
	slices.SortFunc(out.Items, func(a, b *pb.ReleaseWatchItem) int {
		if byDate := strings.Compare(b.UpdatedAt, a.UpdatedAt); byDate != 0 {
			return byDate
		}
		return strings.Compare(a.Id, b.Id)
	})
	return out, nil
}

func (s *Service) githubWorkspaceIDs() map[string][]string {
	result := map[string][]string{}
	if manager := s.Bench.Collections; manager != nil {
		identities := map[string]string{}
		for _, workspace := range manager.List() {
			if workspace.State == "deleted" || workspace.State == "deleting" {
				continue
			}
			seen := map[string]bool{}
			for _, member := range workspace.Members {
				repo, cached := identities[member.RepositoryID]
				if !cached {
					identity := strings.ToLower(manager.RepositoryIdentity(member.RepositoryID))
					if name, github := strings.CutPrefix(identity, "github:"); github {
						repo = name
					}
					identities[member.RepositoryID] = repo
				}
				if repo != "" && !seen[repo] {
					result[repo] = append(result[repo], workspace.ID)
					seen[repo] = true
				}
			}
		}
	}
	return result
}
