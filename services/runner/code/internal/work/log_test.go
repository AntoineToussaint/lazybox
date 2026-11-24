package work

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

// A rewrite whose rename landed has happened even if syncing the directory
// fails: Forget keeps its result in memory, and the next append owes the
// directory sync before acknowledging anything.
func TestForgetLandsWhenOnlyTheDirectorySyncFails(t *testing.T) {
	dir := t.TempDir()
	p, err := OpenPrompts(filepath.Join(dir, "prompts.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = p.Close() })
	for _, conversation := range []string{"gone", "kept"} {
		if err = p.Submit(Human, PromptPayload{Conversation: conversation, Text: "hello from " + conversation}, nil); err != nil {
			t.Fatal(err)
		}
	}
	// Without read permission the directory cannot be opened to sync it, but
	// a rename inside it still succeeds.
	if err = os.Chmod(dir, 0o300); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.Chmod(dir, 0o700) })
	if err = p.Forget("gone"); err != nil {
		t.Fatalf("forget reported failure for a rewrite that landed: %v", err)
	}
	if got := p.History(HistoryQuery{Conversation: "gone"}); len(got) != 0 {
		t.Fatalf("forgotten prompts still recalled: %+v", got)
	}
	if err = p.Submit(Human, PromptPayload{Conversation: "kept", Text: "while the directory cannot sync"}, nil); err == nil {
		t.Fatal("append acknowledged before the rewrite was durable")
	}
	if err = os.Chmod(dir, 0o700); err != nil {
		t.Fatal(err)
	}
	if err = p.Submit(Human, PromptPayload{Conversation: "kept", Text: "after"}, nil); err != nil {
		t.Fatal(err)
	}
	if got := p.History(HistoryQuery{Conversation: "kept"}); len(got) != 2 {
		t.Fatalf("kept history = %+v", got)
	}
}

// When a failed append cannot be cut back off, the bytes it left may hold
// whole lines nobody acknowledged: the log must refuse every later append
// rather than write after them.
func TestLogRefusesWritesAfterAFailedWriteCannotBeUndone(t *testing.T) {
	path := filepath.Join(t.TempDir(), "log.jsonl")
	l, _, err := openFile(path, 0)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = l.close() })
	event := Event{ID: newID(time.Now()), Seq: 1, Subject: "s", Type: PromptSubmitted}
	if err = l.append([]Event{event}); err != nil {
		t.Fatal(err)
	}
	writable := l.f
	// A read-only handle fails both the write and the truncate that would
	// undo it.
	readOnly, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	l.f = readOnly
	event.Seq = 2
	if err = l.append([]Event{event}); err == nil {
		t.Fatal("append through a read-only handle succeeded")
	}
	_ = readOnly.Close()
	l.f = writable
	before, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	event.Seq = 3
	if err = l.append([]Event{event}); err == nil {
		t.Fatal("a log that could not be repaired accepted another append")
	}
	after, err := os.ReadFile(path)
	if err != nil || string(after) != string(before) {
		t.Fatalf("log changed after it was marked broken: %q, %v", after, err)
	}
	// A rewrite replaces every byte, so the log is usable again.
	if err = l.rewrite(path, []Event{event}); err != nil {
		t.Fatal(err)
	}
	event.Seq = 4
	if err = l.append([]Event{event}); err != nil {
		t.Fatal(err)
	}
}
