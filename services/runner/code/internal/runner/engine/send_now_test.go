package engine

import (
	"encoding/json"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// Claude ends a turn it was asked to stop with an "error during execution"
// result. A worker started before it learned to drop that report (workers
// outlive Runner upgrades) still sends it, as the Dev journal shows: this is
// the shape of the two occurrences, failure then turn_ended.
var claudeStopReport = []agent.Event{
	{Type: "failure", Status: "provider_error", Text: "[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=tool_use", Account: &agent.AccountObservation{State: "unknown", Reason: "error_during_execution"}},
	{Type: "turn_ended", Status: "interrupted", Data: json.RawMessage(`{"subtype":"error_during_execution"}`)},
}

// journaled is every event of e's journal of type kind, in order.
func journaled(t *testing.T, e *Engine, kind string) []agent.Event {
	t.Helper()
	ctx := contextFor(t)
	var out []agent.Event
	for i := uint64(0); i < e.journal.Count(); i++ {
		r, err := e.journal.Next(ctx, i)
		if err != nil {
			t.Fatal(err)
		}
		var ev agent.Event
		if json.Unmarshal(r.Value, &ev) == nil && ev.Type == kind {
			out = append(out, ev)
		}
	}
	return out
}

// Send now on a Claude turn: the interrupt, then the message as the next
// turn, whatever shape the stopped turn ends in, and the others after it in
// order. The stop Lazybox asked for is no failure: nothing is journaled as
// one, the conversation is not faulted, no account state is observed.
func TestSendNowDeliversAfterClaudeReportsTheStopAsAnError(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "run the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	s.Events <- agent.Event{Type: "turn_started"}
	for _, id := range []string{"first", "second", "urgent"} {
		if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id}); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := e.DeliverQueued(ctx, "urgent", DeliverNow); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, s); op.Kind != agent.Interrupt || op.ID != "urgent:now" {
		t.Fatalf("send now did not interrupt: %+v", op)
	}
	for _, ev := range claudeStopReport {
		s.Events <- ev
	}
	if op := applied(t, s); op.ID != "urgent" || op.Kind != agent.Message {
		t.Fatalf("the message sent now is not the next turn: %+v", op)
	}
	snapshot := e.Snapshot()
	if snapshot.Fault != "" || snapshot.Account != nil {
		t.Fatalf("the requested stop faulted the conversation or observed an account: fault %q account %+v", snapshot.Fault, snapshot.Account)
	}
	if failures := journaled(t, e, "failure"); len(failures) != 0 {
		t.Fatalf("the requested stop was journaled as a failure: %+v", failures)
	}
	if ended := journaled(t, e, "turn_ended"); len(ended) != 1 || ended[0].Status != "interrupted" {
		t.Fatalf("the stopped turn is not recorded as interrupted: %+v", ended)
	}
	// The rest follow in order, one turn each.
	for _, want := range []string{"first", "second"} {
		s.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
		if op := applied(t, s); op.ID != want {
			t.Fatalf("queued messages out of order: got %q, want %q", op.ID, want)
		}
	}
}

// A worker that says the stopped turn "failed" is still read as interrupted
// when Lazybox asked for the stop.
func TestRequestedStopEndsTheTurnInterrupted(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "run the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	if _, err := e.Apply(ctx, agent.Operation{ID: "stop", Kind: agent.Interrupt}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	s.Events <- claudeStopReport[0]
	s.Events <- agent.Event{Type: "turn_ended", Status: "failed"}
	await(t, e, "turn_ended")
	if ended := journaled(t, e, "turn_ended"); len(ended) != 1 || ended[0].Status != "interrupted" {
		t.Fatalf("a requested stop recorded as %+v", ended)
	}
	if s := e.Snapshot(); s.Fault != "" || s.Agents["root"].Status != agent.Interrupted {
		t.Fatalf("after a requested stop: fault %q, root %s", s.Fault, s.Agents["root"].Status)
	}
}

// Without a stop Lazybox asked for, the same report is a real error: it is a
// failure, journaled and shown. A limit is never a stop, asked for or not.
func TestErrorDuringExecutionWithoutAStopIsStillAFailure(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "run the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	s.Events <- claudeStopReport[0]
	s.Events <- agent.Event{Type: "turn_ended", Status: "failed"}
	await(t, e, "turn_ended")
	if failures := journaled(t, e, "failure"); len(failures) != 1 {
		t.Fatalf("an unrequested error during execution was not a failure: %+v", failures)
	}
	if fault := e.Snapshot().Fault; fault != "provider_error" {
		t.Fatalf("fault %q", fault)
	}
	if ended := journaled(t, e, "turn_ended"); ended[0].Status != "failed" {
		t.Fatalf("an unrequested error recorded as %q", ended[0].Status)
	}

	for _, ev := range []agent.Event{{Type: "failure", Status: "exhausted", Account: &agent.AccountObservation{State: "exhausted"}}, {Type: "failure", Status: "provider_error", Account: &agent.AccountObservation{State: "auth_required"}}} {
		if stopReport(ev) {
			t.Fatalf("a limit or sign-in failure taken for a stop report: %+v", ev)
		}
	}
}

// A provider error ends a turn while messages wait: the agent is idle, so
// the first of them continues the conversation and clears the error, as a
// message sent then would. A message never sits in the queue while the agent
// is idle.
func TestQueueDrainsWhenATurnEndsWithAProviderError(t *testing.T) {
	e, s := bench(t, false)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "look at this image"}); err != nil {
		t.Fatal(err)
	}
	applied(t, s)
	if _, err := e.Apply(ctx, agent.Operation{ID: "waiting", Kind: agent.Message, Text: "then this"}); err != nil {
		t.Fatal(err)
	}
	s.Events <- agent.Event{Type: "failure", Status: "provider_error", Text: "Could not process image"}
	s.Events <- agent.Event{Type: "turn_ended", Status: "failed"}
	if op := applied(t, s); op.ID != "waiting" {
		t.Fatalf("the queued message was not delivered after the error: %+v", op)
	}
	if snapshot := e.Snapshot(); snapshot.Fault != "" || snapshot.Queued != 0 {
		t.Fatalf("after continuing: fault %q, queued %d", snapshot.Fault, snapshot.Queued)
	}
	if failures := journaled(t, e, "failure"); len(failures) != 1 {
		t.Fatal("the real error must stay in the conversation")
	}
}

// The Dev occurrence: a Runner restart (detach, then adopt) while a turn
// runs with messages queued, then Send now on the adopted engine. The queue
// carried in the attachment is the one Send now reorders and the one that
// drains: the message is delivered as soon as the stopped turn ends.
func TestSendNowDeliversOnAnAdoptedSession(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "run the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 1}
	await(t, e, "turn_started")
	for _, id := range []string{"later", "urgent"} {
		if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id}); err != nil {
			t.Fatal(err)
		}
	}
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if q := next.Snapshot().QueuedMessages; len(q) != 2 {
		t.Fatalf("the queue was not carried across the restart: %+v", q)
	}
	if _, err := next.DeliverQueued(ctx, "urgent", DeliverNow); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, a.Session); op.Kind != agent.Interrupt {
		t.Fatalf("send now did not interrupt: %+v", op)
	}
	for i, ev := range claudeStopReport {
		ev.Cursor = uint64(2 + i)
		a.Session.Events <- ev
	}
	if op := applied(t, a.Session); op.ID != "urgent" {
		t.Fatalf("the message sent now is not the next turn after adoption: %+v", op)
	}
	if fault := next.Snapshot().Fault; fault != "" {
		t.Fatalf("fault %q", fault)
	}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 4}
	if op := applied(t, a.Session); op.ID != "later" {
		t.Fatalf("the rest of the queue: %+v", op)
	}
}

// Send now, then a Runner restart before the stopped turn ends: the next
// Runner knows the stop was asked for, so the report is no failure there
// either, and the message goes next.
func TestStopAskedBeforeARestartIsStillAStopAfterIt(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "run the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	await(t, e, "turn_started")
	if _, err := e.Apply(ctx, agent.Operation{ID: "urgent", Kind: agent.Message, Text: "urgent"}); err != nil {
		t.Fatal(err)
	}
	if _, err := e.DeliverQueued(ctx, "urgent", DeliverNow); err != nil {
		t.Fatal(err)
	}
	applied(t, native)
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	for i, ev := range claudeStopReport {
		ev.Cursor = uint64(2 + i)
		a.Session.Events <- ev
	}
	if op := applied(t, a.Session); op.ID != "urgent" {
		t.Fatalf("the message sent now before the restart: %+v", op)
	}
	if failures := journaled(t, next, "failure"); len(failures) != 0 {
		t.Fatalf("the stop asked before the restart became a failure: %+v", failures)
	}
}

// The second Dev occurrence, as the next Runner finds it: the stopped turn
// ended with a provider error recorded, the agent idle, two messages queued.
// Adopting it delivers them, the one sent now first.
func TestAdoptionDrainsAQueueLeftBehindAProviderError(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "turn", Kind: agent.Message, Text: "run the build"}); err != nil {
		t.Fatal(err)
	}
	applied(t, native)
	for _, id := range []string{"urgent", "later"} {
		if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id}); err != nil {
			t.Fatal(err)
		}
	}
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	// What a Runner before this fix left: idle, faulted, the queue carried.
	snapshot, record := st.saved()
	snapshot.Busy, snapshot.Fault = false, "provider_error"
	st.snapshot = snapshot
	st.attachment = record
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if op := applied(t, a.Session); op.ID != "urgent" {
		t.Fatalf("adoption left the queue waiting: %+v", op)
	}
	if fault := next.Snapshot().Fault; fault != "" {
		t.Fatalf("fault %q", fault)
	}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 1}
	if op := applied(t, a.Session); op.ID != "later" {
		t.Fatalf("out of order: %+v", op)
	}
}

func TestRepeatedSendNowKeepsPriorityOrderAndLetsEachTurnFinish(t *testing.T) {
	e, provider := bench(t, false)
	ctx := contextFor(t)
	send := func(id string) {
		t.Helper()
		if _, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id}); err != nil {
			t.Fatal(err)
		}
	}
	now := func(id string) {
		t.Helper()
		if _, err := e.DeliverQueued(ctx, id, DeliverNow); err != nil {
			t.Fatal(err)
		}
	}
	expect := func(id string) {
		t.Helper()
		if op := applied(t, provider); op.ID != id {
			t.Fatalf("provider got %s, want %s", op.ID, id)
		}
	}
	send("original")
	expect("original")
	send("ordinary")
	send("first")
	send("second")
	now("first")
	expect("first:now")
	now("second")
	provider.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	expect("first")
	send("third")
	now("third")
	for _, id := range []string{"second", "third", "ordinary"} {
		provider.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
		expect(id)
	}
}

func TestPriorityComposerSendSurvivesRestartAndRetry(t *testing.T) {
	e, native, store, path := durableEngine(t)
	ctx := contextFor(t)
	first := agent.Operation{ID: "first", Kind: agent.Message, Text: "first", Priority: true}
	if _, err := e.Apply(ctx, first); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, native); op.ID != first.ID {
		t.Fatalf("idle send: %+v", op)
	}
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 1}
	await(t, e, "turn_started")
	for _, op := range []agent.Operation{
		{ID: "ordinary", Kind: agent.Message, Text: "later"},
		{ID: "second", Kind: agent.Message, Text: "second", Priority: true},
	} {
		if _, err := e.Apply(ctx, op); err != nil {
			t.Fatal(err)
		}
	}
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, adapter, err := adopt(t, path, store)
	if err != nil {
		t.Fatal(err)
	}
	third := agent.Operation{ID: "third", Kind: agent.Message, Text: "third", Priority: true}
	r, err := next.Apply(ctx, third)
	if err != nil || r.Delivery != "prioritized" || r.Interrupted {
		t.Fatalf("after restart: %+v %v", r, err)
	}
	third.Priority = false
	if _, err := next.Apply(ctx, third); err != nil {
		t.Fatal(err)
	}
	for i, want := range []string{"second", "third", "ordinary"} {
		adapter.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: uint64(2 + i)}
		if op := applied(t, adapter.Session); op.ID != want {
			t.Fatalf("got %+v, want %s", op, want)
		}
	}
}

func TestSteerDuringSendNowKeepsMessageQueued(t *testing.T) {
	e, provider := bench(t, true)
	ctx := contextFor(t)
	for _, op := range []agent.Operation{
		{ID: "active", Kind: agent.Message, Text: "start"},
		{ID: "urgent", Kind: agent.Message, Text: "next", Priority: true},
		{ID: "follow", Kind: agent.Message, Text: "detail"},
	} {
		if _, err := e.Apply(ctx, op); err != nil {
			t.Fatal(err)
		}
		if op.ID != "follow" {
			applied(t, provider)
		}
	}
	if _, err := e.DeliverQueued(ctx, "follow", DeliverSteer); err == nil {
		t.Fatal("steered into the stopping turn")
	}
	provider.Events <- agent.Event{Type: "turn_ended", Status: "interrupted"}
	if op := applied(t, provider); op.ID != "urgent" {
		t.Fatalf("next turn: %+v", op)
	}
	if _, err := e.DeliverQueued(ctx, "follow", DeliverSteer); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, provider); op.Kind != agent.Steer || op.ID != "follow" {
		t.Fatalf("steer: %+v", op)
	}
}

func TestPriorityQueueAdvancesBeforeInterruptReceipt(t *testing.T) {
	e, provider := bench(t, false)
	ctx := contextFor(t)
	submit := func(id string, priority bool) error {
		_, err := e.Apply(ctx, agent.Operation{ID: id, Kind: agent.Message, Text: id, Priority: priority})
		return err
	}
	if err := submit("active", false); err != nil {
		t.Fatal(err)
	}
	applied(t, provider)
	release := make(chan struct{})
	defer close(release)
	provider.Hold(agent.Interrupt, release, nil)
	go func() { _ = submit("first", true) }()
	for e.Snapshot().Queued == 0 {
		select {
		case <-ctx.Done():
			t.Fatal("priority message was not accepted")
		case <-time.After(time.Millisecond):
		}
	}
	if err := submit("second", true); err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"first", "second"} {
		provider.Events <- agent.Event{Type: "turn_ended", Status: "completed"}
		if op := applied(t, provider); op.ID != want {
			t.Fatalf("got %s, want %s", op.ID, want)
		}
	}
}
