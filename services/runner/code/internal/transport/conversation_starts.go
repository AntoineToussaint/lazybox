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

// Agents' requests to start a conversation (runner/conversation_starts.go).
// An agent credential reaches RequestConversationStart for its own execution
// and WaitConversationStart for its own requests (auth.go); answering is the
// owner's, or a paired phone's that may respond (device_auth.go).

// ConversationStartCredentials answers the allowlist's check of a wait: the
// execution that made a request. A verifier without it admits no wait.
type ConversationStartCredentials interface {
	ConversationStartSession(request string) (string, error)
}

func wireConversationStart(r runner.ConversationStart) *pb.ConversationStartRequest {
	return &pb.ConversationStartRequest{Id: r.ID, SessionId: r.Session, ConversationId: r.Conversation, CollectionId: r.Workspace, FromTitle: r.From, Title: r.Title, Brief: r.Brief, Reason: r.Reason,
		Workspace: r.Mode, Provider: r.Provider, ProfileId: r.Profile, Model: r.Model, Permission: r.Permission,
		BriefArtifactId: r.Artifact, BriefArtifactTitle: r.ArtifactTitle, BriefArtifactVersion: r.ArtifactVersion, SharedWith: uint32(max(r.SharedWith, 0)),
		State: r.State, DeclineReason: r.DeclineReason, StartedSessionId: r.StartedSession, StartedConversationId: r.StartedConversation, StartedCollectionId: r.StartedWorkspace, StartedTitle: r.StartedTitle,
		Child: r.Child, MissionTitle: r.MissionTitle, CreatedAtUnixMillis: millis(r.CreatedAt), ExpiresAtUnixMillis: millis(r.ExpiresAt), ResolvedAtUnixMillis: millis(r.ResolvedAt)}
}

// conversationStartEvent is a waiting request as a session's pending entry,
// so every client that shows approvals (the chat, the Inbox, a phone's
// notifications) shows it: the request itself is the entry's provider data.
func conversationStartEvent(r runner.ConversationStart) *pb.AgentEvent {
	data, _ := protojson.Marshal(wireConversationStart(r))
	return &pb.AgentEvent{Type: "conversation_request", RequestId: r.ID, Text: "Start a conversation: " + r.Title, ProviderDataJson: data}
}

func (s *Service) RequestConversationStart(ctx context.Context, r *pb.RequestConversationStartRequest) (*pb.ConversationStartRequest, error) {
	if r.SessionId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("session required"))
	}
	request, err := s.Bench.RequestConversationStart(ctx, runner.ConversationStartAsk{Session: r.SessionId, Title: r.Title, Brief: r.Brief, Artifact: r.BriefArtifactId, Reason: r.Reason,
		Workspace: r.Workspace, Provider: r.Provider, Profile: r.ProfileId, Model: r.Model})
	if err != nil {
		return nil, agentError(err)
	}
	return wireConversationStart(request), nil
}

func (s *Service) WaitConversationStart(ctx context.Context, r *pb.WaitConversationStartRequest) (*pb.ConversationStartRequest, error) {
	request, err := s.Bench.WaitConversationStart(ctx, r.RequestId, time.Duration(r.TimeoutSeconds)*time.Second)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireConversationStart(request), nil
}

func (s *Service) AnswerConversationStart(ctx context.Context, r *pb.AnswerConversationStartRequest) (*pb.ConversationStartRequest, error) {
	request, err := s.Bench.AnswerConversationStart(ctx, r.RequestId, runner.ConversationStartAnswer{Decision: r.Decision, Reason: r.Reason, Title: r.Title, Brief: r.Brief, Workspace: r.Workspace, Profile: r.ProfileId, Model: r.Model})
	if err != nil {
		return nil, rpcError(err)
	}
	return wireConversationStart(request), nil
}
