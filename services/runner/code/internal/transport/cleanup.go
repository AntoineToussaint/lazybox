package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/reviews"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
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
	// Its review comments anchor to worktrees that no longer exist.
	if s.Bench.Reviews != nil {
		_ = s.Bench.Reviews.Forget(r.CollectionId)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}
func (s *Service) StopSessionResources(ctx context.Context, r *pb.SessionRequest) (*pb.SessionResponse, error) {
	if err := s.Bench.StopExecution(ctx, r.SessionId); err != nil {
		return nil, rpcError(err)
	}
	// End session on a reviewer under way stops its review: never left reviewing.
	s.Bench.ReviewerEndedByHand(s.Bench.ReviewerRuns(r.SessionId), reviews.EndedSession)
	e, err := s.Bench.Get(r.SessionId)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.sessionResponse(ctx, e)
}

func (s *Service) DeleteConversation(ctx context.Context, r *pb.DeleteConversationRequest) (*pb.DeleteConversationResponse, error) {
	// Read before the delete, which forgets the conversation.
	reviewing := s.Bench.ReviewerRuns(r.SessionId)
	if err := s.Bench.DeleteConversation(ctx, r.SessionId, r.StopSessions); err != nil {
		return nil, rpcError(err)
	}
	s.Bench.ReviewerEndedByHand(reviewing, reviews.EndedDeleted)
	return &pb.DeleteConversationResponse{}, nil
}

func (s *Service) ScheduleDeletion(ctx context.Context, r *pb.ScheduleDeletionRequest) (*pb.PendingDeletion, error) {
	var entry runner.PendingDeletion
	switch {
	case r.GetWorkspace() != nil && r.GetWorkspace().CollectionId != "":
		w := r.GetWorkspace()
		entry = runner.PendingDeletion{WorkspaceID: w.CollectionId, Title: w.ConfirmName, Stop: w.StopSessions, Discard: w.DiscardChanges}
	case r.GetConversation() != nil && r.GetConversation().SessionId != "":
		c := r.GetConversation()
		entry = runner.PendingDeletion{SessionID: c.SessionId, Stop: c.StopSessions}
	default:
		return nil, rpcError(errors.New("deletion target required"))
	}
	entry, err := s.Bench.ScheduleDeletion(ctx, entry)
	if err != nil {
		return nil, rpcError(err)
	}
	return s.wirePendingDeletion(entry), nil
}

func (s *Service) ListPendingDeletions(context.Context, *pb.ListPendingDeletionsRequest) (*pb.ListPendingDeletionsResponse, error) {
	out := &pb.ListPendingDeletionsResponse{}
	for _, entry := range s.Bench.PendingDeletions() {
		out.Deletions = append(out.Deletions, s.wirePendingDeletion(entry))
	}
	return out, nil
}

func (s *Service) UndoDeletion(ctx context.Context, r *pb.UndoDeletionRequest) (*pb.ListPendingDeletionsResponse, error) {
	if err := s.Bench.UndoDeletion(r.Id); err != nil {
		return nil, rpcError(err)
	}
	return s.ListPendingDeletions(ctx, &pb.ListPendingDeletionsRequest{})
}

func (s *Service) wirePendingDeletion(entry runner.PendingDeletion) *pb.PendingDeletion {
	result := &pb.PendingDeletion{Id: entry.ID, WorkspaceId: entry.WorkspaceID, SessionId: entry.SessionID, Title: entry.Title, ExpiresAtUnixMillis: entry.Expires.UnixMilli(), Failure: entry.Failure}
	if entry.WorkspaceID != "" && s.Bench.Collections != nil {
		if workspace, err := s.Bench.Collections.Get(entry.WorkspaceID); err == nil {
			result.WorkspaceName = workspace.Name
			for _, member := range workspace.Members {
				result.RepositoryNames = append(result.RepositoryNames, member.Name)
			}
		}
	}
	return result
}

func (s *Service) hiddenWorkspaces() map[string]bool {
	hidden := map[string]bool{}
	for _, entry := range s.Bench.PendingDeletions() {
		if entry.SessionID == "" && entry.Failure == "" {
			hidden[entry.WorkspaceID] = true
		}
	}
	return hidden
}

func (s *Service) hiddenConversations() map[string]bool {
	hidden := map[string]bool{}
	for _, entry := range s.Bench.PendingDeletions() {
		if entry.SessionID == "" || entry.Failure != "" {
			continue
		}
		hidden[entry.SessionID] = true
		if entry.ConversationID != "" {
			hidden[entry.ConversationID] = true
		}
	}
	return hidden
}
