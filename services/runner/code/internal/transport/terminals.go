package transport

import (
	"context"
	"errors"
	"io"
	"math"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
)

func wireTerminal(s terminals.Status) *pb.TerminalStatus {
	w := &pb.TerminalStatus{Id: s.ID, CollectionId: s.Spec.CollectionID, Directory: s.Spec.Directory, Title: s.Spec.Title, Shell: s.Spec.Shell, Cols: uint32(s.Spec.Cols), Rows: uint32(s.Spec.Rows), Offset: s.Offset, Retained: s.Retained, Done: s.Done, Signal: s.Signal, Error: s.Error, CreatedAtUnixMillis: s.CreatedAt.UnixMilli(), CommandEnds: s.CommandEnds, LastCommandStatus: int32(s.LastCommandStatus), PromptMarks: s.Spec.PromptMarks, PromptsReady: s.PromptsReady, Idle: s.Idle()}
	if s.Spec.Place != "" {
		w.Place, w.RepositoryId, w.Cwd = s.Spec.Place, s.Spec.RepositoryID, s.Spec.Cwd
	}
	if s.ExitCode != nil {
		code := int32(*s.ExitCode)
		w.ExitCode = &code
	}
	return w
}

func terminalSize(cols, rows uint32) (uint16, uint16, error) {
	if cols > math.MaxUint16 || rows > math.MaxUint16 || !terminals.ValidSize(uint16(cols), uint16(rows)) {
		return 0, 0, errors.New("terminal size must be 1-1000 columns and 1-500 rows")
	}
	return uint16(cols), uint16(rows), nil
}

func (s *Service) OpenTerminal(ctx context.Context, r *pb.OpenTerminalRequest) (*pb.TerminalStatus, error) {
	cols, rows, err := terminalSize(r.Cols, r.Rows)
	if err != nil {
		return nil, rpcError(err)
	}
	open := func() (*terminals.Terminal, error) {
		if r.RepositoryHome != "" || r.UserHome {
			if r.Directory != "" {
				return nil, errors.New("a terminal starts in a workspace directory or outside the workspace, not both")
			}
			return s.Bench.OpenPlaceTerminal(r.CollectionId, r.RepositoryHome, r.UserHome, cols, rows, r.RequestKey)
		}
		return s.Bench.OpenTerminal(r.CollectionId, r.Directory, cols, rows, r.RequestKey)
	}
	t, err := open()
	if err != nil {
		return nil, rpcError(err)
	}
	return wireTerminal(t.Status()), nil
}

func (s *Service) ListTerminals(ctx context.Context, r *pb.ListTerminalsRequest) (*pb.ListTerminalsResponse, error) {
	w := &pb.ListTerminalsResponse{}
	for _, t := range s.Bench.Terminals.List(r.CollectionId) {
		w.Terminals = append(w.Terminals, wireTerminal(t))
	}
	return w, nil
}

func (s *Service) WriteTerminal(ctx context.Context, r *pb.WriteTerminalRequest) (*pb.TerminalStatus, error) {
	t, err := s.Bench.Terminals.Get(r.TerminalId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = t.Write(r.Data); err != nil {
		return nil, rpcError(err)
	}
	return wireTerminal(t.Status()), nil
}

func (s *Service) ResizeTerminal(ctx context.Context, r *pb.ResizeTerminalRequest) (*pb.TerminalStatus, error) {
	cols, rows, err := terminalSize(r.Cols, r.Rows)
	if err != nil {
		return nil, rpcError(err)
	}
	t, err := s.Bench.Terminals.Get(r.TerminalId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = t.Resize(cols, rows); err != nil {
		return nil, rpcError(err)
	}
	return wireTerminal(t.Status()), nil
}

// CloseTerminal hangs the shell up, waits for it to exit, forgets it and
// revokes its credential.
func (s *Service) CloseTerminal(ctx context.Context, r *pb.TerminalRequest) (*pb.TerminalStatus, error) {
	wait, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()
	t, err := s.Bench.Terminals.Get(r.TerminalId)
	if err != nil {
		return nil, rpcError(err)
	}
	if err = s.Bench.CloseTerminal(wait, r.TerminalId); err != nil {
		return nil, rpcError(err)
	}
	return wireTerminal(t.Status()), nil
}

// WatchTerminal streams output after a byte offset until the shell exits. The
// last event carries the final status and no data.
func (s *Service) WatchTerminal(r *pb.WatchTerminalRequest, stream grpc.ServerStreamingServer[pb.TerminalEvent]) error {
	ctx := stream.Context()
	t, err := s.Bench.Terminals.Get(r.TerminalId)
	if err != nil {
		return rpcError(err)
	}
	after := r.After
	for {
		data, offset, status, err := t.Next(ctx, after)
		if err == io.EOF {
			return stream.Send(&pb.TerminalEvent{Offset: after, Status: wireTerminal(status)})
		}
		if err != nil {
			return rpcError(err)
		}
		if err = stream.Send(&pb.TerminalEvent{Offset: offset, Data: data, Status: wireTerminal(status)}); err != nil {
			return err
		}
		after = offset + uint64(len(data))
	}
}
