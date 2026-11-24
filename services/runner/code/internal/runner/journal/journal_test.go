package journal

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func TestDurableReplayAndIndependentReaders(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events")
	j, err := Open(path, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	for _, text := range []string{"first", "second"} {
		if _, err = j.Append(text); err != nil {
			t.Fatal(err)
		}
	}
	_ = j.Close()
	for viewer := 0; viewer < 2; viewer++ {
		for cursor := uint64(0); cursor < 2; cursor++ {
			r, err := j.Next(context.Background(), cursor)
			if err != nil || r.Sequence != cursor+1 {
				t.Fatalf("viewer %d: %+v %v", viewer, r, err)
			}
		}
	}
	if _, err = j.Next(context.Background(), 2); err != io.EOF {
		t.Fatal(err)
	}
	_ = j.Dispose()
	j, err = Open(path, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	if j.Count() != 2 {
		t.Fatal(j.Count())
	}
	if _, err = j.Append("third"); err != nil {
		t.Fatal(err)
	}
	if _, err = j.Next(context.Background(), 4); !errors.Is(err, ErrCursor) {
		t.Fatal(err)
	}
}

func TestBoundedSpoolAndCanceledViewer(t *testing.T) {
	j, err := Open(filepath.Join(t.TempDir(), "events"), 1024)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err = j.Next(ctx, 0); !errors.Is(err, context.Canceled) {
		t.Fatal(err)
	}
	if _, err = j.Append(strings.Repeat("x", 2000)); !errors.Is(err, ErrFull) {
		t.Fatal(err)
	}
	if j.Count() != 0 {
		t.Fatal("full append changed cursor")
	}
	if _, err = j.Append("control remains available"); err != nil {
		t.Fatal(err)
	}
}

// A record whose write never finished was never acknowledged: opening the
// journal for writing removes it (keeping its bytes in .torn), and records
// continue where it started. Read-only, the journal reads up to it. It is
// never replayed as complete.
func TestTornTailIsRecoveredAtOpen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events")
	j, err := Open(path, 4096)
	if err != nil {
		t.Fatal(err)
	}
	for _, text := range []string{"first", "second"} {
		if _, err = j.Append(text); err != nil {
			t.Fatal(err)
		}
	}
	_ = j.Dispose()
	whole, _ := os.ReadFile(path)
	torn := []byte(`{"sequence":3,"time":"2026-`)
	if err = os.WriteFile(path, append(append([]byte(nil), whole...), torn...), 0600); err != nil {
		t.Fatal(err)
	}

	read, err := OpenExisting(path, 4096)
	if err != nil {
		t.Fatal(err)
	}
	if read.Count() != 2 || read.Damage() != nil {
		t.Fatalf("read-only open: %d records, damage %v", read.Count(), read.Damage())
	}
	_ = read.Dispose()
	if _, err = os.Stat(path + ".torn"); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("a read-only open changed the journal: %v", err)
	}

	j, err = Open(path, 4096)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	if j.Count() != 2 {
		t.Fatalf("recovered %d records, want 2", j.Count())
	}
	if kept, _ := os.ReadFile(path + ".torn"); string(kept) != string(torn) {
		t.Fatalf("torn bytes kept as %q", kept)
	}
	if now, _ := os.ReadFile(path); string(now) != string(whole) {
		t.Fatal("the torn record was not truncated")
	}
	r, err := j.Append("third")
	if err != nil || r.Sequence != 3 {
		t.Fatalf("append after recovery: %+v %v", r, err)
	}
	if got := replay(t, j); !slices.Equal(got, []string{"first", "second", "third"}) {
		t.Fatal(got)
	}
}

// A journal that is nothing but a torn record opens empty.
func TestJournalOfOnlyATornRecordOpensEmpty(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events")
	if err := os.WriteFile(path, []byte(`{"sequence":1`), 0600); err != nil {
		t.Fatal(err)
	}
	j, err := Open(path, 4096)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	if j.Count() != 0 {
		t.Fatal("replayed a torn record as complete")
	}
}

// A record that is complete but unreadable is damage, not a torn write: a
// writer refuses it (nothing may follow it), a reader refuses it too unless
// it salvages what is before it.
func TestCorruptRecordStopsAReaderAndRefusesAWriter(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events")
	j, err := Open(path, 4096)
	if err != nil {
		t.Fatal(err)
	}
	for _, text := range []string{"first", "second", "third"} {
		if _, err = j.Append(text); err != nil {
			t.Fatal(err)
		}
	}
	_ = j.Dispose()
	data, _ := os.ReadFile(path)
	lines := strings.SplitAfter(string(data), "\n")
	if err = os.WriteFile(path, []byte(lines[0]+"not json\n"+lines[2]), 0600); err != nil {
		t.Fatal(err)
	}
	if j, err := Open(path, 4096); err == nil {
		j.Dispose()
		t.Fatal("appending after a corrupt record")
	}
	if j, err := OpenExisting(path, 4096); err == nil {
		j.Dispose()
		t.Fatal("read a corrupt journal without Salvage")
	}
	read, err := OpenExisting(path, 4096, Salvage())
	if err != nil {
		t.Fatal(err)
	}
	defer read.Dispose()
	if read.Count() != 1 || read.Damage() == nil {
		t.Fatalf("read %d records, damage %v", read.Count(), read.Damage())
	}
}

// A write that fails part way leaves nothing of its record: the journal is
// truncated back, readers behind and at the tail carry on, and the next
// record takes the failed one's sequence.
func TestShortWriteIsTruncatedBackAndReadersContinue(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events")
	j, err := Open(path, 1<<16)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	for _, text := range []string{"first", "second"} {
		if _, err = j.Append(text); err != nil {
			t.Fatal(err)
		}
	}
	before, _ := os.Stat(path)
	writeAt = func(f *os.File, b []byte, at int64) (int, error) {
		n, _ := f.WriteAt(b[:len(b)/2], at)
		return n, errors.New("no space left on device")
	}
	_, err = j.Append("lost")
	writeAt = (*os.File).WriteAt
	if err == nil {
		t.Fatal("a failed write reported success")
	}
	if after, _ := os.Stat(path); after.Size() != before.Size() {
		t.Fatalf("journal is %d bytes after a failed write, want %d", after.Size(), before.Size())
	}
	// Behind the tail: the descriptor is still open.
	if r, err := j.Next(context.Background(), 0); err != nil || r.Sequence != 1 {
		t.Fatalf("reader behind the tail: %+v %v", r, err)
	}
	// At the tail: still waiting, and woken by the next record.
	got := make(chan Record, 1)
	go func() {
		r, _ := j.Next(context.Background(), 2)
		got <- r
	}()
	r, err := j.Append("third")
	if err != nil || r.Sequence != 3 {
		t.Fatalf("append after a failed write: %+v %v", r, err)
	}
	if r := <-got; r.Sequence != 3 {
		t.Fatalf("reader at the tail got %+v", r)
	}
	_ = j.Close()
	again, err := Open(path, 1<<16)
	if err != nil || again.Count() != 3 {
		t.Fatalf("reopen: %v", err)
	}
	again.Dispose()
	if _, err = os.Stat(path + ".torn"); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("a truncated-back write left a torn record")
	}
}

func replay(t *testing.T, j *Journal) []string {
	t.Helper()
	var out []string
	for cursor := uint64(0); cursor < j.Count(); cursor++ {
		r, err := j.Next(context.Background(), cursor)
		if err != nil {
			t.Fatal(err)
		}
		var text string
		if err = json.Unmarshal(r.Value, &text); err != nil {
			t.Fatal(err)
		}
		out = append(out, text)
	}
	return out
}

// A reserve keeps the end of the budget for the records that close a journal:
// ordinary appends stop short of it, reserved ones may use it, and reopening
// with the same reserve reads what is there.
func TestReserveKeepsRoomForTheLastRecords(t *testing.T) {
	path := filepath.Join(t.TempDir(), "command")
	const limit, reserve = 4096, 1024
	j, err := Open(path, limit, Reserve(reserve))
	if err != nil {
		t.Fatal(err)
	}
	filler := strings.Repeat("x", 200)
	for {
		if _, err = j.Append(filler); err != nil {
			break
		}
	}
	info, statErr := os.Stat(path)
	if statErr != nil || !errors.Is(err, ErrFull) || info.Size() > limit-reserve {
		t.Fatalf("ordinary appends reached into the reserve: %v %v %v", info, statErr, err)
	}
	if _, err = j.AppendReserved(strings.Repeat("y", 700)); err != nil {
		t.Fatalf("the reserve did not take a closing record: %v", err)
	}
	if _, err = j.AppendReserved(strings.Repeat("z", 1024)); !errors.Is(err, ErrFull) {
		t.Fatalf("a reserved append went past the budget: %v", err)
	}
	_ = j.Close()
	if j, err = Open(path, limit, Reserve(reserve)); err != nil {
		t.Fatalf("a journal whose reserve is used does not reopen: %v", err)
	}
	_ = j.Close()
	if _, err = Open(path, limit, Reserve(limit)); err == nil {
		t.Fatal("a reserve as large as the budget was accepted")
	}
}
