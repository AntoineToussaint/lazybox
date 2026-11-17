package transport

import (
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// The owner adds, changes and removes ACP agents; the report says how each
// starts and names its environment, never its values. No scoped credential
// may touch them: a command line can run anything.
func TestOwnerManagesACPAgents(t *testing.T) {
	for _, method := range []string{pb.RunnerService_SaveAcpAgent_FullMethodName, pb.RunnerService_RemoveAcpAgent_FullMethodName} {
		_, agent := agentRPCs[method]
		_, terminal := terminalRPCs[method]
		_, device := deviceRPCs[method]
		if agent || terminal || device {
			t.Fatalf("%s is allowlisted for a scoped credential", method)
		}
	}
	f := newAuthorityFixture(t)
	ctx := bearer(timeout(t), ownerToken)
	report, err := f.client.SaveAcpAgent(ctx, &pb.SaveAcpAgentRequest{Title: "Team agent", Command: "/bin/sh", Args: []string{"-c", "exit 0"}, Env: []*pb.AcpEnvVar{{Name: "TEAM_TOKEN", Value: "secret-value"}}})
	if err != nil {
		t.Fatal(err)
	}
	var found *pb.ProviderStatus
	for _, p := range report.Providers {
		if p.Provider == "acp:team-agent" {
			found = p
		}
	}
	if found == nil || found.Title != "Team agent" || found.Acp.GetCommand() != "/bin/sh" || len(found.Acp.GetEnvNames()) != 1 || found.Acp.EnvNames[0] != "TEAM_TOKEN" || !found.Acp.Custom || !found.Installed {
		t.Fatalf("saved agent: %+v", found)
	}
	if strings.Contains(report.String(), "secret-value") {
		t.Fatal("an environment value left the Runner")
	}
	info, err := f.client.Info(ctx, &pb.InfoRequest{})
	if err != nil {
		t.Fatal(err)
	}
	offered := false
	for _, p := range info.Profiles {
		offered = offered || (p.Id == "acp-team-agent" && p.Provider == "acp:team-agent" && p.Label == "Team agent")
	}
	if !offered {
		t.Fatalf("the new agent is not offered: %+v", info.Profiles)
	}
	if _, err = f.client.SaveAcpAgent(ctx, &pb.SaveAcpAgentRequest{Title: "Broken", Command: "relative/path"}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("an invalid command line got %v", err)
	}
	if _, err = f.client.RemoveAcpAgent(ctx, &pb.RemoveAcpAgentRequest{Id: "team-agent"}); err != nil {
		t.Fatal(err)
	}
	if _, err = f.client.RemoveAcpAgent(ctx, &pb.RemoveAcpAgentRequest{Id: "team-agent"}); err == nil {
		t.Fatal("removed an agent twice")
	}
	_, _, token := f.open(t)
	if _, err = f.client.SaveAcpAgent(bearer(timeout(t), token), &pb.SaveAcpAgentRequest{Title: "x", Command: "x"}); status.Code(err) != codes.PermissionDenied {
		t.Fatalf("an agent credential got %v, want PermissionDenied", err)
	}
}
