package transport

import (
	"context"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// The process RPCs are the owner's alone: they are on neither the agent nor the
// terminal allowlist (auth.go), so a scoped credential gets PermissionDenied.

func (s *Service) ListProcesses(ctx context.Context, _ *pb.ListProcessesRequest) (*pb.ProcessInventory, error) {
	return s.processInventory(ctx), nil
}

func (s *Service) StopProcess(ctx context.Context, r *pb.StopProcessRequest) (*pb.ProcessInventory, error) {
	if err := s.Bench.StopProcess(ctx, r.Id); err != nil {
		return nil, rpcError(err)
	}
	return s.processInventory(ctx), nil
}

func (s *Service) CleanUpOrphanProcesses(ctx context.Context, _ *pb.CleanUpOrphanProcessesRequest) (*pb.CleanUpOrphanProcessesResponse, error) {
	stopped, err := s.Bench.CleanUpOrphanProcesses(ctx)
	response := &pb.CleanUpOrphanProcessesResponse{Stopped: uint32(stopped), Inventory: s.processInventory(ctx)}
	if err != nil {
		response.Problem = err.Error()
	}
	return response, nil
}

// processInventory never fails: a host it could not list is reported in
// Problem, next to every process it could.
func (s *Service) processInventory(ctx context.Context) *pb.ProcessInventory {
	inventory, err := s.Bench.Processes(ctx)
	out := &pb.ProcessInventory{Hosting: processHosting(inventory.Hosting)}
	if err != nil {
		out.Problem = err.Error()
	}
	for _, p := range inventory.Processes {
		out.Processes = append(out.Processes, hostedProcess(p))
	}
	return out
}

func processHosting(h runner.HostingState) *pb.ProcessHosting {
	return &pb.ProcessHosting{Selected: h.Selected, Configured: h.Configured, TmuxVersion: h.TmuxVersion, TmuxProblem: h.TmuxProblem, Fallback: h.Fallback}
}

func hostedProcess(p runner.HostedProcess) *pb.HostedProcess {
	out := &pb.HostedProcess{Id: p.ID, Kind: p.Kind, Backend: p.Backend, State: p.State, Attached: p.Attached, HostPid: int32(p.HostPID), ProgramPid: int32(p.ProgramPID), UptimeSeconds: int64(p.Uptime.Seconds()), Label: p.Label, WorkspaceId: p.WorkspaceID, WorkspaceName: p.WorkspaceName, SessionId: p.ExecutionID, ConversationTitle: p.ConversationTitle, Provider: p.Provider, CommandId: p.CommandID, TerminalId: p.TerminalID, AttachCommand: p.AttachCommand}
	out.Ports = p.Ports
	if p.Usage != nil {
		out.Usage = &pb.ProcessUsage{CpuPercent: p.Usage.CPU, MemoryBytes: p.Usage.Memory, ProcessCount: uint32(p.Usage.Processes)}
	}
	return out
}
