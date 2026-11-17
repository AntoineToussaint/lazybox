package engine

import (
	"context"
	"errors"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	"path/filepath"
	"strings"
	"testing"
	"time"
	"unicode/utf8"
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

func TestLastMessageAndTurnEndKeepWhatTheMainAgentLastSaid(t *testing.T) {
	e, s := bench(t, true)
	s.Events <- agent.Event{Type: "turn_started"}
	s.Events <- agent.Event{Type: "message_delta", Text: "Looking at the tests"}
	s.Events <- agent.Event{Type: "activity", Status: "tool"}
	s.Events <- agent.Event{Type: "message_delta", Text: "All tests pass.\n"}
	s.Events <- agent.Event{Type: "message_delta", Text: "Ready for review."}
	s.Events <- agent.Event{Type: "message_delta", AgentID: "child", Text: "CHILD_PRIVATE"}
	s.Events <- agent.Event{Type: "marker-1"}
	await(t, e, "marker-1")
	snap := e.Snapshot()
	if snap.LastMessage != "All tests pass.\nReady for review." {
		t.Fatalf("last message: %q", snap.LastMessage)
	}
	if !snap.Agents["root"].TurnEndedAt.IsZero() {
		t.Fatal("a running turn has no end time")
	}
	before := time.Now().UTC()
	s.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	await(t, e, "turn_ended")
	ended := e.Snapshot().Agents["root"].TurnEndedAt
	if ended.Before(before) {
		t.Fatalf("turn end not recorded: %v", ended)
	}
	// A status report after the turn moves UpdatedAt, never the turn end.
	s.Events <- agent.Event{Type: "agent_status", Status: "ready"}
	s.Events <- agent.Event{Type: "marker-2"}
	await(t, e, "marker-2")
	if got := e.Snapshot().Agents["root"]; !got.TurnEndedAt.Equal(ended) || got.Status != agent.Done {
		t.Fatalf("status report moved the turn end: %+v", got)
	}
	s.Events <- agent.Event{Type: "message_delta", Text: strings.Repeat("é", 400)}
	s.Events <- agent.Event{Type: "marker-3"}
	await(t, e, "marker-3")
	if got := e.Snapshot().LastMessage; got != strings.Repeat("é", lastMessageLimit/2) || !utf8.ValidString(got) {
		t.Fatalf("bounded tail: %d bytes, valid %v", len(got), utf8.ValidString(got))
	}
}

func TestPhaseSaysWhatTheMainAgentIsDoingAndSince(t *testing.T) {
	e, s := bench(t, true)
	s.Events <- agent.Event{Type: "turn_started"}
	s.Events <- agent.Event{Type: "marker-1"}
	await(t, e, "marker-1")
	root := e.Snapshot().Agents["root"]
	if root.Phase != "working" || root.TurnStartedAt.IsZero() || root.PhaseSince.IsZero() {
		t.Fatalf("a started turn has no phase: %+v", root)
	}
	s.Events <- agent.Event{Type: "activity", Status: "tool", Name: "Bash"}
	s.Events <- agent.Event{Type: "marker-2"}
	await(t, e, "marker-2")
	tool := e.Snapshot().Agents["root"]
	if tool.Phase != "tool" || tool.Tool != "Bash" || tool.PhaseSince.Before(root.PhaseSince) {
		t.Fatalf("tool phase: %+v", tool)
	}
	// The same step reported again keeps its start: elapsed time keeps counting.
	time.Sleep(5 * time.Millisecond)
	s.Events <- agent.Event{Type: "activity", Status: "tool", Name: "Bash"}
	s.Events <- agent.Event{Type: "message_delta", AgentID: "child", Text: "child"}
	s.Events <- agent.Event{Type: "marker-3"}
	await(t, e, "marker-3")
	if again := e.Snapshot().Agents["root"]; !again.PhaseSince.Equal(tool.PhaseSince) || again.Tool != "Bash" {
		t.Fatalf("a repeated report moved the phase start: %+v", again)
	}
	s.Events <- agent.Event{Type: "message_delta", Text: "Done."}
	s.Events <- agent.Event{Type: "marker-4"}
	await(t, e, "marker-4")
	if got := e.Snapshot().Agents["root"]; got.Phase != "responding" || got.Tool != "" {
		t.Fatalf("writing phase: %+v", got)
	}
	s.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	await(t, e, "turn_ended")
	if got := e.Snapshot().Agents["root"]; got.Phase != "" || got.Tool != "" || got.TurnStartedAt.IsZero() {
		t.Fatalf("an ended turn keeps a phase: %+v", got)
	}
}
