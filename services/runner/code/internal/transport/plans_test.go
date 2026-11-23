package transport

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
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
	holder := o.orch.Saved.ConversationID
	if holder == "" {
		holder = o.orch.ID
	}
	if err := o.bench.Telemetry.Put(ctx, telemetry.Record{ID: "before-grant", Scope: telemetry.Scope{Conversation: holder, Execution: o.orch.ID, Profile: "codex-default", Provider: "codex"}, Model: "gpt-5.4", Source: "gateway", Status: "complete", Nanos: nanos(12_000_000_000), ObservedAt: time.Now().Add(-time.Hour)}); err != nil {
		t.Fatal(err)
	}
	grant := o.grant(t, &pb.OrchestratorCeiling{MaxChildren: 3, PlanRequired: true})
	if grant.SpentUsd != 0 {
		t.Fatalf("new grant inherited pre-grant spend: %v", grant.SpentUsd)
	}
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
	for _, tc := range []struct{ name, content, want string }{
		{"text", `{"schema":"spec.v1","problem":"Reconnect","requirements":["Restore history"],"acceptanceCriteria":["One row after reconnect"]}`, "One row after reconnect"},
		{"named", `{"schema":"spec.v2","problem":"Reconnect","requirements":["Restore history"],"acceptanceCriteria":[{"id":"reconnect","expected":"One row after reconnect","required":true}]}`, "[reconnect, required] One row after reconnect"},
	} {
		t.Run(tc.name, func(t *testing.T) { testPinnedPlanSpawnsFromReviewedBaseline(t, tc.content, tc.want) })
	}
}

func testPinnedPlanSpawnsFromReviewedBaseline(t *testing.T, content, want string) {
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
	req := &pb.ProposePlanRequest{SessionId: o.orch.ID, OperationId: "snapshot", Tasks: []*pb.PlanTask{{Key: "a", Title: "Worker", Workspace: "pinned"}, {Key: "b", Title: "Worker", Workspace: "pinned"}}}
	spec, err := o.bench.Artifacts.Put(parent.ID, artifacts.Put{Title: "Acceptance", Kind: "spec", Format: "json", Content: content})
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
	if plan.Contract.GetVersion() != 1 || len(plan.Contract.GetAcceptanceCriteria()) != 1 || plan.Contract.GetAcceptanceCriteria()[0] != want || plan.Contract.Digest != artifacts.Digest(spec.Content) || len(plan.Baseline) != 1 || plan.Baseline[0].Commit != member.BaseCommit {
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
	task := m.Plans[0].Tasks[0].TaskId
	spawn := &pb.SpawnAgentRequest{SessionId: o.orch.ID, TaskId: task, Workspace: "same"}
	if _, err = o.client.SpawnAgent(orch, spawn); err == nil {
		t.Fatal("overrode pinned task")
	}
	spawn.Workspace = ""
	spawn.OperationId = strings.Repeat("k", 125) + "a"
	view, err := o.client.SpawnAgent(orch, spawn)
	if err != nil {
		t.Fatal(err)
	}
	execution, err := o.bench.Get(view.Spawned.SessionId)
	if err != nil || execution.Options.Workflow == "plan" || execution.Options.Permission != "ask" {
		t.Fatalf("worker options: %v, %v", execution, err)
	}
	brief := <-o.fake.session(view.Spawned.SessionId).Applied
	if !strings.Contains(brief.Text, "\n- "+want) || strings.Contains(brief.Text, "Two rows after reconnect") {
		t.Fatalf("wrong contract revision in child brief: %s", brief.Text)
	}
	// The worker's checks count against the approved contract, never a later revision.
	pipe, err := o.client.RegisterPipe(owner, &pb.RegisterPipeRequest{SessionId: view.Spawned.SessionId, Title: "go test ./reconnect", Check: "reconnect"})
	if !strings.HasPrefix(want, "[") {
		if err == nil || !strings.Contains(err.Error(), "approved plan") {
			t.Fatalf("checked a text-only contract: %v", err)
		}
	} else {
		if err != nil {
			t.Fatal(err)
		}
		done, err := o.client.FinishPipe(owner, &pb.FinishPipeRequest{CommandId: pipe.Id, Report: &pb.CommandReport{ExitCode: 0}})
		if err != nil || !strings.HasPrefix(done.Delivery, "check reconnect: passed on candidate:") || !strings.Contains(done.Delivery, "v1)") {
			t.Fatalf("finish: %v %v", done, err)
		}
		recorded := o.bench.Delivery.Observations()
		last := recorded[len(recorded)-1]
		if last.Subject.Mission != o.mission || last.Subject.Task != task || last.ContractDigest != plan.Contract.Digest || last.Attempt == "" || strings.HasPrefix(last.Attempt, "conversation:") {
			t.Fatalf("observation outside the Mission attempt: %+v", last)
		}
		if _, err := o.client.BindDelivery(owner, &pb.BindDeliveryRequest{SessionId: view.Spawned.SessionId, ArtifactId: spec.ID, Version: 2}); err == nil {
			t.Fatal("a Mission worker bound its own spec")
		}
	}
	child, err := manager.Get(view.Spawned.CollectionId)
	if err != nil {
		t.Fatal(err)
	}
	if child.ID == parent.ID || git(t, child.Members[0].Path, "rev-parse", "HEAD") != member.BaseCommit {
		t.Fatal("child lost approved baseline")
	}
	_, err = o.bench.Artifacts.Put(child.ID, artifacts.Put{Title: "Scratch plan", Kind: "plan", Content: "Unapproved local criteria"})
	if err != nil {
		t.Fatal(err)
	}
	review, err := o.client.StartReview(owner, &pb.StartReviewRequest{CollectionId: child.ID, SourceSessionId: view.Spawned.SessionId, ProfileId: "codex-default"})
	if err != nil {
		t.Fatal(err)
	}
	reviewerBrief := briefOf(t, o.fake.session(review.Session.Id).Applied)
	if !strings.Contains(reviewerBrief.Text, "Approved Mission contract") || !strings.Contains(reviewerBrief.Text, plan.Contract.Digest) || !strings.Contains(reviewerBrief.Text, want) || strings.Contains(reviewerBrief.Text, "Unapproved local criteria") {
		t.Fatalf("wrong reviewer contract: %s", reviewerBrief.Text)
	}
	git(t, child.Members[0].Path, "commit", "--allow-empty", "-m", "new candidate head")
	reviewer := o.fake.session(review.Session.Id)
	reviewer.Events <- agent.Event{Type: "turn_started"}
	reviewer.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	until(t, "stale revision-bound verdict", func() bool {
		board, err := o.client.GetMission(owner, &pb.GetMissionRequest{MissionId: o.mission})
		if err != nil {
			return false
		}
		for _, row := range board.Tasks {
			for _, ev := range row.Evidence {
				if ev.Kind == "review" && ev.State == "stale" && ev.Outcome == "unknown" && ev.ContractDigest == plan.Contract.Digest && ev.Revisions["api"] == member.BaseCommit {
					return true
				}
			}
		}
		return false
	})
	spawn.TaskId = m.Plans[0].Tasks[1].TaskId
	spawn.OperationId = strings.Repeat("k", 125) + "b"
	// Remove the fixture's source temporarily so allocation fails after reservation.
	if err = os.Rename(source, source+"-offline"); err != nil {
		t.Fatal(err)
	}
	_, err = o.client.SpawnAgent(orch, spawn)
	if restore := os.Rename(source+"-offline", source); restore != nil {
		t.Fatal(restore)
	}
	if err == nil {
		t.Fatal("allocation should fail with source offline")
	}
	reserved, err := o.bench.Work.Mission(o.mission)
	if err != nil {
		t.Fatal(err)
	}
	a := reserved.Task(spawn.TaskId).Current()
	if a == nil || a.Dispatch == nil || !a.Started.IsZero() || reserved.LiveChildren(a.Grant) != 2 {
		t.Fatalf("lost pending reservation: %+v", a)
	}
	other, err := o.client.SpawnAgent(orch, spawn)
	if err != nil {
		t.Fatal(err)
	}
	if other.Spawned.SessionId != a.Execution || other.Spawned.CollectionId != a.Workspace {
		t.Fatal("retry replaced reserved identities")
	}
	again, err := o.client.SpawnAgent(orch, spawn)
	if err != nil || again.Spawned.SessionId != a.Execution {
		t.Fatalf("replayed assignment: %v %v", again, err)
	}
	applied := o.fake.session(a.Execution).Applied
	_ = briefOf(t, applied)
	select {
	case <-applied:
		t.Fatal("duplicate worker brief")
	default:
	}
	if other.Spawned.CollectionId == child.ID {
		t.Fatal("distinct pinned tasks share a writable workspace")
	}

}
