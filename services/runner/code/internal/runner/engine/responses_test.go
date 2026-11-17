package engine

import (
	"encoding/json"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// The user's response reaches the provider whole ("always allow" choice,
// decline reason), and the conversation can show it: the operation_accepted
// event carries it in its data, which clients read.
func TestResponsesCarryAlwaysAndReasonToProviderAndConversation(t *testing.T) {
	e, s := bench(t, true)
	ctx := contextFor(t)
	s.Events <- agent.Event{Type: "approval", RequestID: "allow-me"}
	s.Events <- agent.Event{Type: "approval", RequestID: "deny-me"}
	s.Events <- agent.Event{Type: "question", RequestID: "ask-me"}
	await(t, e, "question")
	for _, bad := range []agent.Operation{
		{ID: "a", Kind: agent.Approve, RequestID: "allow-me", Decision: "decline", Always: "session"},
		{ID: "b", Kind: agent.Approve, RequestID: "allow-me", Decision: "accept", Reason: "because"},
	} {
		if _, err := e.Apply(ctx, bad); err == nil {
			t.Fatalf("accepted %+v", bad)
		}
	}
	for _, op := range []agent.Operation{
		{ID: "always", Kind: agent.Approve, RequestID: "allow-me", Decision: "accept", Always: "conversation"},
		{ID: "deny", Kind: agent.Approve, RequestID: "deny-me", Decision: "decline", Reason: "use the Makefile"},
		{ID: "answer", Kind: agent.Answer, RequestID: "ask-me", Answers: map[string][]string{"Which database?": {"Postgres"}}},
	} {
		if _, err := e.Apply(ctx, op); err != nil {
			t.Fatal(err)
		}
		if got := applied(t, s); got.Always != op.Always || got.Reason != op.Reason || len(got.Answers) != len(op.Answers) {
			t.Fatalf("provider got %+v for %+v", got, op)
		}
	}
	type response struct {
		Kind, Decision, Always, Reason string
		Answers                        map[string][]string
	}
	seen := map[string]response{}
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
		var data struct{ Response response }
		if err = json.Unmarshal(ev.Data, &data); err != nil {
			t.Fatal(err)
		}
		seen[ev.RequestID] = data.Response
	}
	if seen["allow-me"].Always != "conversation" || seen["allow-me"].Decision != "accept" ||
		seen["deny-me"].Reason != "use the Makefile" || seen["deny-me"].Decision != "decline" ||
		seen["ask-me"].Kind != "answer" || seen["ask-me"].Answers["Which database?"][0] != "Postgres" {
		t.Fatalf("journaled responses: %+v", seen)
	}
}
