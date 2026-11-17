package codex

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// Codex compacting its context is the same line Claude's conversations show:
// a "compacting" phase while it runs, then one `compacted` event with the
// size before (the context in use from the latest token usage), once, even
// though 0.169 reports it twice (the item and the deprecated notification).
func TestCodexCompactionIsOneCompactedLine(t *testing.T) {
	t.Setenv("FIXTURE_COMPACTION", "1")
	self, _ := os.Executable()
	s, err := Adapter{Binary: self}.Open(context.Background(), agent.Open{Cwd: t.TempDir(), Host: &agent.Hosting{Root: fixtureRoot(t)}})
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	_, _ = s.Apply(context.Background(), agent.Operation{ID: "go", Kind: agent.Message, Text: "long work"})
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	compacting, compacted := false, []agent.Event{}
	for {
		ev, err := s.Receive(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if ev.Type == "activity" && ev.Status == "compacting" {
			compacting = true
		}
		if ev.Type == "compacted" {
			compacted = append(compacted, ev)
		}
		if ev.Type == "turn_ended" {
			break
		}
	}
	if !compacting {
		t.Fatal("no compacting phase")
	}
	if len(compacted) != 1 || string(compacted[0].Data) != `{"preTokens":156000,"trigger":"auto"}` || compacted[0].AgentID != "root" {
		t.Fatalf("compacted: %+v", compacted)
	}
}

func TestCompactionStateWithoutUsage(t *testing.T) {
	var c compactionState
	if string(c.data()) != `{"trigger":"auto"}` {
		t.Fatalf("no usage yet: %s", c.data())
	}
	if !c.first("t1") || c.first("t1") || !c.first("t2") || !c.first("") || !c.first("") {
		t.Fatal("one line per turn; an unnamed turn is never held back")
	}
}
