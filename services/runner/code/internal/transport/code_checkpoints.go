package transport

import (
	"context"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

// checkpointError maps a missing turn to NotFound; the rest are preconditions.
func checkpointError(err error) error {
	if errors.Is(err, codecheckpoints.ErrNoCheckpoint) {
		return grpcError(codes.NotFound, err)
	}
	return rpcError(err)
}

func wireRevert(r codecheckpoints.Revert) *pb.CodeRevert {
	return &pb.CodeRevert{Id: r.ID, Kind: r.Kind, Turn: uint32(r.Turn), UndoOf: r.UndoOf, UndoneBy: r.UndoneBy, AtUnixMillis: r.At.UnixMilli(), Files: uint32(r.Files), Problems: r.Problems}
}

func wireTurnFiles(files []codecheckpoints.FileChange) []*pb.TurnFile {
	out := make([]*pb.TurnFile, 0, len(files))
	for _, f := range files {
		out = append(out, &pb.TurnFile{Path: f.Path, OriginalPath: f.OriginalPath, Status: f.Status, Additions: uint32(f.Additions), Deletions: uint32(f.Deletions), Binary: f.Binary})
	}
	return out
}

func (s *Service) ListTurnCheckpoints(ctx context.Context, r *pb.TurnCheckpointsRequest) (*pb.TurnCheckpoints, error) {
	rec, err := s.Bench.CodeCheckpoints(r.SessionId)
	if err != nil {
		return nil, checkpointError(err)
	}
	out := &pb.TurnCheckpoints{NotePending: rec.Note != ""}
	if r.BeforePromptCursor != 0 {
		reason := ""
		if err := s.Bench.CheckRestartBoundary(ctx, r.SessionId, r.BeforePromptCursor); err != nil {
			reason = err.Error()
		}
		out.RestartBoundaryError = &reason
	}
	for _, t := range rec.Turns {
		turn := &pb.TurnCheckpoint{Turn: uint32(t.Seq), OperationId: t.OperationID, Prompt: t.Prompt, StartedAtUnixMillis: t.Before.At.UnixMilli(), Files: uint32(t.Files), Additions: uint32(t.Additions), Deletions: uint32(t.Deletions)}
		if t.After != nil {
			turn.EndedAtUnixMillis = t.After.At.UnixMilli()
		}
		for _, p := range t.Before.Repos {
			turn.Before = append(turn.Before, &pb.RepositoryCheckpoint{Member: p.Member, Commit: p.Commit, Branch: p.Branch, Skipped: p.Skipped, SkippedCount: uint32(p.SkippedCount), Error: p.Error})
		}
		out.Turns = append(out.Turns, turn)
	}
	for _, r := range rec.Reverts {
		out.Reverts = append(out.Reverts, wireRevert(r))
	}
	return out, nil
}

func (s *Service) ListTurnChanges(ctx context.Context, r *pb.TurnChangesRequest) (*pb.TurnChanges, error) {
	repos, inProgress, err := s.Bench.TurnChanges(ctx, r.SessionId, int(r.Turn))
	if err != nil {
		return nil, checkpointError(err)
	}
	out := &pb.TurnChanges{InProgress: inProgress}
	for _, repo := range repos {
		out.Repositories = append(out.Repositories, &pb.TurnRepositoryChanges{Member: repo.Member, Files: wireTurnFiles(repo.Files), Truncated: repo.Truncated, Error: repo.Error})
	}
	return out, nil
}

func (s *Service) ReadTurnDiff(ctx context.Context, r *pb.TurnDiffRequest) (*pb.WorkspaceDiff, error) {
	if r.Member == "" || r.Path == "" {
		return nil, grpcError(codes.InvalidArgument, errors.New("member and path required"))
	}
	diff, err := s.Bench.TurnDiff(ctx, r.SessionId, int(r.Turn), r.Member, r.Path, r.OriginalPath)
	if err != nil {
		return nil, checkpointError(err)
	}
	return &pb.WorkspaceDiff{Text: diff.Text, Truncated: diff.Truncated, Binary: diff.Binary}, nil
}

func (s *Service) PreviewRevertCode(ctx context.Context, r *pb.RevertCodeRequest) (*pb.RevertCodePreview, error) {
	repos, blocked, err := s.Bench.RevertPreview(ctx, r.SessionId, int(r.Turn))
	if err != nil {
		return nil, checkpointError(err)
	}
	out := &pb.RevertCodePreview{Blocked: blocked}
	for _, repo := range repos {
		out.Repositories = append(out.Repositories, &pb.RevertRepositoryPreview{Member: repo.Member, Files: wireTurnFiles(repo.Files), Truncated: repo.Truncated, CommitsSince: uint32(repo.CommitsSince), BranchThen: repo.BranchThen, BranchNow: repo.BranchNow, Error: repo.Error})
	}
	return out, nil
}

func (s *Service) RevertCode(ctx context.Context, r *pb.RevertCodeRequest) (*pb.CodeRevert, error) {
	revert, err := s.Bench.RevertCode(ctx, r.SessionId, int(r.Turn))
	if err != nil {
		return nil, checkpointError(err)
	}
	return wireRevert(revert), nil
}

func (s *Service) UndoRevertCode(ctx context.Context, r *pb.UndoRevertCodeRequest) (*pb.CodeRevert, error) {
	revert, err := s.Bench.UndoRevertCode(ctx, r.SessionId, r.RevertId)
	if err != nil {
		return nil, checkpointError(err)
	}
	return wireRevert(revert), nil
}
