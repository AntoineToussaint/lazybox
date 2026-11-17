package transport

import (
	"context"
	"errors"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/availability"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// The owner reads which providers can run: every provider, in order, with
// what is missing and how to install it. Programs are probed once until a
// refresh asks again.
func TestOwnerReadsProviderAvailability(t *testing.T) {
	f := newAuthorityFixture(t)
	probes := 0
	f.bench.Availability = &availability.Cache{Probe: availability.Probe{
		LookPath: func(name string) (string, error) { return "", errors.New("not found") },
		Version:  func(context.Context, string) (string, error) { probes++; return "", errors.New("unreachable") },
	}}
	ctx := bearer(timeout(t), ownerToken)
	report, err := f.client.GetProviderAvailability(ctx, &pb.ProviderAvailabilityRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if len(report.Providers) != 2 || report.Providers[0].Provider != "claude" || report.Providers[1].Provider != "codex" || report.CheckedAtUnixMillis == 0 {
		t.Fatalf("report: %+v", report)
	}
	for _, p := range report.Providers {
		if p.State != "not_installed" || p.Installed || p.InstallCommand == "" || p.Error == "" || p.Title == "" {
			t.Fatalf("%s: %+v", p.Provider, p)
		}
		// Nothing lists models: the built-in ones, so /model always has a choice.
		if len(p.Models) < 2 || p.Models[0].Value == "" || p.Models[0].DisplayName == "" {
			t.Fatalf("%s models: %+v", p.Provider, p.Models)
		}
		// The Runner's default profiles are listed with their state.
		if len(p.Subscriptions) != 1 || p.Subscriptions[0].ProfileId != p.Provider+"-default" || p.Subscriptions[0].State == "" || p.Subscriptions[0].Isolation != "inherited" {
			t.Fatalf("%s subscriptions: %+v", p.Provider, p.Subscriptions)
		}
	}
	again, err := f.client.GetProviderAvailability(ctx, &pb.ProviderAvailabilityRequest{})
	if err != nil || again.CheckedAtUnixMillis != report.CheckedAtUnixMillis {
		t.Fatalf("a second read probes again: %v %v", again, err)
	}
	if _, err = f.client.GetProviderAvailability(ctx, &pb.ProviderAvailabilityRequest{Refresh: true}); err != nil {
		t.Fatal(err)
	}
	if probes != 0 {
		t.Fatal("a missing program was run")
	}
}

// Program paths and account addresses are the owner's: no scoped credential
// may read them, and nothing unauthenticated.
func TestProviderAvailabilityIsOwnerOnly(t *testing.T) {
	method := pb.RunnerService_GetProviderAvailability_FullMethodName
	_, agent := agentRPCs[method]
	_, terminal := terminalRPCs[method]
	_, device := deviceRPCs[method]
	if agent || terminal || device {
		t.Fatalf("%s is allowlisted for a scoped credential", method)
	}
	f := newAuthorityFixture(t)
	ctx := timeout(t)
	_, _, token := f.open(t)
	if _, err := f.client.GetProviderAvailability(bearer(ctx, token), &pb.ProviderAvailabilityRequest{}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("agent credential got %v, want PermissionDenied", err)
	}
	if _, err := f.client.GetProviderAvailability(ctx, &pb.ProviderAvailabilityRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("no credential got %v, want Unauthenticated", err)
	}
}
