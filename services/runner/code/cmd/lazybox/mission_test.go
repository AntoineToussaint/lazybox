package main

import (
	"context"
	"strings"
	"testing"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// The plan loop from the CLI: the orchestrator proposes in YAML, the person
// approves from their terminal with a cap, the orchestrator lists the tasks
// and starts the first; the person's verbs are refused to the agent.
func TestMissionPlanApproveAndSpawnByTask(t *testing.T) {
	r := newTestRunner(t)
	ctx := context.Background()
	orch, orchNative := r.open("Orchestrator")
	env := agentEnv(t, orchNative, orch.Id)
	mission, err := r.service.CreateMission(ctx, &pb.CreateMissionRequest{Title: "Ship search"})
	if err != nil {
		t.Fatal(err)
	}
	if _, err = r.service.UpdateMission(ctx, &pb.UpdateMissionRequest{MissionId: mission.Id, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: orch.Id, Ceiling: &pb.OrchestratorCeiling{PlanRequired: true}}}}); err != nil {
		t.Fatal(err)
	}
	if out := r.cli(env, "", "agents", "spawn", "--title", "ad hoc"); out.code == 0 || !strings.Contains(out.err.Error(), "lazybox mission plan") {
		t.Fatalf("spawn before a plan: %+v", out)
	}
	plan := "summary: index first, docs alongside\ntasks:\n  - key: index\n    title: Index the corpus\n    paths: [search/]\n    estimate_usd: 4\n  - key: docs\n    title: Document search\n    paths: [search/README.md]\n    estimate_usd: 1\n"
	out := r.cli(env, plan, "mission", "plan", "-")
	if out.code != 0 || !strings.Contains(out.stdout, "2 tasks, est. $5.00") || !strings.Contains(out.stdout, "search/README.md is in index and docs") {
		t.Fatalf("plan: %+v", out)
	}
	if out = r.cli(env, "", "mission", "approve", mission.Id, "--cap", "100"); out.code != exitDenied {
		t.Fatalf("an agent approving: %+v", out)
	}
	if out = r.cli(r.owner(), "", "mission", "approve", mission.Id, "--cap", "8", "--wip", "2"); out.code != 0 || !strings.Contains(out.stdout, "Approved: 2 tasks on “Ship search”, $8.00 cap") {
		t.Fatalf("approve: %+v", out)
	}
	out = r.cli(env, "", "agents", "ls")
	if out.code != 0 || !strings.Contains(out.stdout, "Budget: $0.00 spent, $0.00 reserved of $8.00") || !strings.Contains(out.stdout, "ready to start: lazybox agents spawn --task") {
		t.Fatalf("ls after approval: %+v", out)
	}
	m, _ := r.service.GetMission(ctx, &pb.GetMissionRequest{MissionId: mission.Id})
	index := m.Plans[0].Tasks[0].TaskId
	if out = r.cli(env, "", "agents", "spawn", "--task", index); out.code != 0 || !strings.Contains(out.stdout, "Started Index the corpus") {
		t.Fatalf("spawn by task: %+v", out)
	}
	if out = r.cli(r.owner(), "", "mission", "budget", mission.Id, "--cap", "20"); out.code != 0 || !strings.Contains(out.stdout, "may spend $20.00") {
		t.Fatalf("budget: %+v", out)
	}
}
