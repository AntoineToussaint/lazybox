package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// Home checkouts (runner/homes.go). Every RPC here is the owner's: agents
// learn their workspace's homes from ListWorkspaceRepositories and can
// neither create, change nor open one.

func wireHome(h runner.RepositoryHome) *pb.RepositoryHome {
	return &pb.RepositoryHome{RepositoryId: h.RepositoryID, Name: h.Name, Path: h.Path, Kind: h.Kind, Exists: h.Exists, Branch: h.Branch, Detached: h.Detached, Dirty: uint32(h.Dirty), Error: h.Error, SuggestedPath: h.Suggested, Head: h.Head, DefaultBranch: h.DefaultBranch, Behind: uint32(h.Behind)}
}

func (s *Service) UpdateRepositoryHome(ctx context.Context, r *pb.UpdateRepositoryHomeRequest) (*pb.RepositoryHomeUpdate, error) {
	if r.RepositoryId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("repository required"))
	}
	h, updated, message, err := s.Bench.UpdateRepositoryHome(ctx, r.RepositoryId)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.RepositoryHomeUpdate{Home: wireHome(h), Updated: updated, Message: message}, nil
}

func (s *Service) ListRepositoryHomes(ctx context.Context, r *pb.ListRepositoryHomesRequest) (*pb.RepositoryHomes, error) {
	list, err := s.Bench.RepositoryHomes(ctx, r.RepositoryIds)
	if err != nil {
		return nil, rpcError(err)
	}
	out := &pb.RepositoryHomes{Root: s.Bench.HomeRoot()}
	for _, h := range list {
		out.Homes = append(out.Homes, wireHome(h))
	}
	return out, nil
}

func (s *Service) SetRepositoryHome(ctx context.Context, r *pb.SetRepositoryHomeRequest) (*pb.RepositoryHome, error) {
	if r.RepositoryId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("repository required"))
	}
	h, err := s.Bench.SetRepositoryHome(ctx, r.RepositoryId, r.Action, r.Path)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireHome(h), nil
}

func (s *Service) OpenRepositoryHome(ctx context.Context, r *pb.OpenRepositoryHomeRequest) (*pb.OpenRepositoryHomeResponse, error) {
	if r.RepositoryId == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("repository required"))
	}
	if err := s.Bench.OpenRepositoryHome(ctx, r.RepositoryId, r.App); err != nil {
		return nil, rpcError(err)
	}
	return &pb.OpenRepositoryHomeResponse{}, nil
}
