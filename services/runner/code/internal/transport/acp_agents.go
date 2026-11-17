package transport

import (
	"context"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/acp"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// SaveAcpAgent and RemoveAcpAgent are the owner's alone, like the
// availability report they answer with: a command line can carry anything,
// and environment values are credentials. They are on none of the agent,
// terminal, hook or device allowlists (auth.go, device_auth.go).
func (s *Service) SaveAcpAgent(ctx context.Context, r *pb.SaveAcpAgentRequest) (*pb.ProviderAvailability, error) {
	a := acp.Agent{ID: r.Id, Title: r.Title, Command: r.Command, Args: r.Args}
	if len(r.Env) > 0 {
		a.Env = map[string]string{}
		for _, e := range r.Env {
			a.Env[e.Name] = e.Value
		}
	}
	if _, err := s.Bench.SaveACPAgent(a); err != nil {
		return nil, rpcError(err)
	}
	return s.availabilityAfterChange(ctx)
}

func (s *Service) RemoveAcpAgent(ctx context.Context, r *pb.RemoveAcpAgentRequest) (*pb.ProviderAvailability, error) {
	if err := s.Bench.RemoveACPAgent(r.Id); err != nil {
		return nil, rpcError(err)
	}
	return s.availabilityAfterChange(ctx)
}

// availabilityAfterChange probes the programs again: a changed command is
// another program.
func (s *Service) availabilityAfterChange(ctx context.Context) (*pb.ProviderAvailability, error) {
	report, err := s.Bench.ProviderAvailability(ctx, true)
	if err != nil {
		return nil, rpcError(err)
	}
	return wireAvailability(report), nil
}
