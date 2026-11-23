package transport

import (
	"context"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/availability"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// GetProviderAvailability is the owner's alone: it names program paths and
// account addresses, so it is on none of the agent, terminal, hook or device
// allowlists (auth.go, device_auth.go).
func (s *Service) GetProviderAvailability(ctx context.Context, r *pb.ProviderAvailabilityRequest) (*pb.ProviderAvailability, error) {
	report, err := s.Bench.ProviderAvailability(ctx, r.Refresh)
	if err != nil {
		return nil, rpcError(err)
	}
	result := wireAvailability(report)
	if r.GetCheckUpdates() {
		for _, provider := range result.Providers {
			if provider.Installed && provider.Version != "" {
				provider.LatestVersion = s.Bench.ProviderReleases.Latest(ctx, provider.Provider)
			}
		}
	}
	return result, nil
}

func wireAvailability(report availability.Report) *pb.ProviderAvailability {
	w := &pb.ProviderAvailability{CheckedAtUnixMillis: report.CheckedAt.UnixMilli()}
	for _, p := range report.Providers {
		status := &pb.ProviderStatus{Provider: p.Provider, Title: p.Title, State: p.State, Installed: p.Program.Installed, Path: p.Program.Path, Version: p.Program.Version, Source: p.Program.Source, Error: p.Program.Error, InstallCommand: p.InstallCommand}
		if a := p.ACP; a != nil {
			status.Acp = &pb.AcpAgentInfo{Id: a.ID, Icon: a.Icon, Command: a.Command, Args: a.Args, EnvNames: a.Env, Homepage: a.Homepage, SharesCredentialsWith: a.SharesCredentialsWith, InheritedEnvNames: a.InheritedEnv, SignInCommand: a.SignIn, CredentialsNote: a.CredentialsNote, Custom: a.Custom, LaunchProblem: a.LaunchProblem}
		}
		for _, m := range p.Models {
			status.Models = append(status.Models, &pb.ProviderModel{Value: m.Value, DisplayName: m.DisplayName, Description: m.Description, EffortLevels: m.EffortLevels})
		}
		for _, sub := range p.Subscriptions {
			status.Subscriptions = append(status.Subscriptions, &pb.SubscriptionAvailability{ProfileId: sub.ProfileID, Label: sub.Label, State: sub.State, ResetAt: sub.ResetAt, Detail: sub.Detail, Managed: sub.Managed, Isolation: sub.Isolation})
		}
		w.Providers = append(w.Providers, status)
	}
	return w
}
