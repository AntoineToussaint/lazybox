package engine

import (
	"encoding/json"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// An accepted operation says its kind in the data clients read: a tool call
// that fails right after an interrupt was stopped by the person, not broken.
func TestAcceptedOperationsSayTheirKindToClients(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "ask", Kind: agent.Message, Text: "review it"}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	if _, err := e.Apply(ctx, agent.Operation{ID: "stop", Kind: agent.Interrupt}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	kinds := map[string]string{}
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
		var data struct{ Kind string }
		if err = json.Unmarshal(ev.Data, &data); err != nil {
			t.Fatal(err)
		}
		kinds[ev.OperationID] = data.Kind
	}
	if kinds["ask"] != "message" || kinds["stop"] != "interrupt" {
		t.Fatalf("kinds in the data: %v", kinds)
	}
}
