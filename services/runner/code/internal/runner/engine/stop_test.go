package engine

import (
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A stop belongs to the turn it was asked in. When the next turn starts
// without the stopped one's end being reported, that next turn's error is a
// real error again: it is journaled, faults, and ends the turn as failed.
func TestAStopDoesNotOutliveItsTurn(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "run the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	s.Events <- agent.Event{Type: "turn_started"}
	await(t, e, "turn_started")
	if _, err := e.Apply(ctx, agent.Operation{ID: "stop", Kind: agent.Interrupt}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	// The provider moves on to a turn of its own; the stopped turn's end never came.
	s.Events <- agent.Event{Type: "turn_started"}
	s.Events <- claudeStopReport[0]
	s.Events <- agent.Event{Type: "turn_ended", Status: "failed"}
	await(t, e, "turn_ended")
	if failures := journaled(t, e, "failure"); len(failures) != 1 {
		t.Fatalf("the later turn's error was dropped as the stop's report: %+v", failures)
	}
	if ended := journaled(t, e, "turn_ended"); len(ended) != 1 || ended[0].Status != "failed" {
		t.Fatalf("the later turn ended as %+v", ended)
	}
}

func TestTurnStop(t *testing.T) {
	var s turnStop
	// Asked during a turn: that turn ends stopped, and the stop is gone.
	s.started()
	s.ask()
	if !s.stopping() || !s.ended() || s.stopping() {
		t.Fatal("a stop during a turn")
	}
	// Asked between a message and its turn's first event: that turn is stopped.
	s.ask()
	s.started()
	if !s.stopping() {
		t.Fatal("a stop before the turn's first event did not reach it")
	}
	s.ended()
	// Asked during turn N, which never reports its end: turn N+1 is not stopped.
	s.started()
	s.ask()
	s.started()
	if s.stopping() || s.ended() {
		t.Fatal("a stop outlived its turn")
	}
	// A stop that failed is taken back; one asked later stays.
	s.started()
	first := s.ask()
	s.forget(first)
	if s.stopping() {
		t.Fatal("a failed stop stayed")
	}
	s.ended()
	// A stop waiting for a message that was never delivered is dropped.
	s.ask()
	s.undelivered()
	s.started()
	if s.stopping() {
		t.Fatal("a stop waited for a turn that never came, and took the next one")
	}
	// Adopted mid-turn with a stop pending: that turn is stopped, not the next.
	s.adopted(true, true)
	if !s.stopping() {
		t.Fatal("the adopted stop was lost")
	}
	s.started()
	if s.stopping() {
		t.Fatal("the adopted stop reached the next turn")
	}
}
