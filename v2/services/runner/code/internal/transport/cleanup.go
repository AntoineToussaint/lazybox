package transport

import (
	"context"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
)

func (s *Service) PreviewWorkspaceDeletion(ctx context.Context, r *pb.CollectionRequest) (*pb.WorkspaceDeletionPreview, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	p, err := m.PreviewDeletion(ctx, r.CollectionId, s.Bench.Active)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.WorkspaceDeletionPreview{ActiveSessionIds: p.Active, DirtyRepositories: p.Dirty, FilesOutsideRepositories: p.FilesOutsideRepositories, Blocked: p.Blocked}, nil
}
func (s *Service) DeleteWorkspace(ctx context.Context, r *pb.DeleteWorkspaceRequest) (*pb.CollectionResponse, error) {
	c, err := s.Bench.DeleteWorkspace(ctx, r.CollectionId, r.ConfirmName, r.DiscardChanges, r.StopSessions)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}
func (s *Service) StopSessionResources(ctx context.Context, r *pb.SessionRequest) (*pb.SessionResponse, error) {
	if err := s.Bench.StopExecution(ctx, r.SessionId); err != nil {
		return nil, rpcError(err)
	}
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}

func (s *Service) DeleteConversation(ctx context.Context, r *pb.DeleteConversationRequest) (*pb.DeleteConversationResponse, error) {
	if err := s.Bench.DeleteConversation(ctx, r.SessionId, r.StopSessions); err != nil {
		return nil, rpcError(err)
	}
	return &pb.DeleteConversationResponse{}, nil
}
