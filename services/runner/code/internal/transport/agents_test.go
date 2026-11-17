package transport

import (
	"context"
	"encoding/json"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/emptypb"
)

// fleet is a fake provider that opens a fresh native session per conversation.
type fleet struct {
	mu       sync.Mutex
	sessions map[string]*agenttest.Session
}

func (f *fleet) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	s, err := (&agenttest.Adapter{}).Open(ctx, o)
	if err != nil {
		return nil, err
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.sessions == nil {
		f.sessions = map[string]*agenttest.Session{}
	}
	f.sessions[o.ID] = s.(*agenttest.Session)
	return s, nil
}

func (f *fleet) session(id string) *agenttest.Session {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.sessions[id]
}

// orchestration is a Runner with an orchestrator conversation, a stranger
// conversation and a mission, behind the production interceptors.
type orchFixture struct {
	*authorityFixture
	fake               *fleet
	orch, stranger     *runner.Execution
	orchKey, strangerK string
	mission            string
}

func newOrchestration(t *testing.T) *orchFixture {
	t.Helper()
	f := newAuthorityFixture(t)
	fake := &fleet{}
	f.bench.Adapters["codex"] = fake
	o := &orchFixture{authorityFixture: f, fake: fake}
	var err error
	if o.orch, err = f.bench.OpenEmptyWorkspace("codex-default", accounts.Options{Permission: "ask"}, "direct"); err != nil {
		t.Fatal(err)
	}
	if o.stranger, err = f.bench.OpenEmptyWorkspace("codex-default", accounts.Options{Permission: "ask"}, "direct"); err != nil {
		t.Fatal(err)
	}
	o.orchKey, o.strangerK = agentToken(t, o.orch.Env), agentToken(t, o.stranger.Env)
	m, err := f.client.CreateMission(bearer(timeout(t), ownerToken), &pb.CreateMissionRequest{Title: "Ship search"})
	if err != nil {
		t.Fatal(err)
	}
	o.mission = m.Id
	return o
}

func (o *orchFixture) grant(t *testing.T, ceiling *pb.OrchestratorCeiling) *pb.MissionGrant {
	t.Helper()
	m, err := o.client.UpdateMission(bearer(timeout(t), ownerToken), &pb.UpdateMissionRequest{MissionId: o.mission, OperationId: "grant-" + o.orch.ID, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: o.orch.ID, Ceiling: ceiling}}})
	if err != nil {
		t.Fatal(err)
	}
	for _, g := range m.Grants {
		if g.Id == m.CreatedId {
			return g
		}
	}
	t.Fatalf("grant %q not on the mission: %+v", m.CreatedId, m.Grants)
	return nil
}

func (o *orchFixture) spawn(t *testing.T, ctx context.Context, title string) *pb.AgentChild {
	t.Helper()
	out, err := o.client.SpawnAgent(bearer(ctx, o.orchKey), &pb.SpawnAgentRequest{SessionId: o.orch.ID, Title: title, Brief: "Objective: " + title})
	if err != nil {
		t.Fatal(err)
	}
	if out.Spawned == nil {
		t.Fatal("no child in the answer")
	}
	return out.Spawned
}

func denied(t *testing.T, what string, err error) {
	t.Helper()
	if status.Code(err) != codes.PermissionDenied {
		t.Fatalf("%s: got %v, want PermissionDenied", what, err)
	}
}

// TestOrchestratorGrantWalksEveryRPC: an agent credential reaches only its
// verbs, only for conversations in its lineage, with or without a grant, and
// never an approval, an answer, a gate, an acceptance or a permission change.
func TestOrchestratorGrantWalksEveryRPC(t *testing.T) {
	o := newOrchestration(t)
	ctx := timeout(t)
	orch := bearer(ctx, o.orchKey)

	// Without a grant: nothing starts and nobody is in its lineage.
	_, err := o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, Title: "x"})
	denied(t, "spawn without a grant", err)
	_, err = o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: o.stranger.ID, Text: "hi"})
	denied(t, "message a stranger without a grant", err)
	_, err = o.client.WaitAgents(orch, &pb.WaitAgentsRequest{SessionId: o.orch.ID, Targets: []string{o.stranger.ID}})
	denied(t, "wait on a stranger", err)
	_, err = o.client.StopChildAgent(orch, &pb.StopChildAgentRequest{SessionId: o.orch.ID, Target: o.stranger.ID, End: true})
	denied(t, "stop a stranger", err)
	_, err = o.client.ReportAgentTask(orch, &pb.ReportAgentTaskRequest{SessionId: o.orch.ID, Summary: "done"})
	denied(t, "report without a parent", err)
	if lineage, err := o.client.ListAgentLineage(orch, &pb.AgentLineageRequest{SessionId: o.orch.ID}); err != nil || lineage.Grant != nil || len(lineage.Children) != 0 {
		t.Fatalf("own lineage without a grant: %+v %v", lineage, err)
	}
	// A grant is the person's: an agent cannot give itself one.
	_, err = o.client.UpdateMission(orch, &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: o.orch.ID}}})
	denied(t, "an agent granting itself", err)

	grant := o.grant(t, &pb.OrchestratorCeiling{MaxChildren: 2})
	if !grant.Live || grant.Ceiling.Permission != "ask" || grant.Ceiling.MaxChildren != 2 || grant.Ceiling.Depth != 1 || len(grant.Ceiling.ProfileIds) != 1 || grant.Ceiling.ProfileIds[0] != "codex-default" || grant.IssuedBy != "human" {
		t.Fatalf("grant defaults to the conversation's own: %+v", grant)
	}
	child := o.spawn(t, ctx, "Index the corpus")
	if child.Permission != "ask" || child.ProfileId != "codex-default" || child.Role != "worker" || child.TaskTitle != "Index the corpus" {
		t.Fatalf("child: %+v", child)
	}
	childExec, err := o.bench.Get(child.SessionId)
	if err != nil {
		t.Fatal(err)
	}
	childKey := agentToken(t, childExec.Env)
	kid := bearer(ctx, childKey)

	// With the grant, every RPC off the agent allowlist is still denied —
	// to the orchestrator and to its child.
	allowed := map[string]bool{}
	for method := range agentRPCs {
		allowed[method] = true
	}
	desc := pb.RunnerService_ServiceDesc
	for _, who := range []struct {
		name string
		ctx  context.Context
	}{{"orchestrator", orch}, {"child", kid}} {
		for _, m := range desc.Methods {
			method := "/" + desc.ServiceName + "/" + m.MethodName
			if allowed[method] {
				continue
			}
			if err := o.conn.Invoke(who.ctx, method, &emptypb.Empty{}, &emptypb.Empty{}); status.Code(err) != codes.PermissionDenied {
				t.Errorf("%s with a grant in its tree: %s got %v", who.name, method, err)
			}
		}
		for _, s := range desc.Streams {
			method := "/" + desc.ServiceName + "/" + s.StreamName
			stream, err := o.conn.NewStream(who.ctx, &grpc.StreamDesc{StreamName: s.StreamName, ServerStreams: s.ServerStreams, ClientStreams: s.ClientStreams}, method)
			if err == nil {
				if err = stream.SendMsg(&emptypb.Empty{}); err == nil {
					_ = stream.CloseSend()
					err = stream.RecvMsg(&emptypb.Empty{})
				}
			}
			if status.Code(err) != codes.PermissionDenied {
				t.Errorf("%s: stream %s got %v", who.name, method, err)
			}
		}
	}

	// The forbidden verbs, named: the child asks for approval; neither the
	// orchestrator nor the child can approve or answer it, pass the gate,
	// accept, change a permission, or revoke the grant.
	native := o.fake.session(child.SessionId)
	native.Events <- agent.Event{Type: "approval", RequestID: "req-1", Text: "Bash rm -rf"}
	if native == nil {
		t.Fatal("no native session for the child")
	}
	request := ""
	until(t, "approval pending", func() bool {
		for id := range childExec.Engine.Snapshot().Pending {
			request = id
		}
		return request != ""
	})
	for _, who := range []context.Context{orch, kid} {
		for name, op := range map[string]*pb.ApplyRequest{
			"approve":    {SessionId: child.SessionId, OperationId: "a", Operation: &pb.ApplyRequest_Approval{Approval: &pb.Approval{RequestId: request, Decision: pb.Decision_DECISION_ACCEPT}}},
			"answer":     {SessionId: child.SessionId, OperationId: "b", Operation: &pb.ApplyRequest_Answer{Answer: &pb.Answer{RequestId: request}}},
			"permission": {SessionId: child.SessionId, OperationId: "c", Operation: &pb.ApplyRequest_Permission{Permission: &pb.Permission{Mode: "yolo"}}},
			"message":    {SessionId: child.SessionId, OperationId: "d", Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "as the person"}}},
		} {
			_, err := o.client.Apply(who, op)
			denied(t, name, err)
		}
		for name, change := range map[string]*pb.UpdateMissionRequest{
			"accept": {MissionId: o.mission, Change: &pb.UpdateMissionRequest_Accept{Accept: &pb.GateDecision{TaskId: child.TaskId}}},
			"reject": {MissionId: o.mission, Change: &pb.UpdateMissionRequest_Reject{Reject: &pb.GateDecision{TaskId: child.TaskId}}},
			"revoke": {MissionId: o.mission, Change: &pb.UpdateMissionRequest_RevokeGrant{RevokeGrant: &pb.GrantRevoke{GrantId: grant.Id}}},
			"widen":  {MissionId: o.mission, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: child.SessionId}}},
		} {
			_, err := o.client.UpdateMission(who, change)
			denied(t, name, err)
		}
	}
	if _, ok := childExec.Engine.Snapshot().Pending[request]; !ok {
		t.Fatal("the child's approval was resolved without the person")
	}

	// Lineage scoping: the orchestrator reaches its child, never the stranger;
	// the child reaches its parent, never the stranger, and never stops it.
	if _, err = o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: child.ConversationId, Text: "use the new tokenizer"}); err != nil {
		t.Fatal("message to its own child", err)
	}
	_, err = o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: o.stranger.ID, Text: "hi"})
	denied(t, "message a stranger with a grant", err)
	_, err = o.client.StopChildAgent(orch, &pb.StopChildAgentRequest{SessionId: o.orch.ID, Target: o.stranger.ID})
	denied(t, "stop a stranger with a grant", err)
	_, err = o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.stranger.ID, Title: "spoof"})
	denied(t, "spawn as another execution", err)
	_, err = o.client.SpawnAgent(kid, &pb.SpawnAgentRequest{SessionId: child.SessionId, Title: "grandchild"})
	denied(t, "a child without a grant spawning", err)
	if _, err = o.client.SendAgentMessage(kid, &pb.SendAgentMessageRequest{SessionId: child.SessionId, To: "parent", Text: "tokenizer found"}); err != nil {
		t.Fatal("child to parent", err)
	}
	_, err = o.client.SendAgentMessage(kid, &pb.SendAgentMessageRequest{SessionId: child.SessionId, To: o.stranger.ID, Text: "psst"})
	denied(t, "child to a stranger", err)
	_, err = o.client.StopChildAgent(kid, &pb.StopChildAgentRequest{SessionId: child.SessionId, Target: "parent", End: true})
	denied(t, "child stopping its parent", err)
	_, err = o.client.SendAgentMessage(bearer(ctx, o.strangerK), &pb.SendAgentMessageRequest{SessionId: o.stranger.ID, To: child.ConversationId, Text: "hijack"})
	denied(t, "stranger to the child", err)

	// Revoked: the orchestrator drives nothing any more; its child still reports.
	if _, err = o.client.UpdateMission(bearer(ctx, ownerToken), &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_RevokeGrant{RevokeGrant: &pb.GrantRevoke{GrantId: grant.Id, Reason: "done for today"}}}); err != nil {
		t.Fatal(err)
	}
	_, err = o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, Title: "after revoke"})
	denied(t, "spawn after revoke", err)
	_, err = o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: child.ConversationId, Text: "still there?"})
	denied(t, "drive after revoke", err)
	if _, err = o.client.ReportAgentTask(kid, &pb.ReportAgentTaskRequest{SessionId: child.SessionId, Summary: "indexed 1,204 files; lazybox run go test passed"}); err != nil {
		t.Fatal("child report after its parent's grant was revoked", err)
	}
	m, err := o.client.GetMission(bearer(ctx, ownerToken), &pb.GetMissionRequest{MissionId: o.mission})
	if err != nil {
		t.Fatal(err)
	}
	var task *pb.MissionTask
	for _, tk := range m.Tasks {
		if tk.Id == child.TaskId {
			task = tk
		}
	}
	if task == nil || task.Status != "needs_you" || task.Gate == "" || task.Attempt.ParentConversationId != o.orch.Saved.ConversationID && task.Attempt.ParentConversationId != o.orch.ID {
		t.Fatalf("the child's report asks the person: %+v", task)
	}
	if len(m.Grants) != 1 || m.Grants[0].Live || m.Grants[0].RevokeReason != "done for today" || len(m.Messages) < 2 {
		t.Fatalf("grant and messages on the mission: %+v %d", m.Grants, len(m.Messages))
	}
}

// A child gets at most the ceiling and its parent's own permission; the
// grant caps how many run at once; asking for more is refused with why.
func TestOrchestratorChildrenOnlyShrink(t *testing.T) {
	o := newOrchestration(t)
	ctx := timeout(t)
	orch := bearer(ctx, o.orchKey)
	// The person cannot give more than the conversation's own permission.
	_, err := o.client.UpdateMission(bearer(ctx, ownerToken), &pb.UpdateMissionRequest{MissionId: o.mission, Change: &pb.UpdateMissionRequest_Grant{Grant: &pb.OrchestratorGrantChange{SessionId: o.orch.ID, Ceiling: &pb.OrchestratorCeiling{Permission: "yolo"}}}})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "while it runs at ask") {
		t.Fatalf("a yolo ceiling over an ask conversation: %v", err)
	}
	o.grant(t, &pb.OrchestratorCeiling{Permission: "read-only", MaxChildren: 1})
	for name, r := range map[string]*pb.SpawnAgentRequest{
		"yolo":         {SessionId: o.orch.ID, Title: "x", Permission: "yolo"},
		"ask":          {SessionId: o.orch.ID, Title: "x", Permission: "ask"},
		"inherit":      {SessionId: o.orch.ID, Title: "x", Permission: "inherit"},
		"subscription": {SessionId: o.orch.ID, Title: "x", ProfileId: "claude-default"},
		"workspace":    {SessionId: o.orch.ID, Title: "x", Workspace: "new"},
	} {
		if _, err := o.client.SpawnAgent(orch, r); status.Code(err) != codes.FailedPrecondition {
			t.Errorf("%s: got %v, want a refusal", name, err)
		}
	}
	child := o.spawn(t, ctx, "Read the logs")
	if child.Permission != "read-only" {
		t.Fatalf("default permission is the narrower of the ceiling and the parent's: %q", child.Permission)
	}
	e, _ := o.bench.Get(child.SessionId)
	if e.Options.Permission != "read-only" || !e.Options.NoRotation || !strings.Contains(e.Options.Instructions, "You were started by another agent") {
		t.Fatalf("the child's own options: %+v", e.Options)
	}
	if _, err := o.client.SpawnAgent(orch, &pb.SpawnAgentRequest{SessionId: o.orch.ID, Title: "second"}); status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "1 agents at once") {
		t.Fatalf("over the fan-out: %v", err)
	}
	// Ending the first frees its slot.
	if _, err := o.client.StopChildAgent(orch, &pb.StopChildAgentRequest{SessionId: o.orch.ID, Target: "Read the logs", End: true}); err != nil {
		t.Fatal(err)
	}
	o.spawn(t, ctx, "second")
}

// Messages arrive as a labelled turn with their origin (never as the
// person's), are kept in both transcripts and the mission, and are bounded
// by size and rate; wait follows the children's state.
func TestOrchestratorMessagesAndWait(t *testing.T) {
	o := newOrchestration(t)
	ctx := timeout(t)
	orch := bearer(ctx, o.orchKey)
	o.grant(t, nil)
	child := o.spawn(t, ctx, "Index the corpus")
	native := o.fake.session(child.SessionId)
	brief := <-native.Applied
	if brief.Kind != agent.Message || brief.Origin == nil || brief.Origin.Kind != agent.OriginAgentBrief || brief.Origin.Conversation == "" || !strings.HasPrefix(brief.Origin.Summary, "Task from") || !strings.HasPrefix(brief.Text, "Index the corpus") {
		t.Fatalf("the brief: %+v", brief)
	}
	// The turn runs; wait sees it running, then idle once it ends.
	native.Events <- agent.Event{Type: "turn_started"}
	got, err := o.client.WaitAgents(orch, &pb.WaitAgentsRequest{SessionId: o.orch.ID, Until: "idle", TimeoutSeconds: 1})
	if err != nil || got.Matched {
		t.Fatalf("idle while the turn runs: %+v %v", got, err)
	}
	native.Events <- agent.Event{Type: "message_delta", Text: "indexing…"}
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	got, err = o.client.WaitAgents(orch, &pb.WaitAgentsRequest{SessionId: o.orch.ID, Until: "idle", TimeoutSeconds: 10})
	if err != nil || !got.Matched || len(got.Children) != 1 || got.Children[0].Live != "idle" {
		t.Fatalf("wait for idle: %+v %v", got, err)
	}
	receipt, err := o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: "Index the corpus", Text: "skip vendor/"})
	if err != nil || receipt.ToConversationId != child.ConversationId {
		t.Fatalf("send by title: %+v %v", receipt, err)
	}
	note := <-native.Applied
	if note.Origin == nil || note.Origin.Kind != agent.OriginAgentNote || note.Origin.Detail != "orchestrator" || !strings.Contains(note.Text, "an agent, not the person") || !strings.Contains(note.Text, "> skip vendor/") {
		t.Fatalf("the note: %+v", note)
	}
	childExec, _ := o.bench.Get(child.SessionId)
	if !journalHas(t, o.bench, childExec, "operation_accepted", `"kind":"agent_note"`) {
		t.Fatal("the child's transcript does not say who wrote the message")
	}
	if !journalHas(t, o.bench, o.orch, "agent_message", "To “Index the corpus”: skip vendor/") || !journalHas(t, o.bench, o.orch, "agent_spawned", "Started “Index the corpus”") {
		t.Fatal("the orchestrator's transcript does not show what it did")
	}
	// Size and rate.
	if _, err = o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: child.ConversationId, Text: strings.Repeat("x", 5000)}); status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("an oversized message: %v", err)
	}
	for i := 0; i < 5; i++ {
		if _, err = o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: child.ConversationId, Text: "again"}); err != nil {
			t.Fatal(i, err)
		}
	}
	if _, err = o.client.SendAgentMessage(orch, &pb.SendAgentMessageRequest{SessionId: o.orch.ID, To: child.ConversationId, Text: "flood"}); status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), "6 messages a minute") {
		t.Fatalf("the seventh message in a minute: %v", err)
	}
	// The child reports; wait --until reported matches and the parent is told.
	before, _ := o.client.ListAgentLineage(orch, &pb.AgentLineageRequest{SessionId: o.orch.ID})
	if _, err = o.client.ReportAgentTask(bearer(ctx, agentToken(t, childExec.Env)), &pb.ReportAgentTaskRequest{SessionId: child.SessionId, Summary: "indexed"}); err != nil {
		t.Fatal(err)
	}
	got, err = o.client.WaitAgents(orch, &pb.WaitAgentsRequest{SessionId: o.orch.ID, Until: "reported", TimeoutSeconds: 5, AfterSeq: before.Seq})
	if err != nil || !got.Matched || !got.Children[0].Reported || got.Children[0].Report != "indexed" || len(got.Events) == 0 {
		t.Fatalf("wait for the report: %+v %v", got, err)
	}
	if !journalHas(t, o.bench, o.orch, "operation_accepted", "reported its task “Index the corpus” done") {
		t.Fatal("the orchestrator was not told of the report")
	}
	// The orchestrator's conversation hears about its grant with its next message, once.
	if note := o.bench.OrchestrationNote(o.orch); !strings.Contains(note, "You orchestrate agents") {
		t.Fatalf("orchestration note: %q", note)
	}
}

// The grant and the lineage outlive a Runner restart with the agents.
func TestOrchestratorLineageOutlivesARunnerRestart(t *testing.T) {
	state := t.TempDir()
	fake := &fleet{}
	first, err := runner.New(runner.Config{State: state, OutputFallbackRoot: t.TempDir(), Address: "http://first.invalid"})
	if err != nil {
		t.Fatal(err)
	}
	first.Adapters["codex"] = fake
	orch, err := first.OpenEmptyWorkspace("codex-default", accounts.Options{Permission: "ask"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	m, err := first.Work.Create(work.Human, "", "Ship search", "")
	if err != nil {
		t.Fatal(err)
	}
	ctx := timeout(t)
	if _, _, err = first.Grant(ctx, runner.GrantRequest{Mission: m.ID, Session: orch.ID}); err != nil {
		t.Fatal(err)
	}
	view, err := first.Spawn(ctx, runner.SpawnRequest{Session: orch.ID, Title: "Index"})
	if err != nil {
		t.Fatal(err)
	}
	child := view.Spawned
	if err = first.Close(); err != nil {
		t.Fatal(err)
	}
	second, err := runner.New(runner.Config{State: state, OutputFallbackRoot: t.TempDir(), Address: "http://second.invalid"})
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	second.Adapters["codex"] = fake
	if !second.OrchestratorGrant(orch.ID) || !second.InLineage(orch.ID, child.Conversation) || !second.InLineage(child.Session, "parent") || second.InLineage(child.Session, orch.ID) {
		t.Fatal("grant or lineage lost across a restart")
	}
	again, err := second.Lineage(ctx, orch.ID)
	if err != nil || len(again.Children) != 1 || again.Children[0].Conversation != child.Conversation || again.Grant == nil {
		t.Fatalf("lineage after restart: %+v %v", again, err)
	}
	e, _ := second.Get(child.Session)
	if l, ok := second.LineageOf(ctx, e); !ok || l.Parent != orch.ID || l.Role != "worker" {
		t.Fatalf("session lineage after restart: %+v", l)
	}
}

func journalHas(t *testing.T, b *runner.Bench, e *runner.Execution, kind, text string) bool {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		for cursor := uint64(0); ; {
			read, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
			record, err := b.Timeline(read, e.ID, cursor)
			cancel()
			if err != nil {
				break
			}
			cursor = record.Sequence
			var ev struct{ Type, Text string }
			if json.Unmarshal(record.Value, &ev) == nil && ev.Type == kind && (strings.Contains(ev.Text, text) || strings.Contains(string(record.Value), text)) {
				return true
			}
		}
		time.Sleep(50 * time.Millisecond)
	}
	return false
}
