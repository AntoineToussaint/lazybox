package transport

import (
	"context"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/snippets"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

func (s *Service) ListSnippets(ctx context.Context, _ *pb.InfoRequest) (*pb.SnippetCatalog, error) {
	entries, err := (snippets.Command{Binary: s.Bench.Config.SnippetCommand, Directory: s.Bench.Config.SnippetDirectory}).List(ctx)
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
