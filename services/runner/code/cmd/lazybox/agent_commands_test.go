package main

import (
	"context"
	"encoding/json"
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Inside an agent, `lazybox ls` lists the agent's own commands (a server in
// the background first, with its URL) and `lazybox stop ID` stops one of
// them, never another conversation's.
func TestAgentListsAndStopsItsOwnCommands(t *testing.T) {
	r := newTestRunner(t)
	mine, native := r.open("agent")
	other, _ := r.open("other")
	endpoint := ""
	for _, entry := range native.Open.Env {
		if v, ok := strings.CutPrefix(entry, "LAZYBOX_V2_ENDPOINT="); ok {
			endpoint = v
		}
	}
	env := map[string]string{"LAZYBOX_V2_ENDPOINT": endpoint, "LAZYBOX_V2_SESSION_ID": mine.Id}
	ctx := context.Background()

	if out := r.cli(env, "", "ls"); out.err != nil || !strings.Contains(out.stdout, "No commands in this conversation yet") {
		t.Fatalf("%+v", out)
	}
	server, err := r.service.RegisterPipe(ctx, &pb.RegisterPipeRequest{SessionId: mine.Id, Title: "npm run dev"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = r.service.FollowPipe(ctx, &pb.FollowPipeRequest{CommandId: server.Id, Background: true, Pid: 99, Ports: []uint32{5173}}); err != nil {
		t.Fatal(err)
	}
	done, err := r.service.RegisterPipe(ctx, &pb.RegisterPipeRequest{SessionId: mine.Id, Title: "go test ./..."})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = r.service.FinishPipe(ctx, &pb.FinishPipeRequest{CommandId: done.Id, Report: &pb.CommandReport{ExitCode: 1, Filter: "go-test"}}); err != nil {
		t.Fatal(err)
	}
	theirs, err := r.service.RegisterPipe(ctx, &pb.RegisterPipeRequest{SessionId: other.Id, Title: "theirs"})
	if err != nil {
		t.Fatal(err)
	}

	out := r.cli(env, "", "ls")
	if out.err != nil {
		t.Fatal(out.err)
	}
	lines := strings.Split(strings.TrimSpace(out.stdout), "\n")
	if len(lines) != 4 || !strings.Contains(lines[1], server.Id) || !strings.Contains(lines[1], "in the background") || !strings.Contains(lines[1], "http://localhost:5173") ||
		!strings.Contains(lines[2], done.Id) || !strings.Contains(lines[2], "exit 1") || !strings.Contains(lines[3], "lazybox stop ID") || strings.Contains(out.stdout, theirs.Id) {
		t.Fatalf("ls:\n%s", out.stdout)
	}
	out = r.cli(env, "", "ls", "--json")
	var listed struct {
		Commands []jsonCommand `json:"commands"`
	}
	if err = json.Unmarshal([]byte(out.stdout), &listed); err != nil || len(listed.Commands) != 2 || listed.Commands[0].State != "background" || listed.Commands[0].URLs[0] != "http://localhost:5173" || listed.Commands[1].State != "failed" {
		t.Fatalf("%v %s", err, out.stdout)
	}

	if out = r.cli(env, "", "stop", theirs.Id); out.code != exitNotFound {
		t.Fatalf("an agent stopped another conversation's command: %+v", out)
	}
	if out = r.cli(env, "", "stop", server.Id); out.err != nil || !strings.Contains(out.stdout, "Stopping command "+server.Id+" (npm run dev)") {
		t.Fatalf("%+v", out)
	}
	reply, err := r.service.FollowPipe(ctx, &pb.FollowPipeRequest{CommandId: server.Id, Background: true})
	if err != nil || !reply.Stop {
		t.Fatal("the stop did not reach the run", reply, err)
	}
}
