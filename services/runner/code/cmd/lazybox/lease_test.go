package main

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Live overlap from real per-turn checkpoints: two agents of one
// orchestrator, each in its own workspace, change the same file; when the
// second one's turn ends, the overlap is on the mission and the
// orchestrator is told. An exclusive lease refuses another task's claim on
// its paths, and an edit inside it anyway is recorded too. Nothing is blocked.
func TestLeasesAndLiveOverlapFromCheckpoints(t *testing.T) {
	r := newTestRunner(t)
	ctx := context.Background()
	orch, orchNative := r.open("Orchestrator")
	env := agentEnv(t, orchNative, orch.Id)
	mission, err := r.service.CreateMission(ctx, &pb.CreateMissionRequest{Title: "Ship search"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = r.service.UpdateMission(ctx, &pb.UpdateMissionRequest{MissionId: mission.Id, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: orch.Id}}}); err != nil {
		t.Fatal(err)
	}
	type kid struct {
		native  *agenttest.Session
		env     map[string]string
		session string
	}
	spawn := func(title string) kid {
		out := r.cli(env, "", "agents", "spawn", "--title", title, "--workspace", "new", "--json")
		if out.code != 0 {
			t.Fatalf("spawn %s: %+v", title, out)
		}
		native := <-r.provider.opened
		<-native.Applied // the brief: its turn has begun
		var session string
		until(t, "child listed", func() bool {
			lineage, _ := r.service.ListAgentLineage(ctx, &pb.AgentLineageRequest{SessionId: orch.Id})
			for _, c := range lineage.GetChildren() {
				if c.TaskTitle == title {
					session = c.SessionId
				}
			}
			return session != ""
		})
		return kid{native: native, env: agentEnv(t, native, session), session: session}
	}
	a, b := spawn("Index the corpus"), spawn("Rank results")
	if a.native.Open.Cwd == b.native.Open.Cwd {
		t.Fatal("the two agents share a directory")
	}
	// A leases a folder exclusively; B cannot lease inside it.
	if out := r.cli(a.env, "", "lease", "claim", "api/locked/", "--exclusive", "--reason", "rewriting the lexer"); out.code != 0 || !strings.Contains(out.stdout, "Leased api/locked/ (exclusive) for “Index the corpus”") {
		t.Fatalf("claim: %+v", out)
	}
	if out := r.cli(b.env, "", "lease", "claim", "api/locked/y.go"); out.code == 0 || !strings.Contains(out.err.Error(), "api/locked/ is leased exclusive by “Index the corpus”") {
		t.Fatalf("claim inside an exclusive lease: %+v", out)
	}
	if out := r.cli(b.env, "", "lease", "ls"); out.code != 0 || !strings.Contains(out.stdout, "api/locked/") {
		t.Fatalf("ls: %+v", out)
	}
	write := func(k kid, rel string) {
		path := filepath.Join(k.native.Open.Cwd, rel)
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte("package api\n// "+rel+"\n"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	// Both change api/shared.go; B also writes inside A's exclusive lease.
	write(a, "api/shared.go")
	a.native.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	write(b, "api/shared.go")
	write(b, "api/locked/y.go")
	b.native.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	var m *pb.Mission
	until(t, "overlaps recorded", func() bool {
		m, _ = r.service.GetMission(ctx, &pb.GetMissionRequest{MissionId: mission.Id})
		return len(m.GetOverlaps()) >= 2
	})
	kinds := map[string][]string{}
	for _, o := range m.Overlaps {
		kinds[o.Kind] = o.Paths
	}
	if strings.Join(kinds["files"], ",") != "api/shared.go" || strings.Join(kinds["lease"], ",") != "api/locked/y.go" {
		t.Fatalf("overlaps: %+v", m.Overlaps)
	}
	// The orchestrator is told, as an event from Lazybox.
	until(t, "orchestrator told", func() bool {
		select {
		case op := <-orchNative.Applied:
			return op.Origin != nil && op.Origin.Kind == agent.OriginOverlap && strings.Contains(op.Text, "Your agents overlap")
		default:
			return false
		}
	})
	if len(m.Leases) != 1 || m.Leases[0].Mode != "exclusive" {
		t.Fatalf("leases: %+v", m.Leases)
	}
	if out := r.cli(a.env, "", "lease", "release", "api/locked/"); out.code != 0 {
		t.Fatalf("release: %+v", out)
	}
}
