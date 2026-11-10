package engine

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
	"path/filepath"
	"testing"
)

func TestChildEventsDoNotReplaceRootOrResolveItsTurn(t *testing.T) {
	e, s := bench(t, true)
	s.Events <- agent.Event{Type: "turn_started", SessionID: "root-native"}
	s.Events <- agent.Event{Type: "approval", RequestID: "child-approval", AgentID: "child", SessionID: "child-native"}
	s.Events <- agent.Event{Type: "turn_ended", Status: "completed", SessionID: "root-native"}
	await(t, e, "turn_ended")
	snap := e.Snapshot()
	if snap.Reference != "root-native" || snap.Status != agent.WaitingApproval || len(snap.Pending) != 1 || snap.Busy {
		t.Fatalf("child state damaged root: %+v", snap)
	}
	s.Events <- agent.Event{Type: "request_resolved", RequestID: "child-approval"}
	await(t, e, "request_resolved")
	if snap = e.Snapshot(); snap.Status != agent.Running || snap.Agents["root"].Status != agent.Done || snap.Agents["child"].Status != agent.Running {
		t.Fatalf("child work was declared done: %+v", snap)
	}
	s.Events <- agent.Event{Type: "turn_ended", AgentID: "child", Status: "completed", SessionID: "child-native"}
	// A marker ensures both turn-ended events have been reduced.
	s.Events <- agent.Event{Type: "marker"}
	await(t, e, "marker")
	if snap = e.Snapshot(); snap.Status != agent.Done || snap.Reference != "root-native" {
		t.Fatalf("child changed root reference: %+v", snap)
	}
	snap.Agents["root"] = agent.Activity{Status: agent.Failed}
	if e.Snapshot().Agents["root"].Status != agent.Done {
		t.Fatal("snapshot is mutable")
	}
}
func TestActivityBoundAndLostStreamRemainUnknown(t *testing.T) {
	e, _ := bench(t, true)
	e.mu.Lock()
	for i := 0; i < 300; i++ {
		e.observe(agent.Event{Type: "agent_status", AgentID: string(rune(i + 100)), Status: "running"})
	}
	if len(e.state.Agents) != 256 || !e.state.ActivityTruncated {
		t.Fatal("unbounded activity tree")
	}
	e.observe(agent.Event{Type: "process_exit"})
	for _, a := range e.state.Agents {
		if a.Status != agent.Unknown {
			t.Fatal("lost process left live children")
		}
	}
	e.mu.Unlock()
}

type closeFailureAdapter struct{}
type closeFailureSession struct{ *agenttest.Session }

func (closeFailureAdapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	a := &agenttest.Adapter{}
	native, err := a.Open(ctx, o)
	if err != nil {
		return nil, err
	}
	return closeFailureSession{native.(*agenttest.Session)}, nil
}
func (closeFailureSession) Close() error { return errors.New("native exit unconfirmed") }
func TestCloseFailureIsVisibleAndDoesNotClaimStopped(t *testing.T) {
	spool, err := journal.Open(filepath.Join(t.TempDir(), "events"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	defer spool.Dispose()
	e, err := New(context.Background(), closeFailureAdapter{}, agent.Open{ID: "execution"}, spool)
	if err != nil {
		t.Fatal(err)
	}
	if err = e.Close(); err == nil {
		t.Fatal("native close failure swallowed")
	}
	snap := e.Snapshot()
	if snap.Fault != "native_exit_unconfirmed" || snap.Status != agent.Unknown {
		t.Fatalf("false stopped state: %+v", snap)
	}
}
