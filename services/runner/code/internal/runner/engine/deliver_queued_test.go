package engine

import (
	"encoding/json"
	"errors"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// A queued message is listed while it waits. "Send now" moves it ahead of the
// others and interrupts the turn, so it is the next turn; "steer" delivers it
// into the running turn, only where the provider supports steering, and
// otherwise leaves it where it was.
func TestQueuedMessagesAreListedAndDeliveredNowOrSteered(t *testing.T) {
	for _, steer := range []bool{false, true} {
		t.Run(map[bool]string{false: "claude-like", true: "codex-like"}[steer], func(t *testing.T) {
			e, s := bench(t, steer)
			ctx := contextFor(t)
			if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "start the server"}); err != nil {
				t.Fatal(err)
			}
			applied(t, s)
			for _, id := range []string{"later", "urgent"} {
				if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id + " message"}); err != nil {
					t.Fatal(err)
				}
			}
			queued := e.Snapshot().QueuedMessages
			if len(queued) != 2 || queued[0].OperationID != "later" || queued[1].Text != "urgent message" || queued[1].QueuedAt.IsZero() {
				t.Fatalf("queued messages: %+v", queued)
			}
			if _, err := e.DeliverQueued(ctx, "missing", DeliverNow); !errors.Is(err, ErrNotQueued) {
				t.Fatal("an unknown message", err)
			}
			_, err := e.DeliverQueued(ctx, "urgent", DeliverSteer)
			if !steer {
				if !errors.Is(err, agent.ErrUnsupported) {
					t.Fatal("steering without the capability", err)
				}
				if q := e.Snapshot().QueuedMessages; len(q) != 2 || q[1].OperationID != "urgent" {
					t.Fatalf("a refused steer keeps the message in place: %+v", q)
				}
				// Send now: interrupt, then this message is the next turn.
				if _, err = e.DeliverQueued(ctx, "urgent", DeliverNow); err != nil {
					t.Fatal(err)
				}
				if op := applied(t, s); op.Kind != agent.Interrupt {
					t.Fatalf("send now did not interrupt: %+v", op)
				}
				if q := e.Snapshot().QueuedMessages; len(q) != 2 || q[0].OperationID != "urgent" {
					t.Fatalf("send now moves the message first: %+v", q)
				}
				s.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
				if op := applied(t, s); op.ID != "urgent" || op.Kind != agent.Message {
					t.Fatalf("the next turn is not the message sent now: %+v", op)
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			if op := applied(t, s); op.ID != "urgent" || op.Kind != agent.Steer || op.Text != "urgent message" {
				t.Fatalf("steer delivery: %+v", op)
			}
			if q := e.Snapshot().QueuedMessages; len(q) != 1 || q[0].OperationID != "later" {
				t.Fatalf("a steered message leaves the queue: %+v", q)
			}
			if !e.Snapshot().Busy {
				t.Fatal("steering must not end the turn")
			}
		})
	}
}

// receiptRecord is a journaled receipt as a client reads it: the receipt, and
// the record's status ("steered" for a message steered into a running turn).
type receiptRecord struct {
	Receipt agent.Receipt `json:"receipt"`
	Status  string        `json:"status"`
}

// awaitReceipt waits for id's receipt in e's journal.
func awaitReceipt(t *testing.T, e *Engine, id string) receiptRecord {
	t.Helper()
	ctx := contextFor(t)
	for cursor := uint64(0); ; {
		r, err := e.Events(ctx, cursor)
		if err != nil {
			t.Fatalf("no receipt for %q: %v", id, err)
		}
		var v struct {
			Type    string        `json:"type"`
			Receipt agent.Receipt `json:"receipt"`
			Status  string        `json:"status"`
		}
		if json.Unmarshal(r.Value, &v) == nil && v.Type == "receipt" && v.Receipt.ID == id {
			return receiptRecord{Receipt: v.Receipt, Status: v.Status}
		}
		cursor = r.Sequence
	}
}

// A client shows a queued message where its receipt says it reached the
// agent, so every way a message is delivered journals that receipt: at once,
// at the end of the turn it waited for, by Send now, and by Steer, whose
// receipt says so.
func TestEveryDeliveryOfAMessageJournalsItsReceipt(t *testing.T) {
	for _, steer := range []bool{false, true} {
		t.Run(map[bool]string{false: "claude-like", true: "codex-like"}[steer], func(t *testing.T) {
			e, s := bench(t, steer)
			ctx := contextFor(t)
			if _, err := e.Apply(ctx, agent.Operation{ID: "at-once", Kind: agent.Message, Text: "start"}); err != nil {
				t.Fatal(err)
			}
			applied(t, s)
			if r := awaitReceipt(t, e, "at-once"); r.Receipt.Delivery != "acknowledged" || r.Status != "" {
				t.Fatalf("a message sent while idle: %+v", r)
			}
			s.Events <- agent.Event{Type: "turn_started"}
			for _, id := range []string{"drained", "urgent"} {
				if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id}); err != nil {
					t.Fatal(err)
				}
			}
			if steer {
				if _, err := e.DeliverQueued(ctx, "urgent", DeliverSteer); err != nil {
					t.Fatal(err)
				}
				applied(t, s)
				if r := awaitReceipt(t, e, "urgent"); r.Receipt.Delivery == "" || r.Status != "steered" {
					t.Fatalf("a steered message's receipt does not say it was steered: %+v", r)
				}
			} else {
				if _, err := e.DeliverQueued(ctx, "urgent", DeliverNow); err != nil {
					t.Fatal(err)
				}
				if op := applied(t, s); op.Kind != agent.Interrupt {
					t.Fatalf("send now did not interrupt: %+v", op)
				}
				s.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
				if op := applied(t, s); op.ID != "urgent" {
					t.Fatalf("send now: %+v", op)
				}
				if r := awaitReceipt(t, e, "urgent"); r.Receipt.Delivery != "acknowledged" || r.Status != "" {
					t.Fatalf("a message sent now: %+v", r)
				}
				s.Events <- agent.Event{Type: "turn_started"}
			}
			s.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
			if op := applied(t, s); op.ID != "drained" {
				t.Fatalf("the queue drains in order: %+v", op)
			}
			if r := awaitReceipt(t, e, "drained"); r.Receipt.Delivery != "acknowledged" || r.Status != "" {
				t.Fatalf("a message delivered at the end of a turn: %+v", r)
			}
		})
	}
}

// A message still queued when the Runner restarts is delivered by the next
// one, which journals its receipt.
func TestAMessageQueuedAcrossARestartJournalsItsReceipt(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 1}
	await(t, e, "turn_started")
	if _, err := e.Apply(ctx, agent.Operation{ID: "waiting", Kind: agent.Message, Text: "then this"}); err != nil {
		t.Fatal(err)
	}
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 2}
	if op := applied(t, a.Session); op.ID != "waiting" {
		t.Fatalf("the queued message after the restart: %+v", op)
	}
	if r := awaitReceipt(t, next, "waiting"); r.Receipt.Delivery != "acknowledged" {
		t.Fatalf("its receipt: %+v", r)
	}
}

func TestDeliverNowRetrySendsAnotherInterrupt(t *testing.T) {
	e, session := bench(t, true)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "active", Kind: agent.Message, Text: "run checks"}); err != nil {
		t.Fatal(err)
	}
	applied(t, session)
	if _, err := e.Apply(ctx, agent.Operation{ID: "queued", Kind: agent.Message, Text: "next"}); err != nil {
		t.Fatal(err)
	}
	for attempt := range 2 {
		if _, err := e.DeliverQueued(ctx, "queued", DeliverNow); err != nil {
			t.Fatal(err)
		}
		if operation := applied(t, session); operation.Kind != agent.Interrupt {
			t.Fatalf("attempt %d: %+v", attempt, operation)
		}
	}
	session.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	if operation := applied(t, session); operation.ID != "queued" {
		t.Fatalf("expected queued message once, got %+v", operation)
	}
}

func TestDeleteQueuedLeavesCurrentTurnAndOrderingIntact(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	send := func(id string) {
		t.Helper()
		if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id}); err != nil {
			t.Fatal(err)
		}
	}
	send("current")
	applied(t, s)
	send("first")
	send("deleted")
	send("last")
	for range 2 {
		r, err := e.DeliverQueued(ctx, "deleted", DeleteQueued)
		if err != nil || r.Delivery != "canceled_before_delivery" {
			t.Fatalf("delete receipt = %+v, %v", r, err)
		}
	}
	if _, err := e.DeliverQueued(ctx, "current", DeleteQueued); !errors.Is(err, ErrNotQueued) {
		t.Fatalf("delete current turn: %v", err)
	}
	if state := e.Snapshot(); !state.Busy || len(state.QueuedMessages) != 2 ||
		state.QueuedMessages[0].OperationID != "first" || state.QueuedMessages[1].OperationID != "last" {
		t.Fatalf("unexpected state: %+v", state)
	}
	for _, want := range []string{"first", "last"} {
		s.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
		if op := applied(t, s); op.Kind != agent.Message || op.ID != want {
			t.Fatalf("next operation = %+v, want message %q", op, want)
		}
	}
}

func TestDelegateQueuedRequestsChildWithoutInterrupt(t *testing.T) {
	e, s := bench(t, true)
	ctx := contextFor(t)
	for _, id := range []string{"running", "delegate-me", "later"} {
		if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id}); err != nil {
			t.Fatal(err)
		}
		if id == "running" {
			applied(t, s)
		}
	}
	e.mu.Lock()
	e.state.Capabilities.Subagents = false
	e.mu.Unlock()
	if _, err := e.DeliverQueued(ctx, "delegate-me", DelegateQueued); !errors.Is(err, agent.ErrUnsupported) {
		t.Fatalf("unsupported delegation: %v", err)
	}
	e.mu.Lock()
	e.state.Capabilities.Subagents = true
	e.mu.Unlock()
	if _, err := e.DeliverQueued(ctx, "delegate-me", DelegateQueued); err != nil {
		t.Fatal(err)
	}
	op := applied(t, s)
	if op.Kind != agent.Steer || op.ID != "delegate-me" || !strings.Contains(op.Text, "spawn a sub-agent") || !strings.HasSuffix(op.Text, "\n\ndelegate-me") {
		t.Fatalf("delegation: %+v", op)
	}
	if q := e.Snapshot().QueuedMessages; len(q) != 1 || q[0].OperationID != "later" {
		t.Fatalf("remaining queue: %+v", q)
	}
	if _, err := e.DeliverQueued(ctx, "delegate-me", DelegateQueued); !errors.Is(err, ErrNotQueued) {
		t.Fatalf("duplicate request: %v", err)
	}
}
