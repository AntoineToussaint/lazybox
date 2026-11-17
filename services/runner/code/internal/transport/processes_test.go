package transport

import (
	"context"
	"testing"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// The Processes view can stop any agent, command or shell the Runner hosts: its
// RPCs are the owner's alone. Neither an agent's credential nor a terminal's
// may list, stop or sweep, and a refused stop leaves the process running.
func TestProcessRPCsAreTheOwnersAlone(t *testing.T) {
	f := newTerminalFixture(t)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	owner := bearer(ctx, ownerToken)
	inventory, err := f.client.ListProcesses(owner, &pb.ListProcessesRequest{})
	if err != nil {
		t.Fatal(err)
	}
	var shell *pb.HostedProcess
	for _, p := range inventory.Processes {
		if p.Kind == "terminal" && p.TerminalId == f.terminal {
			shell = p
		}
	}
	if shell == nil || shell.State != "running" || !shell.Attached || shell.WorkspaceId != f.mine.Id || shell.WorkspaceName != "Parser" || shell.HostPid == 0 || shell.Backend != "sessionhost" {
		t.Fatalf("the workspace terminal is not listed as running here: %+v", inventory.Processes)
	}
	if inventory.Hosting == nil || inventory.Hosting.Selected != "sessionhost" {
		t.Fatalf("hosting state missing: %+v", inventory.Hosting)
	}

	for name, token := range map[string]string{"agent": f.agentToken, "terminal": f.terminalToken} {
		scoped := bearer(ctx, token)
		if _, err := f.client.ListProcesses(scoped, &pb.ListProcessesRequest{}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s credential listed processes: %v", name, err)
		}
		if _, err := f.client.StopProcess(scoped, &pb.StopProcessRequest{Id: shell.Id}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s credential stopped a process: %v", name, err)
		}
		if _, err := f.client.CleanUpOrphanProcesses(scoped, &pb.CleanUpOrphanProcessesRequest{}); status.Code(err) != codes.PermissionDenied {
			t.Errorf("%s credential swept processes: %v", name, err)
		}
	}
	terminals, err := f.client.ListTerminals(owner, &pb.ListTerminalsRequest{CollectionId: f.mine.Id})
	if err != nil || len(terminals.Terminals) != 1 || terminals.Terminals[0].Done {
		t.Fatalf("a refused stop reached the shell: %+v %v", terminals, err)
	}

	// The owner's stop closes the terminal the way the panel's × does.
	after, err := f.client.StopProcess(owner, &pb.StopProcessRequest{Id: shell.Id})
	if err != nil {
		t.Fatal(err)
	}
	for _, p := range after.Processes {
		if p.Id == shell.Id && p.State == "running" {
			t.Fatalf("still running after stop: %+v", p)
		}
	}
	if terminals, err = f.client.ListTerminals(owner, &pb.ListTerminalsRequest{CollectionId: f.mine.Id}); err != nil || len(terminals.Terminals) != 0 {
		t.Fatalf("terminal not closed: %+v %v", terminals, err)
	}
	if _, err = f.client.StopProcess(owner, &pb.StopProcessRequest{Id: shell.Id}); status.Code(err) != codes.NotFound {
		t.Fatalf("stopping a stopped process: %v, want NotFound", err)
	}
	swept, err := f.client.CleanUpOrphanProcesses(owner, &pb.CleanUpOrphanProcessesRequest{})
	if err != nil || swept.Stopped != 0 || swept.Problem != "" || swept.Inventory == nil {
		t.Fatalf("sweep with no orphans: %+v %v", swept, err)
	}
}
