package runner

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/claude"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

// padding is a record of about 300 bytes that no reader acts on.
var padding = map[string]any{"type": "padding", "text": strings.Repeat("p", 240)}

func segmentedFixtureBench(t *testing.T, state string) *Bench {
	t.Helper()
	b, err := New(Config{State: state, Worker: "fixture-claude-worker", EventBudget: 2048})
	if err != nil {
		t.Fatal(err)
	}
	self, _ := os.Executable()
	b.Adapters["claude"] = claude.Adapter{Node: self, Worker: "fixture-claude-worker"}
	return b
}

// contiguous checks the conversation reads as records 1…n, one each, and as
// its segment files read in order.
func contiguous(t *testing.T, b *Bench, id string) uint64 {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	timeline, err := b.OpenTimeline(id)
	if err != nil {
		t.Fatal(err)
	}
	n := timeline.Retained()
	files, err := journal.Files(b.journalPath(id))
	if err != nil {
		t.Fatal(err)
	}
	var lines int
	for _, f := range files {
		data, _ := os.ReadFile(f)
		lines += strings.Count(string(data), "\n")
	}
	if uint64(lines) != n {
		t.Fatalf("%d records in %d files, %d in the timeline", lines, len(files), n)
	}
	for cursor := uint64(0); cursor < n; cursor++ {
		r, err := timeline.Next(ctx, cursor)
		if err != nil || r.Sequence != cursor+1 {
			t.Fatalf("record %d: %+v %v", cursor+1, r, err)
		}
	}
	return n
}

// A live conversation whose journal spans several segments is re-attached
// by the next Runner: the same agent finishes its turn, and the journal goes
// on as one, across the restart and the segments.
func TestAdoptionAcrossARollover(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	first := segmentedFixtureBench(t, state)
	e, err := openAwaitingApproval(first)
	if err != nil {
		t.Fatal(err)
	}
	pid := workerPID(t, recorded(t, first, e.ID))
	for range 20 {
		if err = first.Record(e, padding); err != nil {
			t.Fatal(err)
		}
	}
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	files, _ := journal.Files(first.journalPath(e.ID))
	if len(files) < 3 {
		t.Fatalf("the journal spans %d segments before the restart", len(files))
	}

	second := segmentedFixtureBench(t, state)
	if err = second.Adopt(context.Background()); err != nil {
		t.Fatal(err)
	}
	finishAfterAdoption(t, second, e.ID, pid)
	contiguous(t, second, e.ID)
	if fault := second.items[e.ID].Engine.Snapshot().Fault; fault != "" {
		t.Fatalf("fault after adoption: %q", fault)
	}
	if err = second.Close(); err != nil {
		t.Fatal(err)
	}
	stoppedEventually(t, pid)
}

// A journal from before segments (one file, past today's segment budget) is
// read whole as history and continued in a second segment when a live
// conversation goes on.
func TestSingleFileJournalFromBeforeSegments(t *testing.T) {
	state := t.TempDir()
	before, err := New(Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	before.Adapters["codex"] = &agenttest.Adapter{}
	e, err := before.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	for range 30 {
		if err = before.Record(e, padding); err != nil {
			t.Fatal(err)
		}
	}
	if err = before.Close(); err != nil {
		t.Fatal(err)
	}
	if files, _ := journal.Files(before.journalPath(e.ID)); len(files) != 1 {
		t.Fatalf("%d files under the 8 MiB default", len(files))
	}

	b, err := New(Config{State: state, EventBudget: 2048})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	old, err := b.Get(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	if fault := old.Engine.Snapshot().Fault; fault != "" || !old.Archived {
		t.Fatalf("history of an old journal: archived %v, fault %q", old.Archived, fault)
	}
	if n := contiguous(t, b, e.ID); n < 30 {
		t.Fatalf("%d records", n)
	}
	// The same file opened for writing continues past its old size.
	spool, err := b.openJournal(e.ID)
	if err != nil {
		t.Fatal(err)
	}
	defer spool.Dispose()
	if _, err = spool.Append(padding); err != nil || spool.Segments() != 2 {
		t.Fatalf("%v, %d segments", err, spool.Segments())
	}
}

// A journal that cannot be read in full, or at all, leaves its conversation
// readable up to the damage, listed with the reason, and deletable.
func TestABrokenJournalIsReadableToItsDamageAndDeletable(t *testing.T) {
	ctx := context.Background()
	cfg := Config{State: t.TempDir(), EventBudget: 2048}
	b, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	b.Adapters["codex"] = &agenttest.Adapter{}
	var ids []string
	for range 3 {
		e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
		if err != nil {
			t.Fatal(err)
		}
		for range 20 {
			if err = b.Record(e, padding); err != nil {
				t.Fatal(err)
			}
		}
		ids = append(ids, e.ID)
	}
	if err = b.Close(); err != nil {
		t.Fatal(err)
	}
	counts := map[string]uint64{}
	for _, id := range ids {
		spool, err := b.readJournal(id)
		if err != nil {
			t.Fatal(err)
		}
		counts[id] = spool.Count()
		_ = spool.Dispose()
	}
	torn, corrupt, gone := ids[0], ids[1], ids[2]
	// torn: the disk filled part way through the last record.
	tornFiles, _ := journal.Files(b.journalPath(torn))
	f, err := os.OpenFile(tornFiles[len(tornFiles)-1], os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		t.Fatal(err)
	}
	_, _ = f.WriteString(`{"sequence":99,"ti`)
	f.Close()
	// corrupt: a record in the first segment no longer reads.
	first := b.journalPath(corrupt)
	data, _ := os.ReadFile(first)
	lines := strings.SplitAfter(string(data), "\n")
	lines[2] = "\x00\x00\x00\n"
	if err = os.WriteFile(first, []byte(strings.Join(lines, "")), 0600); err != nil {
		t.Fatal(err)
	}
	// gone: its journal was removed outside Lazybox.
	if err = journal.Remove(b.journalPath(gone)); err != nil {
		t.Fatal(err)
	}

	b, err = New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	want := map[string]struct {
		retained uint64
		fault    string
	}{torn: {counts[torn], ""}, corrupt: {2, "transcript unreadable past cursor 2"}, gone: {0, "transcript unreadable past cursor 0"}}
	for id, w := range want {
		e, err := b.Get(id)
		if err != nil {
			t.Fatalf("%s: %v", id, err)
		}
		if got := e.Retained(); got != w.retained {
			t.Fatalf("%s: %d records readable, want %d", id, got, w.retained)
		}
		if fault := e.Engine.Snapshot().Fault; !strings.HasPrefix(fault, w.fault) || (w.fault == "") != (fault == "") {
			t.Fatalf("%s: fault %q, want %q", id, fault, w.fault)
		}
	}
	for _, id := range ids {
		if err = b.DeleteConversation(ctx, id, false); err != nil {
			t.Fatalf("delete %s: %v", id, err)
		}
		entries, _ := os.ReadDir(filepath.Join(cfg.State, "sessions"))
		for _, entry := range entries {
			if strings.HasPrefix(entry.Name(), id+".jsonl") {
				t.Fatalf("%s left behind", entry.Name())
			}
		}
	}
}
