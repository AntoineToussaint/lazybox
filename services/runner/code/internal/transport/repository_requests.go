package transport

import (
	"context"
	"errors"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/protobuf/encoding/protojson"
)

// Agents' requests for a repository (runner/repository_requests.go). An
// agent credential reaches ListWorkspaceRepositories for its own workspace,
// RequestRepository for its own execution and WaitRepositoryRequest for its
// own requests (auth.go); declining and adding are the owner's.

func wireWorkspaceRepository(r runner.WorkspaceRepository) *pb.WorkspaceRepository {
	return &pb.WorkspaceRepository{Name: r.Name, Path: r.Path, Branch: r.Branch, Identity: r.Identity, Ready: r.Ready, Home: r.Home, RepositoryId: r.RepositoryID}
}

func wireRepositoryRequest(r runner.RepositoryRequest) *pb.RepositoryRequest {
	w := &pb.RepositoryRequest{Id: r.ID, SessionId: r.Session, ConversationId: r.Conversation, CollectionId: r.Workspace, Query: r.Query, Reason: r.Reason, State: r.State, Branch: r.Branch, DeclineReason: r.DeclineReason, Already: r.Already,
		CreatedAtUnixMillis: millis(r.CreatedAt), ExpiresAtUnixMillis: millis(r.ExpiresAt), ResolvedAtUnixMillis: millis(r.ResolvedAt)}
	for _, c := range r.Candidates {
		w.Candidates = append(w.Candidates, &pb.RepositoryCandidate{Identity: c.Identity, Name: c.Name, Source: c.Source, LocalPath: c.LocalPath, Score: int32(c.Score)})
	}
	for _, a := range r.Added {
		w.Added = append(w.Added, wireWorkspaceRepository(a))
	}
	return w
}

// repositoryRequestEvent is a waiting request as a session's pending entry,
// so every client that shows approvals shows it: the request itself is the
// entry's provider data.
func repositoryRequestEvent(r runner.RepositoryRequest) *pb.AgentEvent {
	text := "Add a repository to this workspace"
	if top, ok := r.Match(); ok {
		text = "Add " + top.Name + " to this workspace"
	}
	data, _ := protojson.Marshal(wireRepositoryRequest(r))
	return &pb.AgentEvent{Type: "repository_request", RequestId: r.ID, Text: text, ProviderDataJson: data}
}

func (s *Service) ListWorkspaceRepositories(_ context.Context, r *pb.ListWorkspaceRepositoriesRequest) (*pb.WorkspaceRepositories, error) {
	if r.CollectionId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("workspace required"))
	}
	workspace, members, requests, err := s.Bench.WorkspaceRepositories(r.CollectionId)
	if err != nil {
		return nil, rpcError(err)
	}
	out := &pb.WorkspaceRepositories{CollectionId: workspace.ID, Name: workspace.Name, Branch: workspace.Branch()}
	for _, m := range members {
		out.Repositories = append(out.Repositories, wireWorkspaceRepository(m))
	}
	for _, request := range requests {
		out.Requests = append(out.Requests, wireRepositoryRequest(request))
	}
	return out, nil
}

func (s *Service) RequestRepository(ctx context.Context, r *pb.RequestRepositoryRequest) (*pb.RepositoryRequest, error) {
	if r.SessionId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("session required"))
	}
	request, err := s.Bench.RequestRepository(ctx, r.SessionId, r.Query, r.Reason)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireRepositoryRequest(request), nil
}

func (s *Service) WaitRepositoryRequest(ctx context.Context, r *pb.WaitRepositoryRequestRequest) (*pb.RepositoryRequest, error) {
	request, err := s.Bench.WaitRepositoryRequest(ctx, r.RequestId, time.Duration(r.TimeoutSeconds)*time.Second)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireRepositoryRequest(request), nil
}

func (s *Service) DeclineRepositoryRequest(_ context.Context, r *pb.DeclineRepositoryRequestRequest) (*pb.RepositoryRequest, error) {
	request, err := s.Bench.DeclineRepositoryRequest(r.RequestId, r.Reason)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireRepositoryRequest(request), nil
}
