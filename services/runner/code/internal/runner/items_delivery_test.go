package runner

import (
	"context"
	"encoding/json"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Simulate a delayed origin lookup by executing the link write only after
// the conversation has been handed off. It may still track the workspace's
// PR, but neither a stale nor an unassigned completion can claim the new task.
func TestDeliveryUsesOriginalAttemptAcrossHandoff(t *testing.T) {
	source := upstreamOn(t, "main")
	b := freshBench(t, collections.Repository{ID: "api", Name: "api", Path: source})
	ws, err := b.Collections.Create(context.Background(), "fixture", collections.Spec{Name: "deliveries", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: gitIn(t, source, "rev-parse", "HEAD")}}})
	if err != nil {
		t.Fatal(err)
	}
	gitIn(t, source, "remote", "add", "origin", "https://github.com/team/api.git")
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenCollection("codex-default", ws.ID, "", accounts.Options{}, "direct", "")
	if err != nil {
		t.Fatal(err)
	}
	m, err := b.Work.Create(work.Human, "", "Release", "")
	if err != nil {
		t.Fatal(err)
	}
	newTask := func(title string) (string, string) {
		t.Helper()
		_, task, err := b.Work.AddTask(m.ID, work.Human, "", "", title, "", nil)
		if err != nil {
			t.Fatal(err)
		}
		_, attempt, err := b.Work.Assign(m.ID, task, work.Human, "", work.AttemptPayload{Conversation: e.ID, Workspace: e.CollectionID})
		if err != nil {
			t.Fatal(err)
		}
		return task, attempt
	}
	_, first := newTask("First")
	b.itemToolEvent(e, agent.Event{Data: json.RawMessage(`{"id":"tool","name":"Bash","status":"running","input":{"command":"gh pr create --fill"}}`)})
	original, ok := b.workItems.tools[e.ID]["\x00tool"]
	if !ok || original != first {
		t.Fatalf("captured attempt %q, want %q", original, first)
	}
	pipe, err := b.RegisterPipe(e, "Push release", true)
	if err != nil {
		t.Fatal(err)
	}
	if err := b.Work.EndAttempts(e.ID, "handoff"); err != nil {
		t.Fatal(err)
	}
	replacement, _ := newTask("Second")
	delivery := workitems.Scan("https://github.com/team/api/pull/12", true)
	b.linkFound(e, delivery, original)
	if err := pipe.Publish(0, []byte("https://github.com/team/api/pull/13\n")); err != nil {
		t.Fatal(err)
	}
	if err := pipe.FinishPipeWith(nil, &commands.Report{ExitCode: 0}); err != nil {
		t.Fatal(err)
	}
	b.itemPipeFinished(pipe)
	for _, number := range []int64{12, 13} {
		if _, err := b.DeliveredLink(ws.ID, workitems.GitHubRef("team/api", number)); err != nil {
			t.Fatalf("workspace lost PR %d: %v", number, err)
		}
	}
	state, err := b.Work.Mission(m.ID)
	if err != nil {
		t.Fatal(err)
	}
	if evidence := state.Task(replacement).Evidence; len(evidence) != 0 {
		t.Fatalf("replacement received evidence from old assignment: %+v", evidence)
	}
	b.linkFound(e, delivery, b.deliveryAttempt(conversationOf(e)))
	state, err = b.Work.Mission(m.ID)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := state.Task(replacement).LastEvidence(work.PullRequestLinkRef("team/api", 12)); !ok {
		t.Fatal("current attempt did not receive its delivery")
	}
}
