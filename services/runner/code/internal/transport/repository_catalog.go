package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
)

func (s *Service) GetRepositoryCatalog(ctx context.Context, r *pb.GetRepositoryCatalogRequest) (*pb.RepositoryCatalog, error) {
	rows, sources, err := s.Bench.RepositoryCatalogSnapshot(ctx, r.Refresh)
	if err != nil {
		return nil, rpcError(err)
	}
	out := &pb.RepositoryCatalog{RefreshSeconds: uint32(s.Bench.Config.RepositoryCatalog.RefreshInterval())}
	for _, row := range rows {
		out.Entries = append(out.Entries, &pb.RepositoryCatalogEntry{Identity: row.Identity, Name: row.Name, RepositoryId: row.RepositoryID})
	}
	for _, source := range sources {
		out.Sources = append(out.Sources, &pb.RepositoryCatalogSource{Id: source.ID, State: source.State, Detail: source.Detail, UpdatedAt: source.UpdatedAt})
	}
	return out, nil
}

// ListRepositoryBranches answers New task's "Start from" picker (start_from.go).
func (s *Service) ListRepositoryBranches(ctx context.Context, r *pb.ListRepositoryBranchesRequest) (*pb.RepositoryBranches, error) {
	out, err := s.Bench.RepositoryBranches(ctx, r.RepositoryIdentity, r.Remote)
	if err != nil {
		return nil, rpcError(err)
	}
	return out, nil
}

func (s *Service) PrepareWorkspace(r *pb.PrepareWorkspaceRequest, stream grpc.ServerStreamingServer[pb.WorkspacePreparation]) error {
	progress := func(id, message string) error {
		return stream.Send(&pb.WorkspacePreparation{Identity: id, Message: message})
	}
	var workspace collections.Collection
	var err error
	if r.RepositoryRequestId != "" {
		// The person answers an agent's request: the same addition, then the agent is told.
		workspace, err = s.Bench.AddRequestedRepositories(stream.Context(), r.RequestKey, r.CollectionId, r.RepositoryRequestId, r.RepositoryIdentities, r.StartFrom, progress)
	} else if r.Item != nil && r.CollectionId == "" {
		// A new workspace started from a work item: the item is its origin (items_search.go).
		workspace, err = s.Bench.PrepareWorkspaceFromItem(stream.Context(), r.RequestKey, r.Name, r.Branch, r.RepositoryIdentities, r.StartFrom, r.Item, progress)
	} else {
		workspace, err = s.Bench.PrepareWorkspaceFrom(stream.Context(), r.RequestKey, r.CollectionId, r.Name, r.Branch, r.RepositoryIdentities, r.StartFrom, progress)
	}
	if err != nil {
		var conflict *collections.BranchConflict
		if errors.As(err, &conflict) {
			if sendErr := stream.Send(&pb.WorkspacePreparation{ConflictingRepositoryId: conflict.RepositoryID, ConflictingRepositoryName: conflict.RepositoryName}); sendErr != nil {
				return sendErr
			}
		}
		// The client's error card offers replacing Lazybox's copy, then retries.
		var replaced *runner.RepositoryReplaced
		if errors.As(err, &replaced) {
			_ = stream.Send(&pb.WorkspacePreparation{Identity: replaced.Identity, Replaced: &pb.RepositoryReplacement{Identity: replaced.Identity, Name: replaced.Name, HasCopy: replaced.HasCopy}})
		}
		return rpcError(err)
	}
	return stream.Send(&pb.WorkspacePreparation{Message: "Ready", Collection: wireCollection(workspace)})
}

// RefreshRepositoryIdentity replaces Lazybox's copy of a repository whose
// history changed on GitHub; without confirm, says what that changes.
func (s *Service) RefreshRepositoryIdentity(ctx context.Context, r *pb.RefreshRepositoryIdentityRequest) (*pb.RepositoryIdentityRefresh, error) {
	refresh, err := s.Bench.RefreshRepositoryIdentity(ctx, r.Identity, r.Confirm, nil)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.RepositoryIdentityRefresh{Identity: refresh.Identity, Name: refresh.Name, Reason: refresh.Reason, HasCopy: refresh.HasCopy, Workspaces: refresh.Workspaces, Replaced: refresh.Replaced, RepositoryId: refresh.RepositoryID}, nil
}

func (s *Service) DeleteLocalBranch(ctx context.Context, r *pb.DeleteLocalBranchRequest) (*pb.DeleteLocalBranchResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	if err := m.DeleteLocalBranch(ctx, r.RepositoryId, r.Branch); err != nil {
		return nil, rpcError(err)
	}
	return &pb.DeleteLocalBranchResponse{}, nil
}
