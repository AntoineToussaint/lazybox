package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/gitinspect"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
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
	diff, err := s.gitInspector().Diff(ctx, path, r.Path, r.Staged)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.WorkspaceDiff{Text: diff.Text, Truncated: diff.Truncated, Binary: diff.Binary}, nil
}

func (s *Service) gitInspector() gitinspect.Inspector {
	if s.Inspector != nil {
		return s.Inspector
	}
	return gitinspect.Git{}
}
