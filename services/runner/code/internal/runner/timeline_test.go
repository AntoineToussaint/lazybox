package runner

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

func TestTurnStartIsAMessageThePersonSent(t *testing.T) {
	for value, want := range map[string]bool{
		`{"type":"operation_accepted","kind":"message","operationId":"op-1","text":"Fix the test"}`:                   true,
		`{"type":"operation_accepted","kind":"steer","operationId":"op-2","text":"Also the docs","agentId":"root"}`:   true,
		`{"type":"operation_accepted","kind":"message","text":"Look","data":{"files":[],"origin":null}}`:              true,
		`{"type":"operation_accepted","kind":"message","text":"Review it","data":{"origin":{"kind":"review_brief"}}}`: false,
		`{"type":"operation_accepted","kind":"message","operationId":"review-brief-run-1","text":"Review it"}`:        false,
		`{"type":"operation_accepted","kind":"message","operationId":"review-run-1-findings-2","text":"Findings"}`:    false,
		`{"type":"operation_accepted","kind":"answer","requestId":"q-1","text":"yes"}`:                                false,
		`{"type":"operation_accepted","kind":"interrupt","operationId":"op-1:now"}`:                                   false,
		`{"type":"operation_accepted","kind":"message","text":"For the child","agentId":"claude-tool:abc"}`:           false,
		`{"type":"message_delta","text":"operation_accepted"}`:                                                        false,
		`{"type":"operation_accepted","text":"Legacy journals have no kind"}`:                                         true,
		`{"type":"operation_accepted","text":"data of another shape","data":"not an object"}`:                         false,
	} {
		if got := turnStart(json.RawMessage(value)); got != want {
			t.Errorf("turnStart(%s) = %v, want %v", value, got, want)
		}
	}
}

// conversation records `turns` turns of the person's message, a Lazybox note
// and an answer, then returns the cursor each turn starts at.
func conversation(t *testing.T, b *Bench, e *Execution, turns int, answer string) []uint64 {
	t.Helper()
	var starts []uint64
	for turn := range turns {
		starts = append(starts, e.spool.Count()+1)
		for _, value := range []map[string]any{
			{"type": "operation_accepted", "kind": "message", "operationId": fmt.Sprintf("op-%d", turn), "text": fmt.Sprintf("Question %d", turn)},
			{"type": "turn_started", "turnId": fmt.Sprintf("turn-%d", turn)},
			// A message Lazybox wrote mid-turn does not start a turn.
			{"type": "operation_accepted", "kind": "message", "text": "A note", "data": map[string]any{"origin": map[string]any{"kind": "repository_note"}}},
			{"type": "message_delta", "text": answer},
			{"type": "message_delta", "text": fmt.Sprintf(" ANSWER_%d", turn)},
			{"type": "turn_ended", "status": "completed"},
		} {
			if err := b.Record(e, value); err != nil {
				t.Fatal(err)
			}
		}
	}
	return starts
}

func openTimelineBench(t *testing.T) (*Bench, *Execution, string) {
	t.Helper()
	root := t.TempDir()
	b, err := New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	return b, e, root
}

func sequences(records []journal.Record) []uint64 {
	out := make([]uint64, len(records))
	for i, r := range records {
		out[i] = r.Sequence
	}
	return out
}

// The engine's own record of a message the person sent is a turn start.
func TestTheEnginesRecordOfAMessageIsATurnStart(t *testing.T) {
	_, e, _ := openTimelineBench(t)
	before := e.spool.Count()
	if _, err := e.Engine.Apply(context.Background(), agent.Operation{ID: "message-one", Kind: agent.Message, Text: "Start here"}); err != nil {
		t.Fatal(err)
	}
	marks := e.spool.Marks()
	if len(marks) != 1 || marks[0] <= before {
		t.Fatalf("marks %v after a message recorded past %d", marks, before)
	}
	r, err := e.spool.Next(context.Background(), marks[0]-1)
	if err != nil || !strings.Contains(string(r.Value), "Start here") {
		t.Fatalf("the mark is not the message: %s %v", r.Value, err)
	}
}

// The tail starts where one of the person's messages does, `turns` back; the
// rest of the conversation is read backward in pages to its first record,
// and the pages together are the timeline WatchEvents replays.
func TestTailStartsAtATurnAndPagesReachTheFirstRecord(t *testing.T) {
	b, e, root := openTimelineBench(t)
	starts := conversation(t, b, e, 6, "An answer.")
	timeline, err := b.OpenTimeline(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	retained := timeline.Retained()
	tail, start, total, err := timeline.Page(0, 2, 1<<20)
	if err != nil || total != retained || start != starts[4] {
		t.Fatalf("tail starts at %d (retained %d), want the 5th turn at %d: %v", start, total, starts[4], err)
	}
	if len(tail) == 0 || tail[0].Sequence != start || tail[len(tail)-1].Sequence != retained || !strings.Contains(string(tail[0].Value), "Question 4") {
		t.Fatalf("tail %v", sequences(tail))
	}
	// Fewer turns than asked: everything, from the first record.
	if _, start, _, err = timeline.Page(0, 50, 1<<20); err != nil || start != 1 {
		t.Fatalf("a short conversation's tail starts at %d: %v", start, err)
	}
	// Backward, a turn at a time, to the first record: contiguous, no record twice.
	var replay []uint64
	for cursor := uint64(0); cursor < retained; cursor++ {
		r, err := timeline.Next(context.Background(), cursor)
		if err != nil {
			t.Fatal(err)
		}
		replay = append(replay, r.Sequence)
	}
	var paged []uint64
	pages := 0
	for before := uint64(0); before != 1; pages++ {
		page, start, _, err := timeline.Page(before, 1, 1<<20)
		if err != nil {
			t.Fatal(err)
		}
		if before > 0 && page[len(page)-1].Sequence != before-1 {
			t.Fatalf("page before %d ends at %d", before, page[len(page)-1].Sequence)
		}
		if start != page[0].Sequence || (start != 1 && !slices.Contains(starts, start)) {
			t.Fatalf("page starts at %d, not a turn start (%v)", start, starts)
		}
		paged = append(sequences(page), paged...)
		before = start
	}
	if !slices.Equal(paged, replay) || pages != len(starts)+1 {
		t.Fatalf("%d pages %v, want the replay %v in %d", pages, paged, replay, len(starts)+1)
	}
	// Past the end is refused, as for WatchEvents.
	if _, _, _, err = timeline.Page(retained+2, 1, 1<<20); err != journal.ErrCursor {
		t.Fatal("a page past the end", err)
	}

	// A restarted Runner rebuilds the index with one scan of the journal: the
	// same turn starts, so the same tail (and the record the shutdown added).
	marks := e.spool.Marks()
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	next, err := New(Config{State: root})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = next.Close() })
	restored, err := next.Get(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(restored.spool.Marks(), marks) {
		t.Fatalf("turn starts after a restart %v, want %v", restored.spool.Marks(), marks)
	}
	reopened, err := next.OpenTimeline(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	again, start, _, err := reopened.Page(0, 2, 1<<20)
	if err != nil || start != starts[4] || !slices.Equal(sequences(again)[:len(tail)], sequences(tail)) {
		t.Fatalf("after a restart the tail is %v, want %v: %v", sequences(again), sequences(tail), err)
	}
}

// A page holds about max bytes of journal: the most whole turns that fit,
// else the latest records of a turn too large on its own, never nothing.
func TestTailIsBoundedBySize(t *testing.T) {
	b, e, _ := openTimelineBench(t)
	starts := conversation(t, b, e, 4, strings.Repeat("a", 2000))
	timeline, err := b.OpenTimeline(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	retained := timeline.Retained()
	turn := e.spool.Span(starts[2]-1, starts[3]-1)
	last := e.spool.Span(starts[3]-1, retained)
	// Room for the last turn and most of the one before: the last turn only.
	_, start, _, err := timeline.Page(0, 3, last+turn/2)
	if err != nil || start != starts[3] {
		t.Fatalf("starts at %d, want the last whole turn at %d: %v", start, starts[3], err)
	}
	// Room for two turns exactly.
	if _, start, _, err = timeline.Page(0, 3, last+turn); err != nil || start != starts[2] {
		t.Fatalf("starts at %d, want %d: %v", start, starts[2], err)
	}
	// Less than the last turn: its latest records.
	page, start, _, err := timeline.Page(0, 3, last/2)
	if err != nil || start <= starts[3] || page[len(page)-1].Sequence != retained || e.spool.Span(start-1, retained) > last/2 {
		t.Fatalf("a large last turn starts at %d: %v", start, err)
	}
	// Not even one record fits: the last one anyway.
	if page, start, _, err = timeline.Page(0, 3, 1); err != nil || len(page) != 1 || start != retained {
		t.Fatalf("an oversized record: %d records from %d: %v", len(page), start, err)
	}
	// Bounded by size alone (turns 0), a page need not start at a turn.
	if page, start, _, err = timeline.Page(0, 0, last/2); err != nil || page[len(page)-1].Sequence != retained || e.spool.Span(start-1, retained) > last/2 {
		t.Fatalf("a size-bounded page: %d records from %d: %v", len(page), start, err)
	}
}

// A continued conversation's timeline numbers its predecessor's records first:
// a page reads across both, and turn starts keep their timeline cursors.
func TestTailReadsAcrossAContinuedConversation(t *testing.T) {
	b, first, _ := openTimelineBench(t)
	conversation(t, b, first, 2, "First attempt.")
	if err := first.Engine.Close(); err != nil {
		t.Fatal(err)
	}
	offset := first.spool.Count()
	second, err := b.Open("codex-default", first.Cwd, "", accounts.Options{}, "direct", first.WorkID, first.ID)
	if err != nil {
		t.Fatal(err)
	}
	starts := conversation(t, b, second, 2, "Second attempt.")
	timeline, err := b.OpenTimeline(second.ID)
	if err != nil {
		t.Fatal(err)
	}
	retained := timeline.Retained()
	if retained != offset+second.spool.Count() {
		t.Fatalf("retained %d", retained)
	}
	// Three turns back: the last of the first attempt, then both of the second.
	page, start, _, err := timeline.Page(0, 3, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	firstStarts := first.spool.Marks()
	if start != firstStarts[len(firstStarts)-1] || page[len(page)-1].Sequence != retained {
		t.Fatalf("starts at %d, want %d", start, firstStarts[len(firstStarts)-1])
	}
	for i, r := range page {
		want, err := timeline.Next(context.Background(), start-1+uint64(i))
		if err != nil || r.Sequence != want.Sequence || string(r.Value) != string(want.Value) {
			t.Fatalf("record %d differs from the replay: %v", r.Sequence, err)
		}
	}
	if !slices.Contains(sequences(page), offset+starts[0]) {
		t.Fatal("the second attempt's turn start is not at its timeline cursor")
	}
}
