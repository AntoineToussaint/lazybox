package transport

import (
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// A conversation started before Lazybox could start conversations with another
// provider hears of it with the next message it is sent, once; the session
// shows the update pending until then, and a message sent during a turn
// waits for it without the update.
func TestApplyGivesAnOldConversationWhatChangedOnceAndNeverMidTurn(t *testing.T) {
	ctx := timeout(t)
	b, err := runner.New(runner.Config{State: t.TempDir(), OutputFallbackRoot: t.TempDir(), CLIDir: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	s := &Service{Bench: b}
	pending := func() bool {
		view, err := s.sessionView(ctx, e)
		if err != nil {
			t.Fatal(err)
		}
		return view.InstructionsPending
	}
	if pending() {
		t.Fatal("a new conversation has its briefing")
	}
	// Started before: told everything but how to start another conversation.
	i := strings.Index(e.Saved.Context, "## Starting another conversation")
	if i < 0 {
		t.Fatal("fixture: no conversation section")
	}
	old := func(r *sessionstore.Record) { r.Context, r.Briefing = r.Context[:i], nil }
	if err = b.History.Amend(ctx, e.ID, old); err != nil {
		t.Fatal(err)
	}
	old(&e.Saved)
	if !pending() {
		t.Fatal("the session shows the update pending")
	}
	message := func(id string) {
		t.Helper()
		if _, err := s.Apply(ctx, &pb.ApplyRequest{SessionId: e.ID, OperationId: id, Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: "Continue with Codex"}}}); err != nil {
			t.Fatal(err)
		}
	}
	received := func() agent.Operation {
		t.Helper()
		select {
		case op := <-fake.Session.Applied:
			return op
		case <-ctx.Done():
			t.Fatal("message never reached the agent")
		}
		return agent.Operation{}
	}
	turn := func(start, end bool) {
		if start {
			fake.Session.Events <- agent.Event{Type: "turn_started", TurnID: "t"}
		}
		if end {
			fake.Session.Events <- agent.Event{Type: "turn_ended", TurnID: "t", Status: "completed"}
		}
	}
	busy := func(want bool) {
		t.Helper()
		for e.Engine.Snapshot().Busy != want {
			select {
			case <-ctx.Done():
				t.Fatalf("busy never became %v", want)
			case <-time.After(5 * time.Millisecond):
			}
		}
	}

	// Mid-turn, the message waits in the queue, and goes without the update.
	turn(true, false)
	busy(true)
	message("during")
	turn(false, true)
	if got := received(); got.ID != "during" || got.Context != "" {
		t.Fatalf("a turn in progress is never given the update: %+v", got)
	}
	// Delivered, "during" is a turn of its own (busy from its dispatch).
	turn(true, true)
	busy(false)
	if !pending() {
		t.Fatal("still pending after a message that waited")
	}

	message("next")
	got := received()
	if !strings.HasPrefix(got.Context, "[Lazybox] Since this conversation started") || !strings.Contains(got.Context, "lazybox conversation new --provider P") || strings.Contains(got.Context, "## Your Lazybox workspace") {
		t.Fatalf("the next message carries only what changed: %q", got.Context)
	}
	if got.Text != "Continue with Codex" {
		t.Fatalf("the person's words are their own: %q", got.Text)
	}
	if pending() {
		t.Fatal("delivered: no longer pending")
	}
	turn(true, true)
	busy(false)
	message("after")
	if again := received(); again.Context != "" {
		t.Fatalf("an update is given once: %q", again.Context)
	}
}
