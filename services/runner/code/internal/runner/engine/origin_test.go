package engine

import (
	"encoding/json"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A message Lazybox wrote reaches the agent as written, and says where it came
// from to everyone else: in its operation_accepted record, which the
// conversation shows as an event rather than the person's words, and in the
// queue while it waits. Its summary stays one bounded line.
func TestLazyboxMessagesCarryTheirOriginAndTheAgentGetsTheTextUnchanged(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "typed", Kind: agent.Message, Text: "start"}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	text := "[Lazybox] You asked to add authority to this workspace. The person added it:\n- authority at /src/authority (branch main)\nContinue with what you needed it for."
	origin := &agent.Origin{Kind: agent.OriginRepositoryAdded, Summary: "Added authority\nto this workspace " + strings.Repeat("x", 400), Detail: "main", Copy: "/src/authority"}
	if _, err := e.Apply(ctx, agent.Operation{ID: "note", Kind: agent.Message, Text: text, Origin: origin}); err != nil {
		t.Fatal(err)
	}
	queued := e.Snapshot().QueuedMessages
	if len(queued) != 1 || queued[0].Origin == nil || queued[0].Origin.Kind != agent.OriginRepositoryAdded {
		t.Fatalf("the queue does not say who wrote it: %+v", queued)
	}
	s.Events <- agent.Event{Type: "turn_ended"}
	if got := applied(t, s); got.Text != text {
		t.Fatalf("the agent got %q", got.Text)
	}
	origins := map[string]*agent.Origin{}
	for i := uint64(0); i < e.journal.Count(); i++ {
		r, err := e.journal.Next(ctx, i)
		if err != nil {
			t.Fatal(err)
		}
		var ev agent.Event
		_ = json.Unmarshal(r.Value, &ev)
		if ev.Type != "operation_accepted" {
			continue
		}
		var data struct{ Origin *agent.Origin }
		if err = json.Unmarshal(ev.Data, &data); err != nil {
			t.Fatal(err)
		}
		if ev.Text == "" {
			t.Fatal("the transcript lost the text the agent received")
		}
		origins[ev.OperationID] = data.Origin
	}
	if origins["typed"] != nil {
		t.Fatal("the person's own message is marked as Lazybox's")
	}
	got := origins["note"]
	if got == nil || got.Kind != agent.OriginRepositoryAdded || got.Detail != "main" || got.Copy != "/src/authority" {
		t.Fatalf("journaled origin: %+v", got)
	}
	if strings.Contains(got.Summary, "\n") || len([]rune(got.Summary)) > 280 || !strings.HasPrefix(got.Summary, "Added authority to this workspace") {
		t.Fatalf("summary is not one bounded line: %q", got.Summary)
	}
}
