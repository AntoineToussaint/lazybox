package engine

import (
	"errors"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// A Runner stopped while a message waited for its turn's code checkpoint: the
// message had left the queue, no receipt or turn was recorded, and the
// conversation was saved busy. The adopting Runner does not wait forever for
// a turn that never started: the message's outcome is unknown, and what is
// queued behind it goes next.
func TestAdoptionDoesNotStayBusyForATurnThatNeverStarted(t *testing.T) {
	e, native, st, path := durableEngine(t)
	native.Events <- agent.Event{Type: "session_ready", Cursor: 1}
	await(t, e, "session_ready")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	spool, err := journal.Open(path, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = spool.Append(map[string]any{"type": "operation_accepted", "operationId": "slow", "kind": agent.Message, "text": "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = spool.Dispose()
	st.mu.Lock()
	st.snapshot.Busy = true
	root := st.snapshot.Agents["root"]
	root.Status = agent.Starting
	st.snapshot.Agents["root"] = root
	st.attachment.Queue = []agent.Operation{{ID: "next", Kind: agent.Message, Text: "then this"}}
	st.mu.Unlock()

	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if r := awaitReceipt(t, next, "slow"); r.Receipt.Delivery != "unknown_after_runner_restart" {
		t.Fatalf("the undelivered message's outcome was not said: %+v", r)
	}
	if op := applied(t, a.Session); op.ID != "next" {
		t.Fatalf("the queued message was not delivered next: %q", op.ID)
	}
}

// A provider error in a turn (or a limit) does not take Stop, approvals or
// answers away: only the Runner's own faults hold them.
func TestAProviderErrorDoesNotBlockStopOrAnswers(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 2}
	native.Events <- agent.Event{Type: "failure", Status: "provider_error", Text: "overloaded", Cursor: 3}
	await(t, e, "failure")
	if f := e.Snapshot().Fault; !agent.TurnFault(f) {
		t.Fatalf("not a turn fault: %q", f)
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "approve", Kind: agent.Approve, RequestID: "r1", Decision: "accept"}); err != nil {
		t.Fatalf("an approval was refused after a provider error: %v", err)
	}
	if op := applied(t, native); op.Kind != agent.Approve {
		t.Fatalf("delivered %s", op.Kind)
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "stop", Kind: agent.Interrupt}); err != nil {
		t.Fatalf("Stop was refused after a provider error: %v", err)
	}
}

// Stop never waits behind a delivery in progress: a message waiting for its
// code checkpoint (up to minutes) holds the delivery lock.
func TestAStopDoesNotWaitBehindADeliveryInProgress(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	release := make(chan struct{})
	defer close(release)
	e.SetTurnHooks(TurnHooks{Before: func(agent.Operation) { <-release }})
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	for end := time.Now().Add(5 * time.Second); !e.Snapshot().Busy; time.Sleep(5 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatal("the delivery never began")
		}
	}
	stopped := make(chan error, 1)
	go func() {
		_, err := e.Apply(ctx, agent.Operation{ID: "stop", Kind: agent.Interrupt})
		stopped <- err
	}()
	select {
	case err := <-stopped:
		if err != nil {
			t.Fatalf("Stop failed: %v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("Stop waited behind a delivery in progress")
	}
	if op := applied(t, native); op.Kind != agent.Interrupt {
		t.Fatalf("delivered %s before the stop", op.Kind)
	}
}

// A provider that never answers a control call does not hold the
// conversation: the call fails after a bounded wait.
func TestAControlCallThatNeverAnswersIsBounded(t *testing.T) {
	previous := controlDeadline
	controlDeadline = 200 * time.Millisecond
	t.Cleanup(func() { controlDeadline = previous })
	e, native := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 2}
	await(t, e, "approval")
	never := make(chan struct{})
	native.Hold(agent.Approve, never, nil)
	done := make(chan error, 1)
	go func() {
		_, err := e.Apply(ctx, agent.Operation{ID: "approve", Kind: agent.Approve, RequestID: "r1", Decision: "accept"})
		done <- err
	}()
	select {
	case err := <-done:
		if err == nil {
			t.Fatal("an unanswered approval was reported delivered")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("a control call the provider never answered was not bounded")
	}
}

// Send now on a message that leaves the queue on its own meanwhile (the turn
// ended, and it started its own) reports it accepted: it was delivered.
func TestSendNowReportsAcceptedWhenTheMessageWasDeliveredAnyway(t *testing.T) {
	e, native := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	await(t, e, "turn_started")
	if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "and this"}); err != nil {
		t.Fatal(err)
	}
	release := make(chan struct{})
	native.Hold(agent.Interrupt, release, errors.New("no turn to stop"))
	result := make(chan error, 1)
	go func() {
		r, err := e.DeliverQueued(ctx, "second", DeliverNow)
		if err == nil && r.Delivery != "accepted" {
			err = errors.New("delivery " + r.Delivery)
		}
		result <- err
	}()
	for end := time.Now().Add(5 * time.Second); ; time.Sleep(5 * time.Millisecond) {
		e.mu.Lock()
		_, asked := e.ops["second:now"]
		e.mu.Unlock()
		if asked {
			break
		}
		if time.Now().After(end) {
			t.Fatal("the stop was never asked")
		}
	}
	// The turn ends before the stop is answered: the message starts its own.
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 2}
	if op := applied(t, native); op.ID != "second" {
		t.Fatalf("delivered %q", op.ID)
	}
	close(release)
	if err := <-result; err != nil {
		t.Fatalf("Send now reported an error for a delivered message: %v", err)
	}
}

// The Runner's journal taking nothing more (its budget spent) stops the
// Runner reading the agent, never the agent: it is left for Resume or the
// next Runner to re-attach.
func TestAJournalThatTakesNothingMoreLeavesTheAgentRunning(t *testing.T) {
	e, native, _, _ := durableEngine(t)
	refuse(e, journal.ErrFull)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	for end := time.Now().Add(5 * time.Second); e.Streaming(); time.Sleep(5 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatal("the engine kept reading what it cannot record")
		}
	}
	if f := e.Snapshot().Fault; !agent.TransportFault(f) {
		t.Fatalf("not a fault Resume settles: %q", f)
	}
	time.Sleep(50 * time.Millisecond)
	if native.WasClosed() {
		t.Fatal("the agent was stopped because the Runner could not record it")
	}
	if !e.Detachable() {
		t.Fatal("the agent can no longer be handed to Resume or the next Runner")
	}
}

func TestMessageAcceptanceDuringTurnCheckpoint(t *testing.T) {
	e, provider := bench(t, false)
	ctx := contextFor(t)
	entered, release := make(chan struct{}), make(chan struct{})
	defer close(release)
	e.SetTurnHooks(TurnHooks{Before: func(op agent.Operation) {
		if op.ID == "first" {
			close(entered)
			<-release
		}
	}})
	send := func(id string) error {
		_, err := e.Apply(ctx, agent.Operation{Kind: agent.Message, ID: id, Text: id})
		return err
	}
	if err := send("first"); err != nil {
		t.Fatal(err)
	}
	select {
	case <-entered:
	case <-ctx.Done():
		t.Fatal("checkpoint did not start")
	}
	result := make(chan error, 1)
	go func() { result <- send("next") }()
	select {
	case err := <-result:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("message acceptance waited for the previous turn's checkpoint")
	}
	if q := e.Snapshot().QueuedMessages; len(q) != 1 || q[0].OperationID != "next" {
		t.Fatalf("queued: %+v", q)
	}
	select {
	case op := <-provider.Applied:
		t.Fatalf("premature dispatch: %+v", op)
	default:
	}
}
