package transport

import (
	"context"
	"errors"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/workitems"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Work items for starting a workspace from one (PROJECTS_AND_INTEGRATIONS.md):
// the picker's search, one item with the context an agent is given, a pasted
// link resolved, and the workspaces already started from an item. Reads only;
// nothing here changes a tracker. Owner-only, like every RPC not in auth.go's
// agent and terminal lists.

// itemTimeout bounds one provider call: a slow tracker degrades the sheet, it never hangs it.
const itemTimeout = 20 * time.Second

func (s *Service) itemProvider(id string) (workitems.Provider, error) {
	provider, err := s.Bench.ItemProvider(id)
	if err != nil {
		return nil, grpcError(codes.InvalidArgument, err)
	}
	if provider.ID() == "github" && !s.githubConnected() {
		return nil, grpcError(codes.FailedPrecondition, workitems.ErrNotConnected)
	}
	return provider, nil
}

func (s *Service) SearchItems(ctx context.Context, r *pb.SearchItemsRequest) (*pb.ItemPage, error) {
	provider, err := s.itemProvider(r.Provider)
	if err != nil {
		return nil, err
	}
	if len(r.Query) > 2048 || len(r.Cursor) > 64 {
		return nil, grpcError(codes.InvalidArgument, errors.New("search query too long"))
	}
	ctx, cancel := context.WithTimeout(ctx, itemTimeout)
	defer cancel()
	page, err := provider.Search(ctx, r.Query, r.Scope, r.Cursor)
	return page, rpcError(err)
}

func (s *Service) GetItem(ctx context.Context, r *pb.ItemRequest) (*pb.ItemDetail, error) {
	if r.Ref == nil || strings.TrimSpace(r.Ref.Key) == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("item reference required"))
	}
	provider, err := s.itemProvider(r.Ref.Provider)
	if err != nil {
		return nil, err
	}
	comments := int(r.Comments)
	if comments == 0 {
		comments = workitems.DefaultComments
	}
	ctx, cancel := context.WithTimeout(ctx, itemTimeout)
	defer cancel()
	detail, err := provider.Get(ctx, r.Ref.Key, comments)
	if err != nil {
		return nil, rpcError(err)
	}
	return detail.Wire(), nil
}

func (s *Service) ResolveItemUrl(_ context.Context, r *pb.ResolveItemUrlRequest) (*pb.ItemRef, error) {
	ref, err := s.Bench.ResolveItem(r.Url)
	if err != nil {
		return nil, grpcError(codes.InvalidArgument, err)
	}
	return ref, nil
}

func (s *Service) FindItemWorkspaces(_ context.Context, r *pb.ItemRequest) (*pb.ItemWorkspaces, error) {
	if r.Ref == nil || strings.TrimSpace(r.Ref.Key) == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("item reference required"))
	}
	m, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	provider := r.Ref.Provider
	if provider == "" {
		provider = "github"
	}
	out := &pb.ItemWorkspaces{}
	for _, c := range m.StartedFrom(provider, r.Ref.Key) {
		out.Workspaces = append(out.Workspaces, wireCollection(c))
	}
	return out, nil
}
