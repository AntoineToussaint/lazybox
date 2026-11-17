package transport

import (
	"context"
	"encoding/json"
	"errors"
	"os"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/process"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// The orchestration process RPCs are the owner's alone: they are on neither
// the agent nor the terminal allowlist (auth.go). A process names what agents
// may do; an agent reading or (later) proposing it goes through scopes.

func (s *Service) GetOrchestrationProcess(_ context.Context, r *pb.OrchestrationProcessRequest) (*pb.OrchestrationProcess, error) {
	resolved, err := s.orchestrationProcess(r)
	if err != nil {
		return nil, err
	}
	e := resolved.Effective
	out := &pb.OrchestrationProcess{Preset: e.Preset, Valid: e.Valid(), Digest: e.Digest, Issues: wireIssues(e.Issues), Notice: resolved.Notice, UserFile: resolved.UserFile}
	for _, l := range e.Layers {
		out.Layers = append(out.Layers, &pb.OrchestrationProcessLayer{Kind: string(l.Kind), Source: l.Source, Digest: l.Digest})
	}
	for _, c := range resolved.Candidates {
		out.Candidates = append(out.Candidates, &pb.OrchestrationProcessCandidate{RepositoryId: c.RepositoryID, Name: c.Name, Path: c.Path, Digest: c.Digest})
	}
	ceiling, err := json.Marshal(e.Ceiling)
	if err != nil {
		return nil, grpcError(codes.Internal, err)
	}
	out.CeilingJson = string(ceiling)
	if e.Process != nil {
		yaml, err := e.Process.YAML()
		if err != nil {
			return nil, grpcError(codes.Internal, err)
		}
		data, err := json.Marshal(e.Process)
		if err != nil {
			return nil, grpcError(codes.Internal, err)
		}
		out.Yaml, out.Json = string(yaml), string(data)
	}
	return out, nil
}

func (s *Service) ExplainOrchestrationProcess(_ context.Context, r *pb.ExplainOrchestrationProcessRequest) (*pb.OrchestrationProcessExplanation, error) {
	request := r.Process
	if request == nil {
		request = &pb.OrchestrationProcessRequest{}
	}
	resolved, err := s.orchestrationProcess(request)
	if err != nil {
		return nil, err
	}
	e := resolved.Effective
	if e.Process == nil {
		return nil, grpcError(codes.FailedPrecondition, e.Issues.Err())
	}
	reactions, err := e.Process.Reactions(r.EventType)
	if err != nil {
		return nil, grpcError(codes.InvalidArgument, err)
	}
	out := &pb.OrchestrationProcessExplanation{EventType: r.EventType, Text: process.FormatReactions(r.EventType, reactions), Digest: e.Digest, Valid: e.Valid(), Issues: wireIssues(e.Issues)}
	for _, x := range reactions {
		out.Reactions = append(out.Reactions, &pb.OrchestrationReaction{Kind: x.Kind, Path: x.Path, Condition: x.Condition, Role: x.Role, Permission: x.Permission, Scopes: x.Scopes, Manual: x.Manual, Limit: uint32(x.Limit), To: x.To, Emits: x.Emits, Summary: x.Summary})
	}
	return out, nil
}

func (s *Service) orchestrationProcess(r *pb.OrchestrationProcessRequest) (runner.OrchestrationProcess, error) {
	resolved, err := s.Bench.OrchestrationProcess(r.CollectionId, r.Repository, int(r.Tasks))
	switch {
	case errors.Is(err, runner.ErrNoProcessFile), errors.Is(err, os.ErrNotExist):
		return resolved, grpcError(codes.NotFound, err)
	case err != nil:
		return resolved, grpcError(codes.FailedPrecondition, err)
	}
	return resolved, nil
}

func wireIssues(issues process.Issues) []*pb.OrchestrationProcessIssue {
	var out []*pb.OrchestrationProcessIssue
	for _, i := range issues {
		out = append(out, &pb.OrchestrationProcessIssue{Severity: string(i.Severity), Path: i.Path, Message: i.Message, Layer: i.Layer, Source: i.Source, Line: uint32(i.Line), Column: uint32(i.Column)})
	}
	return out
}
