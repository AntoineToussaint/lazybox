package engine

import (
	"context"
	"encoding/json"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
)

func contextFor(t *testing.T) context.Context {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	t.Cleanup(cancel)
	return ctx
}
func bench(t *testing.T, steer bool) (*Engine, *agenttest.Session) {
	t.Helper()
	spool, err := journal.Open(filepath.Join(t.TempDir(), "session"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	a := &agenttest.Adapter{Steer: steer}
	e, err := New(context.Background(), a, agent.Open{ID: "execution", Profile: "pinned-profile"}, spool)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
	return e, a.Session
}
func await(t *testing.T, e *Engine, kind string) {
	t.Helper()
	ctx := contextFor(t)
	for cursor := uint64(0); ; {
		r, err := e.Events(ctx, cursor)
		if err != nil {
			t.Fatal(err)
		}
		var event agent.Event
		if err = json.Unmarshal(r.Value, &event); err != nil {
			t.Fatal(err)
		}
		if event.Type == kind {
			return
		}
		cursor = r.Sequence
	}
}
func applied(t *testing.T, s *agenttest.Session) agent.Operation {
	t.Helper()
	select {
	case op := <-s.Applied:
		return op
	case <-contextFor(t).Done():
		t.Fatal("operation was never delivered")
		return agent.Operation{}
	}
}

func TestSharedConformanceQueueIdentityAndInterruption(t *testing.T) {
	for _, steer := range []bool{false, true} {
		t.Run(map[bool]string{false: "fake-claude-capabilities", true: "fake-codex-capabilities"}[steer], func(t *testing.T) {
			e, s := bench(t, steer)
			ctx := contextFor(t)
			first := agent.Operation{ID: "first", Kind: agent.Message, Text: "one"}
			if _, err := e.Apply(ctx, first); err != nil {
				t.Fatal(err)
			}
			if applied(t, s).ID != "first" {
				t.Fatal("wrong first operation")
			}
			if _, err := e.Apply(ctx, first); err != nil {
				t.Fatal(err)
			}
			if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "different"}); err == nil {
				t.Fatal("identity collision accepted")
			}
			if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "two"}); err != nil {
				t.Fatal(err)
			}
			if e.Snapshot().Queued != 1 {
				t.Fatal("follow-up not queued")
			}
			if _, err := e.Apply(ctx, agent.Operation{ID: "interrupt", Kind: agent.Interrupt}); err != nil {
				t.Fatal(err)
			}
			if applied(t, s).Kind != agent.Interrupt {
				t.Fatal("follow-up delivered during active turn")
			}
			s.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
			if applied(t, s).ID != "second" {
				t.Fatal("queued turn not delivered")
			}
			_, err := e.Apply(ctx, agent.Operation{ID: "steer", Kind: agent.Steer, Text: "change"})
			if (err == nil) != steer {
				t.Fatalf("steer capability mismatch: %v", err)
			}
			if steer {
				if applied(t, s).Kind != agent.Steer {
					t.Fatal("not steer")
				}
			}
		})
	}
}

func TestPendingRequestsAreCorrelatedAndCannotBeApprovedByChat(t *testing.T) {
	e, s := bench(t, true)
	ctx := contextFor(t)
	s.Events <- agent.Event{Type: "approval", RequestID: "native-request", TurnID: "native-turn"}
	await(t, e, "approval")
	if _, err := e.Apply(ctx, agent.Operation{ID: "wrong", Kind: agent.Approve, RequestID: "wrong", Decision: "accept"}); err == nil {
		t.Fatal("foreign approval accepted")
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "chat", Kind: agent.Message, Text: "approved"}); err != nil {
		t.Fatal(err)
	}
	if len(e.Snapshot().Pending) != 1 {
		t.Fatal("chat resolved native approval")
	}
	_ = applied(t, s)
	if _, err := e.Apply(ctx, agent.Operation{ID: "approval", Kind: agent.Approve, RequestID: "native-request", Decision: "decline"}); err != nil {
		t.Fatal(err)
	}
	if applied(t, s).RequestID != "native-request" {
		t.Fatal("lost request identity")
	}
	s.Events <- agent.Event{Type: "request_resolved", RequestID: "native-request"}
	await(t, e, "request_resolved")
	if _, err := e.Apply(ctx, agent.Operation{ID: "duplicate", Kind: agent.Approve, RequestID: "native-request", Decision: "accept"}); err == nil {
		t.Fatal("resolved request approved again")
	}
}

func TestViewerCancellationDoesNotCancelExecution(t *testing.T) {
	e, s := bench(t, false)
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := e.Events(ctx, 1); err == nil {
		t.Fatal("viewer ignored cancellation")
	}
	if _, err := e.Apply(contextFor(t), agent.Operation{ID: "message", Kind: agent.Message, Text: "still alive"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, s)
	s.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
	await(t, e, "turn_ended")
	if e.Snapshot().Closed {
		t.Fatal("viewer closed execution")
	}
}
