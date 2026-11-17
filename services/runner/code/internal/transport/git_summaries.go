package transport

import (
	"context"
	"sync"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// The sidebar's git state. ListWorkspaceGitSummaries is polled by every
// window, so it stays cheap: a few local Git processes per repository, at most
// a handful at a time, and never a network call on the request path. Pull
// requests come from the tracked items (runner/items.go): each branch a
// summary reads is recorded in the workspace's branch history, and a pushed
// one is asked about in the background, so the next poll has its pull request.

const (
	summaryWorkspaceLimit = 200
	summaryParallel       = 4
)

type summarizer interface {
	Summary(ctx context.Context, root, base string) (gitinspect.Summary, error)
}

// changeCounter counts a branch's lines, when a request asks for them.
type changeCounter interface {
	Changes(ctx context.Context, root, against, start string) (gitinspect.Changes, error)
}

func (s *Service) ListWorkspaceGitSummaries(ctx context.Context, r *pb.WorkspaceGitSummariesRequest) (*pb.WorkspaceGitSummaries, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	var workspaces []collections.Collection
	if len(r.CollectionIds) == 0 {
		workspaces = manager.List()
	} else {
		seen := map[string]bool{}
		for _, id := range r.CollectionIds {
			if seen[id] {
				continue
			}
			seen[id] = true
			// A workspace deleted since the window listed them is simply absent.
			if w, err := manager.Get(id); err == nil && w.State != "deleted" {
				workspaces = append(workspaces, w)
			}
		}
	}
	if len(workspaces) > summaryWorkspaceLimit {
		workspaces = workspaces[:summaryWorkspaceLimit]
	}
	git, ok := s.gitInspector().(summarizer)
	if !ok {
		git = gitinspect.Git{}
	}
	out := &pb.WorkspaceGitSummaries{}
	type job struct {
		workspace collections.Collection
		member    collections.Member
		row       *pb.RepositoryGitSummary
	}
	var jobs []job
	for _, w := range workspaces {
		summary := &pb.WorkspaceGitSummary{CollectionId: w.ID}
		out.Workspaces = append(out.Workspaces, summary)
		for _, m := range w.Members {
			row := &pb.RepositoryGitSummary{Member: m.Name}
			summary.Repositories = append(summary.Repositories, row)
			jobs = append(jobs, job{w, m, row})
		}
	}
	slots := make(chan struct{}, summaryParallel)
	var wg sync.WaitGroup
	for _, j := range jobs {
		wg.Add(1)
		slots <- struct{}{}
		go func(j job) {
			defer wg.Done()
			defer func() { <-slots }()
			row := j.row
			path, err := manager.InspectionPath(ctx, j.workspace.ID, j.member.Name)
			if err != nil {
				row.Error = err.Error()
				return
			}
			state, err := git.Summary(ctx, path, j.member.BaseCommit)
			if err != nil {
				row.Error = err.Error()
				return
			}
			row.Branch, row.Head = state.Branch, state.Head
			row.Upstream, row.Ahead, row.Behind = state.Upstream, uint32(state.Ahead), uint32(state.Behind)
			row.DefaultBranch, row.AheadOfDefault, row.BehindDefault = state.Default, uint32(state.AheadOfDefault), uint32(state.BehindOfDefault)
			row.Dirty, row.DirtyTruncated = uint32(state.Dirty), state.DirtyTruncated
			if counter, ok := git.(changeCounter); ok && r.Changes {
				// A count that fails leaves the rest of the summary as it is: the card shows none.
				if changes, err := counter.Changes(ctx, path, state.Default, j.member.BaseCommit); err == nil {
					row.Changes = &pb.LineChanges{Base: changes.Base, Files: uint32(changes.Files), Additions: uint32(changes.Additions), Deletions: uint32(changes.Deletions), Truncated: changes.Truncated}
				}
			}
			if origin, err := (gitpublish.Git{}).Origin(ctx, path); err == nil {
				row.GithubRepository = origin.GitHub
			}
			if row.Branch == "" || row.GithubRepository == "" {
				return
			}
			s.Bench.SeeWorkspaceBranch(j.workspace.ID, j.member.Name, row.GithubRepository, row.Branch, row.Upstream != "", row.DefaultBranch)
			if store := s.Bench.WorkItems(); store != nil {
				if item, ok := store.BranchPullRequest(j.workspace.ID, row.GithubRepository, row.Branch); ok {
					row.PullRequest = pullSummary(item)
				}
			}
		}(j)
	}
	wg.Wait()
	return out, nil
}
