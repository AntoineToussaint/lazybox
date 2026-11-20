package runner

import (
	"context"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Interrupt after the workspace exists but before opening the conversation.
// The next Runner reads the reservation from disk after adopting live hosts.
func TestRestartFinishesReservedPinnedDispatch(t *testing.T) {
	for _, boundary := range []string{"reserved", "workspace", "brief"} {
		t.Run(boundary, func(t *testing.T) { checkRestartedDispatch(t, boundary) })
	}
}

func checkRestartedDispatch(t *testing.T, boundary string) {
	ctx := context.Background()
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	b.Adapters["codex"] = &agenttest.Adapter{}
	parent, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{Permission: "ask"}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	m, err := b.Work.Create(work.Human, "", "Restart test", "")
	if err != nil {
		t.Fatal(err)
	}
	_, g, err := b.Work.IssueGrant(m.ID, work.Human, "", work.GrantPayload{Holder: parent.ID, Ceiling: work.Ceiling{Permission: "ask", Profiles: []string{"codex-default"}, MaxChildren: 1, Depth: 1}})
	if err != nil {
		t.Fatal(err)
	}
	actor := work.Actor{Kind: work.ActorAgent, Role: "orchestrator", Conversation: parent.ID, Execution: parent.ID}
	head := gitIn(t, source, "rev-parse", "HEAD")
	_, p, err := b.Work.ProposePlan(m.ID, actor, "", work.PlanPayload{Grant: g, Tasks: []work.PlanTask{{Key: "worker", Title: "Worker", Workspace: "pinned"}}, Baseline: []work.BaselineMember{{Name: "api", Repository: "api", Commit: head}}})
	if err != nil {
		t.Fatal(err)
	}
	state, err := b.Work.DecidePlan(m.ID, p, work.Human, "", true, 0, 1, "")
	if err != nil {
		t.Fatal(err)
	}
	a := work.AttemptPayload{Conversation: commands.ID(), Workspace: commands.ID(), Parent: parent.ID, Grant: g, Role: "worker", Profile: "codex-default", Permission: "ask", Dispatch: &work.Dispatch{Title: "Worker", Brief: "Inspect the approved baseline", ParentTitle: "Planner", ParentSession: parent.ID}}
	a.Execution = a.Conversation
	task := state.PlanOf(p).Created["worker"]
	_, _, _, err = b.Work.Spawn(m.ID, actor, "reserve", task, "Worker", "", a)
	if err != nil {
		t.Fatal(err)
	}
	if boundary != "reserved" {
		_, err = b.Collections.CreateReserved(ctx, a.Workspace, collections.Spec{Name: "Worker", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: head}}})
		if err != nil {
			t.Fatal(err)
		}
	}
	if boundary == "brief" {
		child, err := b.openWithID(a.Execution, a.Profile, "", "", accounts.Options{Permission: "ask"}, "direct", "", "", a.Workspace, "coding", "", false)
		if err != nil {
			t.Fatal(err)
		}
		_, err = b.deliverAgentMessage(ctx, child, "spawn-"+task+"-"+child.ID, a.Dispatch.Brief, agent.Origin{Kind: agent.OriginAgentBrief}, work.PromptSource{Kind: work.SourceAgent, Mission: m.ID, Task: task})
		if err != nil {
			t.Fatal(err)
		}
	}
	if err = b.Detach(); err != nil {
		t.Fatal(err)
	}
	next, err := New(b.Config)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	fake := &agenttest.Adapter{}
	next.Adapters["codex"] = fake
	if err = next.Adopt(ctx); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(10 * time.Second)
	for {
		s, err := next.Work.Mission(m.ID)
		if err != nil {
			t.Fatal(err)
		}
		current := s.Task(task).Current()
		if !current.Started.IsZero() {
			if current.Conversation != a.Conversation || current.Workspace != a.Workspace || len(s.Task(task).Attempts) != 1 {
				t.Fatalf("reservation changed: %+v", current)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("reserved dispatch never completed")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if boundary == "brief" {
		if fake.Session != nil {
			t.Fatal("already briefed child was restarted")
		}
		return
	}
	select {
	case op := <-fake.Session.Applied:
		if op.Text != a.Dispatch.Brief {
			t.Fatal(op.Text)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("missing recovered brief")
	}
}
