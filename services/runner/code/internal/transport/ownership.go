package transport

import (
	"context"
	"errors"

	"google.golang.org/grpc/codes"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// GetOwnership says what a previous Runner left unconfirmed in a workspace or
// a conversation, settling what the Runner proves gone (runner/ownership.go).
func (s *Service) GetOwnership(ctx context.Context, r *pb.OwnershipRequest) (*pb.Ownership, error) {
	var o runner.Ownership
	var err error
	switch {
	case (r.CollectionId == "") == (r.SessionId == ""):
		return nil, grpcError(codes.InvalidArgument, errors.New("name a workspace or a conversation"))
	case r.CollectionId != "":
		o, err = s.Bench.WorkspaceOwnership(ctx, r.CollectionId)
	default:
		o, err = s.Bench.ConversationOwnership(ctx, r.SessionId)
	}
	if err != nil && len(o.Processes) == 0 && !o.Settled {
		return nil, rpcError(err)
	}
	return wireOwnership(o), nil
}

// ResolveOwnership settles it: stops the recorded processes that still run, or
// records the person's word that they stopped.
func (s *Service) ResolveOwnership(ctx context.Context, r *pb.ResolveOwnershipRequest) (*pb.Ownership, error) {
	var o runner.Ownership
	var err error
	switch {
	case (r.CollectionId == "") == (r.SessionId == ""):
		return nil, grpcError(codes.InvalidArgument, errors.New("name a workspace or a conversation"))
	case r.CollectionId != "":
		o, err = s.Bench.ResolveWorkspaceOwnership(ctx, r.CollectionId, r.Stop, r.Reason)
	default:
		o, err = s.Bench.ResolveConversationOwnership(ctx, r.SessionId, r.Stop, r.Reason)
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return wireOwnership(o), nil
}

func wireOwnership(o runner.Ownership) *pb.Ownership {
	w := &pb.Ownership{Settled: o.Settled, Note: o.Note}
	for _, p := range o.Processes {
		w.Processes = append(w.Processes, &pb.OwnedProcess{Kind: p.Kind, SessionId: p.ExecutionID, CommandId: p.CommandID, Label: p.Label, CommandLine: p.CommandLine, Pid: int32(p.PID), State: p.State, Detail: p.Detail})
	}
	return w
}
