package transport

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// A conversation whose journal spans many segments reads the same through
// every reader: WatchEvents replays it whole, ReadSessionTail pages back to
// its first record by turns, and both equal its segment files read in order.
func TestASegmentedConversationReadsAsOne(t *testing.T) {
	state := t.TempDir()
	b, err := runner.New(runner.Config{State: state, EventBudget: 2048})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = b.Close() })
	b.Adapters["codex"] = &agenttest.Adapter{}
	e, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
	if err != nil {
		t.Fatal(err)
	}
	for turn := range 12 {
		if err = b.Record(e, map[string]any{"type": "operation_accepted", "kind": "message", "text": fmt.Sprintf("Question %d", turn)}); err != nil {
			t.Fatal(err)
		}
		for i := range 5 {
			if err = b.Record(e, map[string]any{"type": "message_delta", "text": fmt.Sprintf("answer %d.%d %s", turn, i, strings.Repeat("a", 120))}); err != nil {
				t.Fatal(err)
			}
		}
	}
	files, err := journal.Files(filepath.Join(state, "sessions", e.ID+".jsonl"))
	if err != nil || len(files) < 5 {
		t.Fatalf("%d segments: %v", len(files), err)
	}
	var fromFiles []string
	for _, f := range files {
		data, _ := os.ReadFile(f)
		for _, line := range strings.Split(strings.TrimSuffix(string(data), "\n"), "\n") {
			fromFiles = append(fromFiles, line)
		}
	}
	retained := e.Retained()
	if uint64(len(fromFiles)) != retained {
		t.Fatalf("%d records in the files, %d retained", len(fromFiles), retained)
	}

	service := &Service{Bench: b}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stream := &eventStream{ctx: ctx, sent: make(chan *pb.JournalEvent, 1024)}
	go func() {
		_ = service.WatchEvents(&pb.WatchEventsRequest{SessionId: e.ID, IncludePrevious: true}, stream)
	}()
	replay := make([]*pb.JournalEvent, 0, retained)
	for uint64(len(replay)) < retained {
		select {
		case event := <-stream.sent:
			replay = append(replay, event)
		case <-time.After(5 * time.Second):
			t.Fatalf("replay stopped at %d of %d", len(replay), retained)
		}
	}
	for i, event := range replay {
		if event.Cursor != uint64(i+1) || !strings.Contains(fromFiles[i], fmt.Sprintf(`"sequence":%d,`, i+1)) {
			t.Fatalf("replayed record %d has cursor %d", i+1, event.Cursor)
		}
		if event.Event.Text != "" && !strings.Contains(fromFiles[i], event.Event.Text) {
			t.Fatalf("record %d: replayed %q, the file holds %s", i+1, event.Event.Text, fromFiles[i])
		}
	}

	paged := map[uint64]*pb.JournalEvent{}
	for before, pages := uint64(0), 0; ; pages++ {
		page, err := service.ReadSessionTail(ctx, &pb.ReadSessionTailRequest{SessionId: e.ID, Before: before, Turns: 2})
		if err != nil {
			t.Fatal(err)
		}
		if first := page.Events[0].Event; page.Start != 1 && first.Type != "operation_accepted" {
			t.Fatalf("a page starts with %s, not a turn", first.Type)
		}
		for _, event := range page.Events {
			if paged[event.Cursor] != nil {
				t.Fatalf("record %d paged twice", event.Cursor)
			}
			paged[event.Cursor] = event
		}
		if page.Start == 1 {
			break
		}
		if pages > 20 {
			t.Fatal("paging never reached the first record")
		}
		before = page.Start
	}
	for i, event := range replay {
		if p := paged[event.Cursor]; p == nil || p.Event.Text != event.Event.Text || p.Event.Type != event.Event.Type {
			t.Fatalf("record %d: paged %+v, replayed %+v", i+1, p, event)
		}
	}
}

// A conversation whose journal cannot be read does not take the history page
// with it: the others are listed, and it is listed with the reason.
func TestListHistoryListsTheRestPastABrokenJournal(t *testing.T) {
	state := t.TempDir()
	b, err := runner.New(runner.Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	var ids []string
	for range 3 {
		e, err := b.Open("codex-default", t.TempDir(), "", accounts.Options{}, "direct", "", "")
		if err != nil {
			t.Fatal(err)
		}
		ids = append(ids, e.ID)
	}
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	broken := filepath.Join(state, "sessions", ids[1]+".jsonl")
	if err = os.WriteFile(broken, []byte("not a journal\n"), 0600); err != nil {
		t.Fatal(err)
	}
	b, err = runner.New(runner.Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	page, err := (&Service{Bench: b}).ListHistory(context.Background(), &pb.ListHistoryRequest{})
	if err != nil {
		t.Fatal(err)
	}
	listed := map[string]*pb.Session{}
	for _, s := range page.Sessions {
		listed[s.Id] = s
	}
	for _, id := range ids {
		s := listed[id]
		if s == nil {
			t.Fatalf("%s not listed", id)
		}
		if broke := id == ids[1]; broke != strings.Contains(s.Fault, "unreadable") {
			t.Fatalf("%s: fault %q", id, s.Fault)
		}
	}
}
