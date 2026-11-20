package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/snippets"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func (s *Service) ListSnippets(ctx context.Context, _ *pb.InfoRequest) (*pb.SnippetCatalog, error) {
	entries, err := (snippets.Command{Binary: s.Bench.Config.SnippetCommand, Directory: s.Bench.Config.SnippetDirectory, Packs: s.Bench.Config.SnippetPacks}).List(ctx)
	if err != nil {
		return nil, rpcError(err)
	}
	// Kept to label snippet messages in the prompt log, with their next: chain.
	s.Bench.RememberSnippets(entries)
	out := &pb.SnippetCatalog{}
	for _, v := range entries {
		out.Snippets = append(out.Snippets, &pb.Snippet{Key: v.Key, Category: v.Category, Description: v.Description, Body: v.Body, Origin: v.Origin, Next: v.Next})
	}
	return out, nil
}

func (s *Service) ReviewWorkflow(ctx context.Context, r *pb.ReviewWorkflowRequest) (*pb.ReviewWorkflowResult, error) {
	titles, err := s.Bench.ReviewWorkflow(r.CollectionId, r.SnippetKey, r.Feedback)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.ReviewWorkflowResult{ProposalTitles: titles}, nil
}

func (s *Service) SetWorkflowReviewSchedule(ctx context.Context, r *pb.SetWorkflowReviewScheduleRequest) (*pb.CollectionResponse, error) {
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	c, err := m.SetWorkflowReviewSchedule(r.CollectionId, r.Hourly)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.CollectionResponse{Collection: wireCollection(c)}, nil
}
