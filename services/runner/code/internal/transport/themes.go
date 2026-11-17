package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/themes"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// The theme RPCs are the owner's alone: they are on none of the agent,
// terminal or device allowlists (auth.go, device_auth.go). The folder is
// read on every call, so the client's poll sees an edit without a restart.

func (s *Service) ListThemes(context.Context, *pb.ListThemesRequest) (*pb.ThemeFolder, error) {
	folder, err := themes.List(s.Bench.Config.ThemeDirectory)
	if err != nil {
		return nil, grpcError(codes.Internal, err)
	}
	return wireThemeFolder(folder), nil
}

func (s *Service) CreateTheme(_ context.Context, r *pb.CreateThemeRequest) (*pb.CreateThemeResponse, error) {
	file, path, err := themes.Create(s.Bench.Config.ThemeDirectory, r.Name, r.Content)
	switch {
	case errors.Is(err, themes.ErrInvalid):
		return nil, grpcError(codes.InvalidArgument, err)
	case errors.Is(err, themes.ErrExists):
		return nil, grpcError(codes.AlreadyExists, err)
	case errors.Is(err, themes.ErrFull):
		return nil, grpcError(codes.FailedPrecondition, err)
	case err != nil:
		return nil, grpcError(codes.Internal, err)
	}
	folder, err := themes.List(s.Bench.Config.ThemeDirectory)
	if err != nil {
		return nil, grpcError(codes.Internal, err)
	}
	return &pb.CreateThemeResponse{Folder: wireThemeFolder(folder), File: file, Path: path}, nil
}

func wireThemeFolder(f themes.Folder) *pb.ThemeFolder {
	out := &pb.ThemeFolder{Directory: f.Directory, Revision: f.Revision}
	for _, file := range f.Files {
		out.Files = append(out.Files, &pb.ThemeFile{Name: file.Name, Format: file.Format, Json: file.JSON, Error: file.Error, ModifiedUnixMillis: file.Modified.UnixMilli()})
	}
	return out
}
