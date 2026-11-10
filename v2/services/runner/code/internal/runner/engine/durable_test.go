package engine

import (
	"context"
	"encoding/json"
	"errors"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
)

// store stands in for the Runner's durable records: the snapshot capture and
// the attachment sidecar.
type store struct {
	mu         sync.Mutex
	snapshot   Snapshot
	attachment Attachment
}

func (s *store) capture(snapshot Snapshot, _ json.RawMessage) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.snapshot = snapshot
	return nil
}
func (s *store) attach(a Attachment) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	// A fresh value, as a record read back from its file would be: an omitted
	// (emptied) queue must not keep the previous one.
	data, _ := json.Marshal(a)
	s.attachment = Attachment{}
	return json.Unmarshal(data, &s.attachment)
}
func (s *store) saved() (Snapshot, Attachment) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.snapshot, s.attachment
}

func durableEngine(t *testing.T) (*Engine, *agenttest.Session, *store, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "session.jsonl")
	spool, err := journal.Open(path, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	st := &store{}
	a := &agenttest.Adapter{Durable: json.RawMessage(`{"host":"fixture-host"}`)}
	e, err := NewDurable(context.Background(), a, agent.Open{ID: "execution", Profile: "pinned", Host: &agent.Hosting{Root: "/hosts"}}, spool, st.capture, st.attach)
	if err != nil {
		t.Fatal(err)
	}
	return e, a.Session, st, path
}

func adopt(t *testing.T, path string, st *store) (*Engine, *agenttest.Adapter, error) {
	t.Helper()
	spool, err := journal.Open(path, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	snapshot, record := st.saved()
	a := &agenttest.Adapter{Durable: record.Provider}
	e, err := Adopt(context.Background(), a, agent.Open{ID: "execution", Profile: "pinned", Host: &agent.Hosting{Root: "/hosts"}}, spool, snapshot, record, st.capture, st.attach)
	if err == nil {
		t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
	}
	return e, a, err
}

func journalTypes(t *testing.T, e *Engine) map[string]int {
	t.Helper()
	counts := map[string]int{}
	ctx := contextFor(t)
	for i := uint64(0); i < e.journal.Count(); i++ {
		r, err := e.journal.Next(ctx, i)
		if err != nil {
			t.Fatal(err)
		}
		var v struct{ Type string }
		_ = json.Unmarshal(r.Value, &v)
		counts[v.Type]++
	}
	return counts
}

func TestDetachedExecutionIsAdoptedWithItsLiveState(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 4}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 5}
	await(t, e, "approval")
	if commits := native.Commits(); len(commits) != 2 || commits[1] != 5 {
		t.Fatalf("recorded events not committed to the transport: %v", commits)
	}
	if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "then this"}); err != nil {
		t.Fatal(err)
	}
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	if !native.Detached || native.Closed {
		t.Fatal("detach stopped the native session")
	}
	if err := e.Close(); err != nil || native.Closed {
		t.Fatal("Close after Detach reached the native session")
	}

	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	attach := a.Session.Open.Host.Attach
	if attach == nil || attach.Cursor != 5 || attach.Reference != "native-ref" || string(attach.State) != `{"host":"fixture-host"}` {
		t.Fatalf("adapter not re-attached from the recorded facts: %+v", attach)
	}
	snapshot := next.Snapshot()
	if !snapshot.Busy || snapshot.Closed || snapshot.Queued != 1 {
		t.Fatalf("live state lost across the Runner restart: %+v", snapshot)
	}
	if _, ok := snapshot.Pending["r1"]; !ok {
		t.Fatal("pending approval lost")
	}
	// The host redelivers the unanswered request: recorded once, not twice.
	a.Session.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 5}
	if _, err = next.Apply(ctx, agent.Operation{ID: "approve", Kind: agent.Approve, RequestID: "r1", Decision: "accept"}); err != nil {
		t.Fatal(err)
	}
	if op := applied(t, a.Session); op.RequestID != "r1" {
		t.Fatal("approval after adoption not delivered")
	}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 6}
	if op := applied(t, a.Session); op.ID != "second" || op.Text != "then this" {
		t.Fatalf("queued message lost across the restart: %+v", op)
	}
	if counts := journalTypes(t, next); counts["approval"] != 1 || counts["runner_reattached"] != 1 || counts["session_opened"] != 1 {
		t.Fatalf("journal continuity: %v", counts)
	}
	// An operation ID retried after the restart returns its original receipt.
	r, err := next.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"})
	if err != nil || r.Delivery != "acknowledged" {
		t.Fatalf("retried operation re-delivered or lost its receipt: %+v %v", r, err)
	}
}

func TestAdoptionAfterCrashMarksUnprovenDeliveryUnknown(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.jsonl")
	spool, err := journal.Open(path, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	// What a Runner killed mid-dispatch leaves: the message accepted, no receipt.
	for _, v := range []any{
		map[string]any{"type": "session_opened"},
		map[string]any{"type": "operation_accepted", "operationId": "lost", "kind": "message", "text": "do it"},
		agent.Event{Type: "turn_started", Cursor: 9},
	} {
		if _, err = spool.Append(v); err != nil {
			t.Fatal(err)
		}
	}
	spool.Dispose()
	st := &store{snapshot: Snapshot{ID: "execution", Busy: true, Reference: "native-ref", Pending: map[string]agent.Event{}}, attachment: Attachment{Version: AttachmentVersion, Provider: json.RawMessage(`{"host":"h"}`)}}
	e, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if a.Session.Open.Host.Attach.Cursor != 9 {
		t.Fatal("cursor not recovered from the journal")
	}
	r, err := e.Apply(contextFor(t), agent.Operation{ID: "lost", Kind: agent.Message, Text: "do it"})
	if err != nil || r.Delivery != "unknown_after_runner_restart" {
		t.Fatalf("unproven delivery not reported as unknown: %+v %v", r, err)
	}
	select {
	case op := <-a.Session.Applied:
		t.Fatalf("message silently redelivered after the crash: %+v", op)
	default:
	}
}

func TestDetachRefusedWhenProviderCannotOutliveTheRunner(t *testing.T) {
	spool, err := journal.Open(filepath.Join(t.TempDir(), "session"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	a := &agenttest.Adapter{}
	e, err := NewDurable(context.Background(), a, agent.Open{ID: "execution"}, spool, nil, func(Attachment) error { return nil })
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = e.Close(); _ = spool.Dispose() }()
	if err = e.Detach(); !errors.Is(err, ErrNotDetachable) {
		t.Fatalf("a local provider claimed it could detach: %v", err)
	}
	if e.Snapshot().Closed {
		t.Fatal("refused detach closed the execution")
	}
}

func TestAdoptRejectsAClosedExecution(t *testing.T) {
	_, _, st, path := durableEngine(t)
	st.snapshot.Closed = true
	if _, _, err := adopt(t, path, st); err == nil {
		t.Fatal("closed execution adopted")
	}
}

// One native line can yield several events. A Runner that dies after recording
// some of them must neither lose the rest nor record the first ones twice.
func TestLineRecordedInPartIsCompletedOnceAfterAdoption(t *testing.T) {
	e, native, st, path := durableEngine(t)
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 6}
	native.Events <- agent.Event{Type: "failure", Status: "rate_limited", Cursor: 7, Partial: true}
	await(t, e, "failure")
	// The Runner goes away before the line's last event (turn_ended) arrives.
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if cursor := a.Session.Open.Host.Attach.Cursor; cursor != 6 {
		t.Fatalf("re-attached after cursor %d: a line recorded only in part must be replayed", cursor)
	}
	// The host replays line 7 whole; a fresh adapter may add an event the old
	// one had suppressed, so matching is by content, not position.
	a.Session.Events <- agent.Event{Type: "activity", Status: "thinking", Cursor: 7, Partial: true}
	a.Session.Events <- agent.Event{Type: "failure", Status: "rate_limited", Cursor: 7, Partial: true}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "failed", Cursor: 7}
	await(t, next, "turn_ended")
	counts := journalTypes(t, next)
	if counts["failure"] != 1 || counts["turn_ended"] != 1 || counts["activity"] != 1 {
		t.Fatalf("replayed line recorded %v", counts)
	}
	if commits := a.Session.Commits(); len(commits) == 0 || commits[len(commits)-1] != 7 {
		t.Fatalf("completed line not acknowledged: %v", commits)
	}
}

// What the previous Runner recorded of a line only deduplicates that line's
// replay: once it completes, an identical event is new and is recorded.
func TestReplayDeduplicationEndsWithTheReplayedLine(t *testing.T) {
	e, native, st, path := durableEngine(t)
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 6}
	native.Events <- agent.Event{Type: "activity", Status: "thinking", Cursor: 7, Partial: true}
	await(t, e, "activity")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	// The replayed line 7 now yields different events: the recorded activity
	// is not among them, so its entry is left over when the line completes.
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 7}
	await(t, next, "turn_ended")
	// A new native session numbers its lines from 1 again (after a profile
	// switch); an event identical to the leftover must still be recorded.
	a.Session.Events <- agent.Event{Type: "profile_changed", Text: "other"}
	a.Session.Events <- agent.Event{Type: "activity", Status: "thinking", Cursor: 7, Partial: true}
	a.Session.Events <- agent.Event{Type: "message_delta", Text: "marker", Cursor: 7}
	await(t, next, "message_delta")
	if counts := journalTypes(t, next); counts["activity"] != 2 {
		t.Fatalf("a new event matching a leftover replay entry was dropped: %v", counts)
	}
}

// Adoption hands the layers above the conversation the journal recorded, minus
// messages still queued (they join it when delivered).
func TestAdoptionPassesTheRecordedConversation(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	await(t, e, "receipt")
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 1}
	native.Events <- agent.Event{Type: "message_delta", Text: "Building ", Cursor: 2}
	native.Events <- agent.Event{Type: "message_delta", Text: "a sub-agent aside", AgentID: "child", Cursor: 3}
	native.Events <- agent.Event{Type: "message_delta", Text: "now.", Cursor: 4}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 5}
	await(t, e, "approval")
	if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "queued, not delivered"}); err != nil {
		t.Fatal(err)
	}
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	_, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if got := a.Session.Open.Host.Attach.Transcript; got != "\nUSER: build it\nBuilding now." {
		t.Fatalf("transcript: %q", got)
	}
}

// The user's decision is journaled with the response, so a request redelivered
// after a restart can be answered as the user answered it.
func TestAdoptionPassesTheRecordedDecision(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 1}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 2}
	await(t, e, "approval")
	if _, err := e.Apply(ctx, agent.Operation{ID: "no", Kind: agent.Approve, RequestID: "r1", Decision: "decline"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	_, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if got := a.Session.Open.Host.Attach.Responded["r1"]; got.Kind != agent.Approve || got.Decision != "decline" {
		t.Fatalf("recorded response: %+v", got)
	}
}

// A message queued during a reply joins the rebuilt conversation where it was
// delivered, after that reply, as it did in the live one.
func TestAdoptedConversationPlacesMessagesWhereTheyWereDelivered(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "first"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	await(t, e, "receipt")
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 1}
	native.Events <- agent.Event{Type: "message_delta", Text: "part one ", Cursor: 2}
	await(t, e, "message_delta")
	if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "second"}); err != nil {
		t.Fatal(err)
	}
	native.Events <- agent.Event{Type: "message_delta", Text: "part two.", Cursor: 3}
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 4}
	_ = applied(t, native) // The queued message is delivered after the turn.
	for end := time.Now().Add(5 * time.Second); journalTypes(t, e)["receipt"] < 2; time.Sleep(10 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatal("second message never delivered")
		}
	}
	native.Events <- agent.Event{Type: "turn_started", Cursor: 5}
	native.Events <- agent.Event{Type: "approval", RequestID: "r1", Cursor: 6}
	await(t, e, "approval")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	_, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if got := a.Session.Open.Host.Attach.Transcript; got != "\nUSER: first\npart one part two.\nUSER: second\n" {
		t.Fatalf("transcript: %q", got)
	}
}
