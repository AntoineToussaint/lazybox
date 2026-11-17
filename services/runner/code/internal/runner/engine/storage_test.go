package engine

import (
	"encoding/json"
	"errors"
	"io/fs"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

// loseStream ends the engine's stream from the agent as the subscription
// layer did when a metering write hit a full disk: the cause as a
// process_exit, then the supervisor's end. The engine records the second as
// its fault, "provider stream ended: provider supervisor ended".
func loseStream(t *testing.T, e *Engine, native *agenttest.Session, cause string) {
	t.Helper()
	native.Events <- agent.Event{Type: "process_exit", Status: "unknown", Text: cause}
	native.End(errors.New("provider supervisor ended"))
	for end := time.Now().Add(5 * time.Second); e.Streaming(); time.Sleep(5 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatal("the stream never ended")
		}
	}
}

// The Dev journal that left a live agent's conversation failed: the Runner's
// SQLite write failed ("database or disk is full (13)"), the supervisor
// ended, and the next Runner re-adopted the still-running agent, whose
// buffered turns then arrived and finished. Re-adoption settles the fault,
// and the conversation goes on as the agent reports.
func TestReadoptionSettlesAFaultThatOnlyLostTheStream(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "measure the disk"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", SessionID: "native-ref", Cursor: 1}
	native.Events <- agent.Event{Type: "activity", Status: "thinking", Cursor: 2}
	await(t, e, "activity")
	loseStream(t, e, native, "database or disk is full (13)")
	if s := e.Snapshot(); s.Fault != "provider stream ended: provider supervisor ended" || s.Closed {
		t.Fatalf("the lost stream was not recorded as it was in Dev: %+v", s)
	}
	saved, _ := st.saved()
	if saved.Fault == "" {
		t.Fatal("the fault was not saved: the next Runner would not see it")
	}
	// The Runner restarts: it leaves the agent running, the next one adopts it.
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	s := next.Snapshot()
	if s.Fault != "" || s.Status == agent.Failed || s.Status == agent.Unknown || !s.Busy {
		t.Fatalf("re-adopting the live agent did not settle the lost stream: %+v", s)
	}
	if saved, _ = st.saved(); saved.Fault != "" {
		t.Fatalf("the settled fault was not saved: %q", saved.Fault)
	}
	// The agent's buffered turn arrives and ends normally.
	a.Session.Events <- agent.Event{Type: "message_delta", Text: "The disk is still full.", Cursor: 3}
	a.Session.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 4}
	await(t, next, "turn_ended")
	if s = next.Snapshot(); s.Fault != "" || s.Status == agent.Failed || s.Busy {
		t.Fatalf("a recovered conversation is not ready: %+v", s)
	}
	// And the person can go on.
	if _, err = next.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "go on"}); err != nil {
		t.Fatalf("the recovered conversation refused a message: %v", err)
	}
	if op := applied(t, a.Session); op.ID != "second" {
		t.Fatalf("delivered %q", op.ID)
	}
}

// A provider fault is not the Runner's: re-adoption leaves it for the person.
func TestReadoptionKeepsAProviderLimitFault(t *testing.T) {
	e, native, st, path := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	native.Events <- agent.Event{Type: "failure", Status: "exhausted", Cursor: 2}
	await(t, e, "failure")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	if s := next.Snapshot(); s.Fault != "exhausted" {
		t.Fatalf("re-adoption cleared a provider limit: %+v", s)
	}
	// Nor does a later event from the agent.
	a.Session.Events <- agent.Event{Type: "activity", Status: "requesting", Cursor: 3}
	await(t, next, "activity")
	if s := next.Snapshot(); s.Fault != "exhausted" {
		t.Fatalf("an event cleared a provider limit: %+v", s)
	}
}

// An agent whose native process really exited is not brought back by
// re-adoption: its host reports the exit again, and the conversation fails.
func TestAnAgentThatExitedStaysFailedAfterReadoption(t *testing.T) {
	e, native, st, path := durableEngine(t)
	native.Events <- agent.Event{Type: "session_ready", Cursor: 1}
	await(t, e, "session_ready")
	loseStream(t, e, native, "native process exited (code 1)")
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	next, a, err := adopt(t, path, st)
	if err != nil {
		t.Fatal(err)
	}
	// The host still holds the exit no Runner recorded: it is replayed.
	a.Session.Events <- agent.Event{Type: "process_exit", Status: "unknown", Text: "native process exited (code 1)"}
	a.Session.End(errors.New("provider supervisor ended"))
	for end := time.Now().Add(5 * time.Second); next.Streaming(); time.Sleep(5 * time.Millisecond) {
		if time.Now().After(end) {
			t.Fatal("the replayed exit did not end the stream")
		}
	}
	if s := next.Snapshot(); s.Fault == "" {
		t.Fatalf("an agent that exited came back as live: %+v", s)
	}
	if _, err = next.Apply(contextFor(t), agent.Operation{ID: "late", Kind: agent.Message, Text: "hello?"}); err == nil {
		t.Fatal("a message was accepted for an agent that exited")
	}
}

// diskFull is what the journal's file reports on a full disk.
var diskFull = &fs.PathError{Op: "write", Path: "session.jsonl", Err: syscall.ENOSPC}

func refuse(e *Engine, err error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if err == nil {
		e.refuseWrites = nil
		return
	}
	e.refuseWrites = func() error { return err }
}

// A journal write the disk refuses is the Runner's failure, not the agent's:
// supervision goes on, nothing is acknowledged before it is recorded, the
// write is retried with the same event, and the conversation says what
// happened once it can.
func TestAJournalWriteFailureDoesNotEndSupervision(t *testing.T) {
	e, native, _, _ := durableEngine(t)
	ctx := contextFor(t)
	if _, err := e.Apply(ctx, agent.Operation{ID: "first", Kind: agent.Message, Text: "build it"}); err != nil {
		t.Fatal(err)
	}
	_ = applied(t, native)
	native.Events <- agent.Event{Type: "turn_started", Cursor: 1}
	await(t, e, "turn_started")
	if _, err := native.AwaitCommits(ctx, 1); err != nil {
		t.Fatal(err)
	}
	refuse(e, diskFull)
	native.Events <- agent.Event{Type: "message_delta", Text: "written while the disk is full", Cursor: 2}
	time.Sleep(600 * time.Millisecond) // Some retries.
	s := e.Snapshot()
	if s.Fault != "" || s.Closed || native.Closed || !e.Streaming() {
		t.Fatalf("a full disk ended the agent's supervision: %+v (provider closed %v)", s, native.Closed)
	}
	if got := native.Commits(); len(got) != 1 {
		t.Fatalf("an event the journal refused was acknowledged to the host: %v", got)
	}
	// A message cannot be accepted meanwhile, and says why.
	if _, err := e.Apply(ctx, agent.Operation{ID: "second", Kind: agent.Message, Text: "and this"}); err == nil {
		t.Fatal("a message was accepted that could not be recorded")
	} else if want := "the disk is full"; !strings.Contains(err.Error(), want) {
		t.Fatalf("refusal does not say the disk is full: %v", err)
	}
	// Room again: the same event is recorded, then acknowledged.
	refuse(e, nil)
	await(t, e, "message_delta")
	if got, err := native.AwaitCommits(ctx, 2); err != nil || got[1] != 2 {
		t.Fatalf("the recorded event was not acknowledged: %v %v", got, err)
	}
	await(t, e, agent.StorageErrorEvent)
	native.Events <- agent.Event{Type: "turn_ended", Status: "completed", Cursor: 3}
	await(t, e, "turn_ended")
	if s = e.Snapshot(); s.Fault != "" || s.Busy {
		t.Fatalf("the turn did not end normally after the disk recovered: %+v", s)
	}
	if n := journalTypes(t, e)["message_delta"]; n != 1 {
		t.Fatalf("the retried event was recorded %d times", n)
	}
}

// A snapshot that cannot be saved (SQLite: "database or disk is full") is
// reported once and saved again later; the agent goes on.
func TestASnapshotSaveFailureIsReportedAndRetried(t *testing.T) {
	e, native, st, _ := durableEngine(t)
	var mu sync.Mutex
	failing := true
	e.mu.Lock()
	capture := e.capture
	e.capture = func(s Snapshot, settings json.RawMessage) error {
		mu.Lock()
		defer mu.Unlock()
		if failing {
			return errors.New("database or disk is full (13)")
		}
		return capture(s, settings)
	}
	e.mu.Unlock()
	native.Events <- agent.Event{Type: "session_ready", Cursor: 1}
	native.Events <- agent.Event{Type: "turn_started", Cursor: 2}
	await(t, e, "turn_started")
	if s := e.Snapshot(); s.Fault != "" || native.Closed {
		t.Fatalf("a failed save ended the conversation: %+v", s)
	}
	types := journalTypes(t, e)
	if types[agent.StorageErrorEvent] != 1 {
		t.Fatalf("the failed save was not reported once: %v", types)
	}
	mu.Lock()
	failing = false
	mu.Unlock()
	for end := time.Now().Add(10 * time.Second); ; time.Sleep(20 * time.Millisecond) {
		if saved, _ := st.saved(); saved.Busy {
			break
		}
		if time.Now().After(end) {
			t.Fatal("the snapshot was never saved again")
		}
	}
}
