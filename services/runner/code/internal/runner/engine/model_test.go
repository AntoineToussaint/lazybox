package engine

import (
	"errors"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// A model switch reaches a provider that can take it, at once (not behind
// queued messages), and is never shown as something the person said.
func TestModelSwitchNeedsTheCapabilityAndAValidName(t *testing.T) {
	ctx := contextFor(t)
	open := func(live bool) (*Engine, *agenttest.Session) {
		spool, err := journal.Open(filepath.Join(t.TempDir(), "session"), 1<<20)
		if err != nil {
			t.Fatal(err)
		}
		a := &agenttest.Adapter{LiveModel: live}
		e, err := New(ctx, a, agent.Open{ID: "execution", Profile: "pinned-profile"}, spool)
		if err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
		return e, a.Session
	}
	unsupported, _ := open(false)
	if _, err := unsupported.Apply(ctx, agent.Operation{ID: "m", Kind: agent.Model, Model: "opus"}); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("a provider without live model switches must refuse: %v", err)
	}
	e, s := open(true)
	for _, bad := range []string{"", "two words", "new\nline", strings.Repeat("m", 257)} {
		if _, err := e.Apply(ctx, agent.Operation{ID: "bad", Kind: agent.Model, Model: bad}); !errors.Is(err, agent.ErrInvalidModel) {
			t.Fatalf("%q accepted as a model: %v", bad, err)
		}
	}
	// The provider can reject the original model without closing the worker.
	// The next prompt needs the corrected model before it can succeed.
	s.Events <- agent.Event{Type: "failure", Status: "unknown", Text: "model_not_found"}
	eventually(t, "provider model rejection", func() bool { return e.Snapshot().Fault == "unknown" })

	if _, err := e.Apply(ctx, agent.Operation{ID: "m", Kind: agent.Model, Model: "claude-opus-5-5"}); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, s); op.Kind != agent.Model || op.Model != "claude-opus-5-5" || op.Text != "" {
		t.Fatalf("switch delivered as %+v", op)
	}
}

func TestRetryTurnPreservesPromptAndQueueOrder(t *testing.T) {
	ctx := contextFor(t)
	e, s := bench(t, false)
	e.mu.Lock()
	e.state.Capabilities.LiveModel = true
	e.mu.Unlock()
	send := func(op agent.Operation) {
		t.Helper()
		if _, err := e.Apply(ctx, op); err != nil {
			t.Fatal(err)
		}
	}
	input := agent.Operation{ID: "original", Kind: agent.Message, Text: "inspect image", Images: []agent.Image{{MediaType: "image/png", Data: "YQ=="}}}
	send(input)
	applied(t, s)
	s.Events <- agent.Event{Type: "turn_started"}
	await(t, e, "turn_started")
	if e.Snapshot().RetryableOperationID != input.ID {
		t.Fatal("missing retry target")
	}
	send(agent.Operation{ID: "queued", Kind: agent.Message, Text: "afterwards"})
	send(agent.Operation{ID: "selection", Kind: agent.Model, Model: "sonnet"})
	if applied(t, s).Kind != agent.Model {
		t.Fatal("model selection delayed behind queue")
	}
	for _, pair := range [][2]string{{"stale", "selection"}, {"original", "obsolete"}} {
		if _, err := e.RetryTurn(ctx, pair[0], pair[1]); err == nil {
			t.Fatal("stale retry accepted")
		}
	}
	if _, err := e.RetryTurn(ctx, input.ID, "selection"); err != nil {
		t.Fatal(err)
	}
	if applied(t, s).Kind != agent.Interrupt {
		t.Fatal("missing interrupt")
	}
	if _, err := e.RetryTurn(ctx, input.ID, "selection"); err != nil {
		t.Fatal(err)
	}
	s.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	retry := applied(t, s)
	if retry.ID == input.ID || retry.Text != input.Text || len(retry.Images) != 1 || retry.Images[0] != input.Images[0] {
		t.Fatalf("unexpected retry: %+v", retry)
	}
	s.Events <- agent.Event{Type: "turn_ended"}
	if applied(t, s).ID != "queued" {
		t.Fatal("follow-up displaced or retry duplicated")
	}
}
