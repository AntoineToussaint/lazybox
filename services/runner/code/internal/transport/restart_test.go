package transport

import (
	"encoding/json"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"

	"connectrpc.com/connect"
	"connectrpc.com/vanguard/vanguardgrpc"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/restart"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen/genconnect"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// restartFixture serves a Runner whose launcher will start it again. Stop is
// the shutdown restart.Terminate would begin; here it is only observed.
func restartFixture(t *testing.T) (*authorityFixture, string, chan struct{}) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "restart-request.json")
	stopped := make(chan struct{}, 4)
	supervisor := &restart.Supervisor{Path: path, Stop: func() { stopped <- struct{}{} }}
	return newAuthorityFixture(t, func(s *Service) { s.Restart = supervisor }), path, stopped
}

func TestOwnerRestartsTheRunnerThroughItsLauncher(t *testing.T) {
	f, path, stopped := restartFixture(t)
	ctx := bearer(timeout(t), ownerToken)
	info, err := f.client.Info(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if !info.RestartAvailable || info.InstanceId == "" {
		t.Fatalf("a supervised Runner must offer restart and name its process: %+v", info)
	}
	response, err := f.client.RestartRunner(ctx, &pb.RestartRunnerRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if response.InstanceId != info.InstanceId {
		t.Fatalf("restart names process %q, Info named %q", response.InstanceId, info.InstanceId)
	}
	select {
	case <-stopped:
	case <-time.After(5 * time.Second):
		t.Fatal("the Runner never began its shutdown")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal("the launcher would find no restart request", err)
	}
	var request restart.Request
	if err = json.Unmarshal(data, &request); err != nil || request.Version != restart.Version {
		t.Fatalf("unreadable restart request %s: %v", data, err)
	}
}

// Nothing an agent runs may restart the Runner: the RPC is not on the agent
// allowlist, over gRPC or Connect. Terminals get no Runner credential at all
// (terminals.Environment), so they reach it only unauthenticated.
func TestOnlyTheOwnerMayRestartTheRunner(t *testing.T) {
	f, path, stopped := restartFixture(t)
	ctx := timeout(t)
	_, _, token := f.open(t)
	if _, err := f.client.RestartRunner(bearer(ctx, token), &pb.RestartRunnerRequest{}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("agent credential got %v, want PermissionDenied", err)
	}
	if _, err := f.client.RestartRunner(ctx, &pb.RestartRunnerRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("no credential got %v, want Unauthenticated", err)
	}
	transcoder, err := vanguardgrpc.NewTranscoder(f.server)
	if err != nil {
		t.Fatal(err)
	}
	host := httptest.NewServer(transcoder)
	t.Cleanup(host.Close)
	request := connect.NewRequest(&pb.RestartRunnerRequest{})
	request.Header().Set("Authorization", "Bearer "+token)
	if _, err = genconnect.NewRunnerServiceClient(host.Client(), host.URL).RestartRunner(ctx, request); connect.CodeOf(err) != connect.CodePermissionDenied {
		t.Fatalf("agent credential over Connect got %v, want PermissionDenied", err)
	}
	select {
	case <-stopped:
		t.Fatal("a denied request stopped the Runner")
	case <-time.After(100 * time.Millisecond):
	}
	if _, err = os.Stat(path); !os.IsNotExist(err) {
		t.Fatal("a denied request recorded a restart", err)
	}
}

// Without a launcher nothing would start the Runner again, so the owner is
// told so instead of being left with a stopped Runner.
func TestRestartIsUnavailableWithoutALauncher(t *testing.T) {
	f := newAuthorityFixture(t)
	ctx := bearer(timeout(t), ownerToken)
	info, err := f.client.Info(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	if info.RestartAvailable {
		t.Fatal("an unsupervised Runner offered restart")
	}
	if _, err = f.client.RestartRunner(ctx, &pb.RestartRunnerRequest{}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("unsupervised restart got %v, want FailedPrecondition", err)
	}
}
