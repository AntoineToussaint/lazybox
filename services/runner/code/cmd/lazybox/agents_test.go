package main

import (
	"context"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// agentEnv is the environment an agent's commands run with: its own scoped
// endpoint and its session.
func agentEnv(t *testing.T, native *agenttest.Session, session string) map[string]string {
	t.Helper()
	for _, entry := range native.Open.Env {
		if v, ok := strings.CutPrefix(entry, "LAZYBOX_V2_ENDPOINT="); ok {
			return map[string]string{"LAZYBOX_V2_ENDPOINT": v, "LAZYBOX_V2_SESSION_ID": session}
		}
	}
	t.Fatal("agent has no endpoint")
	return nil
}

// An orchestrator the person granted starts an agent in a workspace of its
// own, lists, messages and waits on it; the agent talks back and reports;
// neither reaches past its lineage, and nothing works without the grant.
func TestAgentsVerbRunsALineage(t *testing.T) {
	r := newTestRunner(t)
	ctx := context.Background()
	orch, orchNative := r.open("Orchestrator")
	env := agentEnv(t, orchNative, orch.Id)

	// No grant: spawn asks the person to start it instead (conversation_test.go), ls explains.
	if out := r.cli(env, "", "agents", "spawn", "--title", "Index", "--brief", "An index.", "--no-wait"); out.code != 0 || !strings.Contains(out.stdout, "Asked the person to start “Index”") {
		t.Fatalf("spawn without a grant: %+v", out)
	}
	if out := r.cli(env, "", "agents", "ls"); out.code != 0 || !strings.Contains(out.stdout, "You run no agents") {
		t.Fatalf("ls without a grant: %+v", out)
	}
	if out := r.cli(map[string]string{"LAZYBOX_V2_ENDPOINT": env["LAZYBOX_V2_ENDPOINT"]}, "", "agents", "ls"); out.code != exitUsage {
		t.Fatalf("outside an agent: %+v", out)
	}

	mission, err := r.service.CreateMission(ctx, &pb.CreateMissionRequest{Title: "Ship search"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = r.service.UpdateMission(ctx, &pb.UpdateMissionRequest{MissionId: mission.Id, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: orch.Id, Ceiling: &pb.OrchestratorCeiling{MaxChildren: 2}}}}); err != nil {
		t.Fatal(err)
	}
	out := r.cli(env, "", "agents", "spawn", "--title", "Index the corpus", "--brief", "Objective: an index\nDone when: go test passes", "--workspace", "new", "--role", "researcher")
	if out.code != 0 || !strings.Contains(out.stdout, "Started Index the corpus (researcher · claude-default") || !strings.Contains(out.stdout, "1 of 2 agents running") {
		t.Fatalf("spawn: %+v", out)
	}
	childNative := <-r.provider.opened
	brief := <-childNative.Applied
	if brief.Origin == nil || brief.Origin.From != "Orchestrator" || brief.Origin.Detail != "researcher" || !strings.Contains(brief.Text, "Done when: go test passes") {
		t.Fatalf("brief: %+v", brief)
	}
	if childNative.Open.Cwd == orchNative.Open.Cwd {
		t.Fatal("--workspace new shares the orchestrator's directory")
	}
	childSession := ""
	until(t, "child listed", func() bool {
		lineage, err := r.service.ListAgentLineage(ctx, &pb.AgentLineageRequest{SessionId: orch.Id})
		if err == nil && len(lineage.Children) == 1 {
			childSession = lineage.Children[0].SessionId
		}
		return childSession != ""
	})
	kid := agentEnv(t, childNative, childSession)
	if out = r.cli(env, "", "agents", "ls"); out.code != 0 || !strings.Contains(out.stdout, "Orchestrating “Ship search”") || !strings.Contains(out.stdout, "Index the corpus") {
		t.Fatalf("ls: %+v", out)
	}
	if out = r.cli(env, "", "agents", "send", "Index the corpus", "skip vendor/"); out.code != 0 || !strings.Contains(out.stdout, "Sent to “Index the corpus”") {
		t.Fatalf("send: %+v", out)
	}
	if out = r.cli(kid, "", "agents", "send", "parent", "found the tokenizer"); out.code != 0 {
		t.Fatalf("child to parent: %+v", out)
	}
	if out = r.cli(kid, "", "agents", "ls"); !strings.Contains(out.stdout, "Started by “Orchestrator”") {
		t.Fatalf("child's ls: %+v", out)
	}
	// A child has no grant: spawning asks the person, never its orchestrator.
	if out = r.cli(kid, "", "agents", "spawn", "--title", "grandchild", "--brief", "b", "--no-wait"); out.code != 0 || !strings.Contains(out.stdout, "Asked the person") {
		t.Fatalf("child spawning: %+v", out)
	}
	// Past the lineage: refused.
	if out = r.cli(kid, "", "agents", "stop", "parent", "--end"); out.code != exitDenied {
		t.Fatalf("child stopping its parent: %+v", out)
	}
	stranger, _ := r.open("Stranger")
	if out = r.cli(env, "", "agents", "send", stranger.Id, "hi"); out.code != exitDenied {
		t.Fatalf("message a stranger: %+v", out)
	}
	// The child ends its turn and reports; the orchestrator's wait returns.
	childNative.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	if out = r.cli(kid, "", "agents", "report", "indexed 1,204 files; go test passed"); out.code != 0 || !strings.Contains(out.stdout, "Reported") {
		t.Fatalf("report: %+v", out)
	}
	if out = r.cli(env, "", "agents", "wait", "Index the corpus", "--until", "reported", "--timeout", "5s"); out.code != 0 || !strings.Contains(out.stdout, "reported: indexed 1,204 files") {
		t.Fatalf("wait: %+v", out)
	}
	if out = r.cli(env, "", "agents", "wait", "--until", "needs-input", "--timeout", "1s"); out.code != exitTimeout {
		t.Fatalf("a wait that cannot hold times out: %+v", out)
	}
	if out = r.cli(env, "", "agents", "stop", "Index the corpus", "--end", "--reason", "done"); out.code != 0 || !strings.Contains(out.stdout, "Ended") {
		t.Fatalf("stop: %+v", out)
	}
	// The person's view: the task waits for acceptance, attributed to the agent tree.
	m, err := r.service.GetMission(ctx, &pb.GetMissionRequest{MissionId: mission.Id})
	if err != nil {
		t.Fatal(err)
	}
	if len(m.Tasks) != 1 || m.Tasks[0].Gate == "" || m.Tasks[0].Attempt.Role != "researcher" || len(m.Messages) < 3 {
		t.Fatalf("mission: %+v", m)
	}
}
