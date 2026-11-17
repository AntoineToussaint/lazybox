package engine

import (
	"context"
	"encoding/json"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

func eventually(t *testing.T, what string, ok func() bool) {
	t.Helper()
	for deadline := time.Now().Add(10 * time.Second); !ok(); time.Sleep(5 * time.Millisecond) {
		if time.Now().After(deadline) {
			t.Fatal("never: " + what)
		}
	}
}

// A conversation longer than its segment budget goes on in the next segment:
// nothing faults, and every event is recorded.
func TestALongConversationRollsOverWithoutFault(t *testing.T) {
	spool, err := journal.Open(filepath.Join(t.TempDir(), "session.jsonl"), 1024, journal.Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	a := &agenttest.Adapter{}
	e, err := New(context.Background(), a, agent.Open{ID: "execution"}, spool)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = e.Close(); _ = spool.Dispose() })
	start := spool.Count()
	for i := 0; i < 60; i++ {
		a.Session.Events <- agent.Event{Type: "message", Text: strings.Repeat("a", 200)}
	}
	eventually(t, "60 events recorded", func() bool { return spool.Count() == start+60 })
	if spool.Segments() < 5 || e.Snapshot().Fault != "" {
		t.Fatalf("%d segments, fault %q", spool.Segments(), e.Snapshot().Fault)
	}
}

// durableSegmented is a re-attachable engine on a segmented journal whose
// segments together hold at most total bytes (0: no bound).
func durableSegmented(t *testing.T, path string, total int64) (*Engine, *agenttest.Session, *store, *journal.Journal) {
	t.Helper()
	spool, err := journal.Open(path, 1024, journal.Rollover(total))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = spool.Dispose() })
	st := &store{}
	a := &agenttest.Adapter{Durable: json.RawMessage(`{"host":"fixture-host"}`)}
	e, err := NewDurable(context.Background(), a, agent.Open{ID: "execution", Profile: "pinned", Host: &agent.Hosting{Root: "/hosts"}}, spool, st.capture, st.attach)
	if err != nil {
		t.Fatal(err)
	}
	return e, a.Session, st, spool
}

// A journal that can take nothing more (its total budget is spent) never
// stops the agent, and the conversation can still be ended: it is saved
// closed although session_closed could not be journaled.
func TestASpentJournalNeverStopsTheAgentAndStillEnds(t *testing.T) {
	e, native, st, _ := durableSegmented(t, filepath.Join(t.TempDir(), "session.jsonl"), 4096)
	for i := 0; i < 40; i++ {
		native.Events <- agent.Event{Type: "message", Text: strings.Repeat("a", 200), Cursor: uint64(i + 1)}
		if strings.HasPrefix(e.Snapshot().Fault, "event capture failed") {
			break
		}
	}
	eventually(t, "the capture failure is reported", func() bool {
		return strings.HasPrefix(e.Snapshot().Fault, "event capture failed")
	})
	if e.ctx.Err() != nil {
		t.Fatal("a spent journal canceled the engine")
	}
	if err := e.Close(); err != nil {
		t.Fatal(err)
	}
	if saved, _ := st.saved(); !saved.Closed {
		t.Fatalf("ended, but saved as %+v", saved)
	}
}

// A conversation whose journal failed (event capture failed) and whose agent
// a later Runner re-attaches goes on: the journal, opened again across its
// segments, takes the re-attachment notice and the fault is settled.
func TestReadoptionAcrossSegmentsSettlesACaptureFault(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.jsonl")
	e, native, st, _ := durableSegmented(t, path, 4096)
	for i := 0; i < 40 && !strings.HasPrefix(e.Snapshot().Fault, "event capture failed"); i++ {
		native.Events <- agent.Event{Type: "message", Text: strings.Repeat("a", 200), Cursor: uint64(i + 1)}
		time.Sleep(2 * time.Millisecond)
	}
	eventually(t, "the capture failure is reported", func() bool {
		return strings.HasPrefix(e.Snapshot().Fault, "event capture failed")
	})
	if err := e.Detach(); err != nil {
		t.Fatal(err)
	}
	snapshot, record := st.saved()
	if !strings.HasPrefix(snapshot.Fault, "event capture failed") {
		t.Fatalf("saved fault %q", snapshot.Fault)
	}
	// The next Runner's journal has room (its history budget is larger).
	spool, err := journal.Open(path, 1024, journal.Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = spool.Dispose() })
	if spool.Segments() < 3 {
		t.Fatalf("%d segments", spool.Segments())
	}
	a := &agenttest.Adapter{Durable: record.Provider}
	next, err := Adopt(context.Background(), a, agent.Open{ID: "execution", Profile: "pinned", Host: &agent.Hosting{Root: "/hosts"}}, spool, snapshot, record, st.capture, st.attach)
	if err != nil {
		t.Fatalf("adoption across segments: %v", err)
	}
	t.Cleanup(func() { _ = next.Close() })
	if fault := next.Snapshot().Fault; fault != "" {
		t.Fatalf("fault after re-adoption: %q", fault)
	}
	if n := journalTypes(t, next)["runner_reattached"]; n != 1 {
		t.Fatalf("%d re-attachment notices", n)
	}
	a.Session.Events <- agent.Event{Type: "turn_started", Cursor: 100}
	await(t, next, "turn_started")
}
