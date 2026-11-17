package transport

import (
	"context"

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
func (s *Service) PrepareWorkspace(r *pb.PrepareWorkspaceRequest, stream grpc.ServerStreamingServer[pb.WorkspacePreparation]) error {
	progress := func(id, message string) error {
		return stream.Send(&pb.WorkspacePreparation{Identity: id, Message: message})
	}
	var workspace collections.Collection
	var err error
	if r.RepositoryRequestId != "" {
		// The person answers an agent's request: the same addition, then the agent is told.
		workspace, err = s.Bench.AddRequestedRepositories(stream.Context(), r.RequestKey, r.CollectionId, r.RepositoryRequestId, r.RepositoryIdentities, progress)
	} else if r.Item != nil && r.CollectionId == "" {
		// A new workspace started from a work item: the item is its origin (items_search.go).
		workspace, err = s.Bench.PrepareWorkspaceFromItem(stream.Context(), r.RequestKey, r.Name, r.Branch, r.RepositoryIdentities, r.Item, progress)
	} else {
		workspace, err = s.Bench.PrepareWorkspace(stream.Context(), r.RequestKey, r.CollectionId, r.Name, r.Branch, r.RepositoryIdentities, progress)
	}
	if err != nil {
		return rpcError(err)
	}
	return stream.Send(&pb.WorkspacePreparation{Message: "Ready", Collection: wireCollection(workspace)})
}
