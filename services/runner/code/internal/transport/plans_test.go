package transport

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/artifacts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// A grant that requires a plan starts nothing until the person approved
// one; the orchestrator proposes, never decides; approval creates the tasks
// with their order and makes the cap the tree's; a spawn whose estimate
// would cross the cap is refused, recorded once and shown, until the person
// raises it; what each task cost is on the board.
func TestOrchestratorPlanApprovalAndBudget(t *testing.T) {
	o := newOrchestration(t)
	ctx := timeout(t)
	orch, owner := bearer(ctx, o.orchKey), bearer(ctx, ownerToken)
	grant := o.grant(t, &pb.OrchestratorCeiling{MaxChildren: 3, PlanRequired: true})
	if !grant.Ceiling.PlanRequired {
		t.Fatal("plan requirement lost")
	}
	_, err := o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, Title: "ad hoc"})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "approve a plan before any agent starts") {
		t.Fatalf("spawn before a plan: %v", err)
	}
	// Only the orchestrator proposes; only the person decides.
	_, err = o.client.ProposePlan(bearer(ctx, o.strangerK), &pb.ProposePlanRequest{SessionId: o.stranger.ID, Tasks: []*pb.PlanTask{{Key: "x", Title: "x"}}})
	denied(t, "a stranger proposing", err)
	plan, err := o.client.ProposePlan(orch, &pb.ProposePlanRequest{SessionId: o.orch.ID, OperationId: "plan-1", Summary: "index, then rank; docs alongside", Tasks: []*pb.PlanTask{
		{Key: "index", Title: "Index the corpus", Paths: []string{"search/"}, EstimateUsd: 4, Role: "worker"},
		{Key: "rank", Title: "Rank results", Paths: []string{"search/rank.go"}, EstimateUsd: 3, After: []string{"index"}, Permission: "read-only"},
		{Key: "docs", Title: "Document search", Paths: []string{"docs/search.md"}, EstimateUsd: 3},
	}})
	if err != nil {
		t.Fatal(err)
	}
	if plan.State != "proposed" || plan.TotalUsd != 10 || len(plan.Overlaps) != 1 || plan.Overlaps[0].Path != "search/rank.go" {
		t.Fatalf("plan: %+v", plan)
	}
	if !journalHas(t, o.bench, o.orch, "plan_proposed", "Proposed a plan: 3 agents · est. $10.00") {
		t.Fatal("the orchestrator's transcript does not show its plan")
	}
	_, err = o.client.UpdateMission(orch, &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_ApprovePlan{ApprovePlan: &pb.PlanDecision{PlanId: plan.Id, CapUsd: 1000}}})
	denied(t, "the orchestrator approving its own plan", err)
	m, err := o.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: o.mission, OperationId: "approve-1", Change: &pb.UpdateMissionRequest_ApprovePlan{ApprovePlan: &pb.PlanDecision{PlanId: plan.Id, CapUsd: 6, MaxChildren: 2, Note: "go"}}})
	if err != nil {
		t.Fatal(err)
	}
	ids := map[string]string{}
	for _, p := range m.Plans {
		if p.Id == plan.Id {
			if p.State != "approved" || p.CapUsd != 6 {
				t.Fatalf("approved plan: %+v", p)
			}
			for _, task := range p.Tasks {
				ids[task.Key] = task.TaskId
			}
		}
	}
	if len(m.Tasks) != 3 || ids["index"] == "" || m.Grants[0].Ceiling.BudgetUsd != 6 || m.Grants[0].Ceiling.MaxChildren != 2 {
		t.Fatalf("after approval: tasks %d ids %v grant %+v", len(m.Tasks), ids, m.Grants[0].Ceiling)
	}
	// The orchestrator is told, as an event from Lazybox.
	if !journalHas(t, o.bench, o.orch, "operation_accepted", `"kind":"plan_decided"`) {
		t.Fatal("the orchestrator was not told its plan was approved")
	}
	// Spawns: only the plan's tasks, in order, with the plan's choices.
	_, err = o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, Title: "not in the plan"})
	if status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("an unplanned spawn: %v", err)
	}
	_, err = o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, TaskId: ids["rank"]})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "starts after “Index the corpus”") {
		t.Fatalf("out of order: %v", err)
	}
	index, err := o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, TaskId: ids["index"]})
	if err != nil {
		t.Fatal(err)
	}
	if index.Spawned.EstimateUsd != 4 || index.Spawned.Role != "worker" || index.Grant.ReservedUsd != 4 {
		t.Fatalf("index: %+v grant %+v", index.Spawned, index.Grant)
	}
	// It spends $2 of its $4: $2 stays reserved.
	if err = o.bench.Telemetry.Put(ctx, telemetry.Record{ID: "r1", Scope: telemetry.Scope{Conversation: index.Spawned.ConversationId, Execution: index.Spawned.SessionId, Profile: "codex-default", Provider: "codex"}, Model: "gpt-5.4", Source: "gateway", Status: "complete", Nanos: nanos(2_000_000_000), ObservedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	time.Sleep(5100 * time.Millisecond) // the cost cache
	// docs: $2 spent + $2 reserved + $3 > $6: refused, recorded, shown.
	_, err = o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, TaskId: ids["docs"]})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "the tree's budget is $6.00, and $2.00 is spent, $2.00 reserved") {
		t.Fatalf("over the budget: %v", err)
	}
	m, _ = o.client.GetMission(owner, &pb.GetMissionRequest{MissionId: o.mission})
	if m.Grants[0].BudgetState != "exhausted" || m.Grants[0].SpentUsd != 2 || m.Grants[0].ReservedUsd != 2 {
		t.Fatalf("grant budget: %+v", m.Grants[0])
	}
	for _, task := range m.Tasks {
		if task.Id == ids["index"] && (task.CostUsd != 2 || task.EstimateUsd != 4 || task.PlanId != plan.Id) {
			t.Fatalf("task cost: %+v", task)
		}
	}
	if !journalHas(t, o.bench, o.orch, "budget_exhausted", "New agents paused") {
		t.Fatal("the orchestrator's transcript does not say new agents are paused")
	}
	// Only the person raises the cap; then it starts.
	_, err = o.client.UpdateMission(orch, &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_RaiseBudget{RaiseBudget: &pb.BudgetRaise{GrantId: grant.Id, CapUsd: 100}}})
	denied(t, "the orchestrator raising its budget", err)
	if _, err = o.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_RaiseBudget{RaiseBudget: &pb.BudgetRaise{GrantId: grant.Id, CapUsd: 10}}}); err != nil {
		t.Fatal(err)
	}
	if _, err = o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, TaskId: ids["docs"]}); err != nil {
		t.Fatal("after the raise", err)
	}
	// Rejecting a later plan creates nothing.
	next, err := o.client.ProposePlan(orch, &pb.ProposePlanRequest{SessionId: o.orch.ID, Tasks: []*pb.PlanTask{{Key: "more", Title: "More"}}})
	if err != nil {
		t.Fatal(err)
	}
	m, err = o.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_RejectPlan{RejectPlan: &pb.PlanDecision{PlanId: next.Id, Note: "enough"}}})
	if err != nil || len(m.Tasks) != 3 || m.Plans[0].State != "rejected" {
		t.Fatalf("reject: %v %d", err, len(m.Tasks))
	}
}

// Work is sized to review: while as many tasks wait for the person as the
// grant allows, new agents are held, said once, until the person decides.
func TestOrchestratorHoldsNewAgentsWhileReviewIsFull(t *testing.T) {
	o := newOrchestration(t)
	ctx := timeout(t)
	orch, owner := bearer(ctx, o.orchKey), bearer(ctx, ownerToken)
	o.grant(t, &pb.OrchestratorCeiling{MaxChildren: 3, ReviewLimit: 1})
	first := o.spawn(t, ctx, "Index the corpus")
	firstExec, _ := o.bench.Get(first.SessionId)
	if _, err := o.client.ReportAgentTask(bearer(ctx, agentToken(t, firstExec.Env)), &pb.ReportAgentTaskRequest{SessionId: first.SessionId, Summary: "indexed"}); err != nil {
		t.Fatal(err)
	}
	_, err := o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, Title: "Rank results"})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "1 task waits for the person's review") {
		t.Fatalf("spawn while review is full: %v", err)
	}
	m, _ := o.client.GetMission(owner, &pb.GetMissionRequest{MissionId: o.mission})
	if m.AwaitingReview != 1 {
		t.Fatalf("awaiting review: %d", m.AwaitingReview)
	}
	if !journalHas(t, o.bench, o.orch, "wip_held", "New agents held: 1 task waits for your review") {
		t.Fatal("the orchestrator's transcript does not say new agents are held")
	}
	if _, err = o.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_Accept{Accept: &pb.GateDecision{TaskId: first.TaskId}}}); err != nil {
		t.Fatal(err)
	}
	o.spawn(t, ctx, "Rank results")
}

func nanos(v int64) *int64 { return &v }

func TestPinnedPlanSpawnsFromReviewedBaseline(t *testing.T) {
	o := newOrchestration(t)
	ctx := timeout(t)
	source := history(t, "base")
	manager, err := collections.New(t.TempDir(), []collections.Repository{{ID: "api", Name: "api", Path: source}})
	if err != nil {
		t.Fatal(err)
	}
	o.bench.Collections = manager
	parent, err := manager.Create(ctx, "base", collections.Spec{Name: "Base", Members: []collections.MemberSpec{{RepositoryID: "api"}}})
	if err != nil {
		t.Fatal(err)
	}
	o.orch, err = o.bench.OpenCollection("codex-default", parent.ID, "", accounts.Options{Permission: "ask", Workflow: "plan"}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	o.orchKey = agentToken(t, o.orch.Env)
	o.grant(t, &pb.OrchestratorCeiling{MaxChildren: 2, PlanRequired: true})
	orch, owner := bearer(ctx, o.orchKey), bearer(ctx, ownerToken)
	req := &pb.ProposePlanRequest{SessionId: o.orch.ID, OperationId: "snapshot", Tasks: []*pb.PlanTask{{Key: "a", Title: "Worker", Workspace: "pinned"}}}
	spec, err := o.bench.Artifacts.Put(parent.ID, artifacts.Put{Title: "Acceptance", Kind: "spec", Format: "json", Content: `{"schema":"spec.v1","problem":"Reconnect","requirements":["Restore history"],"acceptanceCriteria":["One row after reconnect"]}`})
	if err != nil {
		t.Fatal(err)
	}
	req.Contract = &pb.PlanContract{ArtifactId: spec.ID, Version: 1}
	member := parent.Members[0]
	dirty := filepath.Join(member.Path, "uncommitted")
	if err = os.WriteFile(dirty, []byte("pending"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err = o.client.ProposePlan(orch, req); err == nil {
		t.Fatal("dirty baseline accepted")
	}
	if err = os.Remove(dirty); err != nil {
		t.Fatal(err)
	}
	plan, err := o.client.ProposePlan(orch, req)
	if err != nil {
		t.Fatal(err)
	}
	if plan.Contract.GetVersion() != 1 || len(plan.Contract.GetAcceptanceCriteria()) != 1 || plan.Contract.Digest != artifacts.Digest(spec.Content) || len(plan.Baseline) != 1 || plan.Baseline[0].Commit != member.BaseCommit {
		t.Fatalf("proposal missing review inputs: %v", plan)
	}
	if _, err = o.bench.Artifacts.Put(parent.ID, artifacts.Put{ID: spec.ID, Content: strings.ReplaceAll(spec.Content, "One row", "Two rows")}); err != nil {
		t.Fatal(err)
	}
	git(t, member.Path, "commit", "--allow-empty", "-m", "after proposal")
	m, err := o.client.UpdateMission(owner, &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_ApprovePlan{ApprovePlan: &pb.PlanDecision{PlanId: plan.Id, MaxChildren: 2}}})
	if err != nil {
		t.Fatal(err)
	}
	task := m.Tasks[0].Id
	spawn := &pb.SpawnAgentRequest{SessionId: o.orch.ID, TaskId: task, Workspace: "same"}
	if _, err = o.client.SpawnAgent(orch, spawn); err == nil {
		t.Fatal("overrode pinned task")
	}
	spawn.Workspace = ""
	view, err := o.client.SpawnAgent(orch, spawn)
	if err != nil {
		t.Fatal(err)
	}
	execution, err := o.bench.Get(view.Spawned.SessionId)
	if err != nil || execution.Options.Workflow == "plan" || execution.Options.Permission != "ask" {
		t.Fatalf("worker options: %v, %v", execution, err)
	}
	brief := <-o.fake.session(view.Spawned.SessionId).Applied
	if !strings.Contains(brief.Text, "One row after reconnect") || strings.Contains(brief.Text, "Two rows after reconnect") {
		t.Fatalf("wrong contract revision in child brief: %s", brief.Text)
	}
	child, err := manager.Get(view.Spawned.CollectionId)
	if err != nil {
		t.Fatal(err)
	}
	if child.ID == parent.ID || git(t, child.Members[0].Path, "rev-parse", "HEAD") != member.BaseCommit {
		t.Fatal("child lost approved baseline")
	}
}
