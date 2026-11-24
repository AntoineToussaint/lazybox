package work

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

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
