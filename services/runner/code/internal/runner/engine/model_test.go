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
	if _, err := e.Apply(ctx, agent.Operation{ID: "m", Kind: agent.Model, Model: "claude-opus-5-5"}); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, s); op.Kind != agent.Model || op.Model != "claude-opus-5-5" || op.Text != "" {
		t.Fatalf("switch delivered as %+v", op)
	}
}
