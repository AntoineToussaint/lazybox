package transport

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
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

func (s *Service) SearchWorkspaceFiles(ctx context.Context, r *pb.SearchWorkspaceFilesRequest) (*pb.WorkspaceFileSearch, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	matches, truncated, indexed, err := manager.SearchFiles(ctx, r.CollectionId, r.Query, int(r.Limit))
	if err != nil {
		return nil, rpcError(err)
	}
	out := &pb.WorkspaceFileSearch{Truncated: truncated, Indexed: uint32(indexed)}
	for _, match := range matches {
		out.Matches = append(out.Matches, &pb.WorkspaceFileMatch{Path: match.Path, Positions: match.Positions})
	}
	return out, nil
}

func (s *Service) ResolveWorkspacePath(ctx context.Context, r *pb.ResolveWorkspacePathRequest) (*pb.WorkspacePathResolution, error) {
	manager, err := s.collectionStore()
	if err != nil {
		return nil, err
	}
	matches, exact, truncated, err := manager.ResolvePath(ctx, r.CollectionId, r.Path)
	if err != nil {
		return nil, rpcError(err)
	}
	out := &pb.WorkspacePathResolution{Exact: exact, Truncated: truncated}
	for _, match := range matches {
		out.Matches = append(out.Matches, &pb.WorkspacePathMatch{Path: match.Path, Directory: match.Directory})
	}
	return out, nil
}
