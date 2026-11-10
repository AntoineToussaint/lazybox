package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"google.golang.org/grpc/codes"
)

func (s *Service) ReadWorkspaceFile(ctx context.Context, r *pb.WorkspaceFileRequest) (*pb.WorkspaceFile, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	file, err := manager.ReadFile(ctx, r.CollectionId, r.Path)
	if err != nil {
		return nil, rpcError(err)
	}
	out := &pb.WorkspaceFile{Path: file.Path, Text: file.Text, Truncated: file.Truncated, Binary: file.Binary, Version: file.Version, Directory: file.Directory}
	for _, entry := range file.Entries {
		out.Entries = append(out.Entries, &pb.WorkspaceDirectoryEntry{Name: entry.Name, Path: entry.Path, Directory: entry.Directory})
	}
	return out, nil
}

func (s *Service) SaveWorkspaceFile(ctx context.Context, r *pb.SaveWorkspaceFileRequest) (*pb.WorkspaceFile, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	file, err := manager.SaveFile(ctx, r.CollectionId, r.Path, r.Text, r.ExpectedVersion)
	if errors.Is(err, collections.ErrFileChanged) {
		return nil, grpcError(codes.Aborted, err)
	}
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.WorkspaceFile{Path: file.Path, Text: file.Text, Truncated: file.Truncated, Binary: file.Binary, Version: file.Version}, nil
}
