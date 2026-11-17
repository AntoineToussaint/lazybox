package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitinspect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func (s *Service) ListWorkspaceChanges(ctx context.Context, r *pb.WorkspaceChangesRequest) (*pb.WorkspaceChanges, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	workspace, err := manager.Get(r.CollectionId)
	if err != nil {
		return nil, rpcError(err)
	}
	result := &pb.WorkspaceChanges{}
	for _, member := range workspace.Members {
		row := &pb.RepositoryChanges{Member: member.Name}
		result.Repositories = append(result.Repositories, row)
		path, err := manager.InspectionPath(ctx, workspace.ID, member.Name)
		if err != nil {
			row.Error = err.Error()
			continue
		}
		state, err := s.gitInspector().Status(ctx, path)
		if err != nil {
			row.Error = err.Error()
			continue
		}
		row.Head = state.Head
		row.Branch = state.Branch
		row.Truncated = state.Truncated
		for _, f := range state.Files {
			row.Files = append(row.Files, &pb.ChangedFile{Path: f.Path, OriginalPath: f.OriginalPath, IndexStatus: f.IndexStatus, WorktreeStatus: f.WorktreeStatus})
		}
		row.Branches, row.DefaultBranch, _ = s.gitInspector().Branches(ctx, path)
		// Push state for the PR loop; a detached HEAD or a repository without
		// origin simply has none.
		publisher := gitpublish.Git{}
		if tracking, err := publisher.Track(ctx, path, member.BaseCommit); err == nil {
			row.Upstream, row.Ahead = tracking.Upstream, uint32(tracking.Ahead)
		}
		if origin, err := publisher.Origin(ctx, path); err == nil {
			row.GithubRepository = origin.GitHub
		}
		// What the agent committed matters as much as what it left uncommitted.
		row.Base = compareBase(member.BaseCommit, r.Compare)
		committed, err := s.gitInspector().Committed(ctx, path, row.Base)
		if err != nil {
			row.CommittedError = err.Error()
			continue
		}
		row.CommittedTruncated = committed.Truncated
		for _, f := range committed.Files {
			row.Committed = append(row.Committed, &pb.ChangedFile{Path: f.Path, OriginalPath: f.OriginalPath, IndexStatus: f.IndexStatus})
		}
		for _, c := range committed.Commits {
			row.Commits = append(row.Commits, &pb.Commit{Id: c.ID, Subject: c.Subject, CommittedAtUnix: c.Time})
		}
	}
	return result, nil
}
func (s *Service) ReadWorkspaceDiff(ctx context.Context, r *pb.WorkspaceDiffRequest) (*pb.WorkspaceDiff, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	path, err := manager.InspectionPath(ctx, r.CollectionId, r.Member)
	if err != nil {
		return nil, rpcError(err)
	}
	var diff gitinspect.Diff
	if r.Committed {
		workspace, err := manager.Get(r.CollectionId)
		if err != nil {
			return nil, rpcError(err)
		}
		base := ""
		for _, member := range workspace.Members {
			if member.Name == r.Member {
				base = compareBase(member.BaseCommit, r.Compare)
			}
		}
		diff, err = s.gitInspector().CommittedDiff(ctx, path, base, r.Path)
		if err != nil {
			return nil, rpcError(err)
		}
		return &pb.WorkspaceDiff{Text: diff.Text, Truncated: diff.Truncated, Binary: diff.Binary}, nil
	}
	diff, err = s.gitInspector().Diff(ctx, path, r.Path, r.Staged)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.WorkspaceDiff{Text: diff.Text, Truncated: diff.Truncated, Binary: diff.Binary}, nil
}

// compareBase is the ref a branch's commits are compared with: the requested
// one, or the commit the workspace started from.
func compareBase(start, compare string) string {
	if compare != "" {
		return compare
	}
	return start
}

func (s *Service) gitInspector() gitinspect.Inspector {
	if s.Inspector != nil {
		return s.Inspector
	}
	return gitinspect.Git{}
}
