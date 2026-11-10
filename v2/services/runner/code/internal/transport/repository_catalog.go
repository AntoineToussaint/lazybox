package transport

import (
	"context"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
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
	workspace, err := s.Bench.PrepareWorkspace(stream.Context(), r.RequestKey, r.CollectionId, r.Name, r.RepositoryIdentities, func(id, message string) error {
		return stream.Send(&pb.WorkspacePreparation{Identity: id, Message: message})
	})
	if err != nil {
		return rpcError(err)
	}
	return stream.Send(&pb.WorkspacePreparation{Message: "Ready", Collection: wireCollection(workspace)})
}
