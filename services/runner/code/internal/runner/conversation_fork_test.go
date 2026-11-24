package runner

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/codecheckpoints"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
)

func TestForkBoundaryExcludesLaterAndQueuedPrompts(t *testing.T) {
	spool, err := journal.Open(filepath.Join(t.TempDir(), "events"), 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { spool.Dispose() })
	prompt := func(id, text string) agent.Event {
		return agent.Event{Type: "operation_accepted", OperationID: id, Text: text}
	}
	reply := func(turn, text string) agent.Event {
		return agent.Event{Type: "message_delta", TurnID: turn, Text: text}
	}
	end := func(turn, operation string) agent.Event {
		return agent.Event{Type: "turn_ended", SessionID: "thread", TurnID: turn, OperationID: operation, Status: "completed"}
	}
	for _, event := range []agent.Event{
		prompt("first", "first prompt"),
		{Type: "turn_started", TurnID: "one"},
		prompt("queued", "queued while one was running"),
		reply("one", "first reply"),
		end("one", "first"),
		{Type: "turn_started", TurnID: "two"},
		reply("two", "second reply"),
		end("two", "queued"),
		prompt("third", "later prompt"),
	} {
		if _, err := spool.Append(event); err != nil {
			t.Fatal(err)
		}
	}
	timeline := &Timeline{chain: []*Execution{{spool: spool, Engine: &archivedRuntime{spool: spool}}}}
	after, err := readForkBoundary(context.Background(), timeline, "thread", "one", 0)
	if err != nil {
		t.Fatal(err)
	}
	if after.turn != "one" || after.operation != "first" || len(after.history) != 3 ||
		after.history[0].Text != "first prompt" || after.history[1].Text != "first reply" {
		t.Fatalf("unexpected fork: %+v", after)
	}
	retry, err := readForkBoundary(context.Background(), timeline, "thread", "", 9)
	if err != nil {
		t.Fatal(err)
	}
	if retry.turn != "two" || retry.operation != "third" || len(retry.history) != 6 ||
		retry.history[3].Text != "queued while one was running" || retry.history[4].Text != "second reply" {
		t.Fatalf("unexpected retry: %+v", retry)
	}
	first, err := readForkBoundary(context.Background(), timeline, "thread", "", 1)
	if err != nil || first.thread != "" || len(first.history) != 0 || first.operation != "first" {
		t.Fatalf("unexpected first retry: %+v, %v", first, err)
	}
	if _, err := readForkBoundary(context.Background(), timeline, "thread", "", 3); err == nil {
		t.Fatal("accepted an in-turn prompt as an edit boundary")
	}
	if _, err := readForkBoundary(context.Background(), timeline, "different", "one", 0); err == nil {
		t.Fatal("accepted a turn from a different native thread")
	}
}

func TestForkCheckpointWorkspaceLeavesSourceFilesAlone(t *testing.T) {
	repo := runtimeRepo(t, "")
	b := runtimeBench(t, t.TempDir(), t.TempDir(), collections.Repository{ID: "api", Path: repo})
	t.Cleanup(func() { b.Close() })
	ctx := context.Background()
	source, err := b.Collections.Create(ctx, "source", collections.Spec{
		Name: "Source", Members: []collections.MemberSpec{{Name: "api", RepositoryID: "api", BaseRef: "refs/heads/main"}},
	})
	if err != nil {
		t.Fatal(err)
	}
	path := source.Members[0].Path
	repositories := []codecheckpoints.Repository{{Member: "api", Path: path}}
	write := func(value string) {
		t.Helper()
		if err := os.WriteFile(filepath.Join(path, "notes.txt"), []byte(value), 0600); err != nil {
			t.Fatal(err)
		}
	}
	write("before")
	if _, err := b.Checkpoints.BeginTurn(ctx, "source-chat", repositories, "prompt", "Make a change"); err != nil {
		t.Fatal(err)
	}
	write("after")
	if err := b.Checkpoints.EndTurn(ctx, "source-chat", repositories, "prompt", time.Now()); err != nil {
		t.Fatal(err)
	}
	write("present")
	execution := &Execution{ID: "source-chat", CollectionID: source.ID, Purpose: "coding"}
	// The bench's own goroutines read items (the power poll lists them every
	// second), so the test registers its execution under the bench's lock.
	b.mu.Lock()
	b.items[execution.ID] = execution
	b.mu.Unlock()
	t.Cleanup(func() {
		b.mu.Lock()
		defer b.mu.Unlock()
		delete(b.items, execution.ID)
	})
	for _, test := range []struct {
		key    string
		before bool
	}{{"before", true}, {"after", false}} {
		id, err := b.forkCheckpointWorkspace(ctx, execution, test.key, "prompt", test.before)
		if err != nil {
			t.Fatal(err)
		}
		child, err := b.Collections.Get(id)
		if err != nil {
			t.Fatal(err)
		}
		data, err := os.ReadFile(filepath.Join(child.Members[0].Path, "notes.txt"))
		if err != nil || string(data) != test.key {
			t.Fatalf("checkpoint file = %q, %v, want %q", data, err, test.key)
		}
		repeated, err := b.forkCheckpointWorkspace(ctx, execution, test.key, "prompt", test.before)
		if err != nil || repeated != id {
			t.Fatalf("retry created %q, %v; want %q", repeated, err, id)
		}
	}
	if current, err := os.ReadFile(filepath.Join(path, "notes.txt")); err != nil || string(current) != "present" {
		t.Fatalf("source file = %q, %v", current, err)
	}
}
