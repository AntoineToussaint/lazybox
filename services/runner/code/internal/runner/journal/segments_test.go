package journal

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

// fill appends n records of about 200 bytes, every fifth a turn start.
func fill(t *testing.T, j *Journal, from, n int) {
	t.Helper()
	for i := from; i < from+n; i++ {
		text := fmt.Sprintf("reply %d %s", i, strings.Repeat("x", 160))
		if i%5 == 0 {
			text = fmt.Sprintf("turn %d %s", i, strings.Repeat("x", 160))
		}
		if _, err := j.Append(text); err != nil {
			t.Fatalf("append %d: %v", i, err)
		}
	}
}

// fileRecords reads the records of the segment files, in order, as plain
// JSON lines: what a journal spanning them must read as one.
func fileRecords(t *testing.T, path string) (records []Record, bytes int64) {
	t.Helper()
	files, err := Files(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, name := range files {
		f, err := os.Open(name)
		if err != nil {
			t.Fatal(err)
		}
		scanner := bufio.NewScanner(f)
		for scanner.Scan() {
			var r Record
			if err = json.Unmarshal(scanner.Bytes(), &r); err != nil {
				t.Fatal(err)
			}
			records = append(records, r)
			bytes += int64(len(scanner.Bytes()) + 1)
		}
		f.Close()
	}
	return records, bytes
}

func sequences(records []Record) []uint64 {
	out := make([]uint64, len(records))
	for i, r := range records {
		out[i] = r.Sequence
	}
	return out
}

// A journal that reaches its segment budget continues in the next segment;
// replay, ranges, positions and turn marks read the chain as one journal,
// equal to its segment files concatenated, before and after it reopens.
func TestRolloverReadsAcrossSegmentsAsOneJournal(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	j, err := Open(path, 2048, turn, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	fill(t, j, 0, 60)
	if j.Segments() < 4 {
		t.Fatalf("%d segments for 60 records of a 2 KiB budget", j.Segments())
	}
	for _, name := range mustFiles(t, path) {
		if info, _ := os.Stat(name); info.Size() > 2048 {
			t.Fatalf("%s is %d bytes, over its budget", name, info.Size())
		}
	}
	check := func(j *Journal) {
		t.Helper()
		want, size := fileRecords(t, path)
		if uint64(len(want)) != j.Count() || len(want) != 60 {
			t.Fatalf("%d records in the files, %d in the journal", len(want), j.Count())
		}
		var replayed []Record
		for cursor := uint64(0); cursor < j.Count(); cursor++ {
			r, err := j.Next(context.Background(), cursor)
			if err != nil {
				t.Fatal(err)
			}
			replayed = append(replayed, r)
		}
		all, err := j.Range(0, j.Count())
		if err != nil {
			t.Fatal(err)
		}
		for i := range want {
			if string(replayed[i].Value) != string(want[i].Value) || string(all[i].Value) != string(want[i].Value) || replayed[i].Sequence != uint64(i+1) {
				t.Fatalf("record %d: replay %s, range %s, files %s", i+1, replayed[i].Value, all[i].Value, want[i].Value)
			}
		}
		// Every range, including those across a segment boundary.
		for after := uint64(0); after < 60; after += 7 {
			for upTo := after; upTo <= 60; upTo += 11 {
				part, err := j.Range(after, upTo)
				if err != nil {
					t.Fatal(err)
				}
				if !slices.Equal(sequences(part), sequences(want[after:upTo])) {
					t.Fatalf("range (%d, %d]: %v", after, upTo, sequences(part))
				}
			}
		}
		if j.Span(0, 60) != size {
			t.Fatalf("span %d, files hold %d bytes", j.Span(0, 60), size)
		}
		// Fit agrees with the byte positions it is computed from.
		for _, budget := range []int64{0, 100, 1000, 3000, size} {
			after := j.Fit(60, budget)
			if j.Span(after, 60) > budget && after != 60 {
				t.Fatalf("fit(%d) = %d spans %d", budget, after, j.Span(after, 60))
			}
			if after > 0 && j.Span(after-1, 60) <= budget {
				t.Fatalf("fit(%d) = %d is not the earliest", budget, after)
			}
		}
		var marks []uint64
		for _, r := range want {
			if strings.Contains(string(r.Value), "turn") {
				marks = append(marks, r.Sequence)
			}
		}
		if !slices.Equal(j.Marks(), marks) {
			t.Fatalf("marks %v, want %v", j.Marks(), marks)
		}
	}
	check(j)
	segments := j.Segments()
	_ = j.Dispose()

	j, err = Open(path, 2048, turn, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	if j.Segments() != segments {
		t.Fatalf("reopened with %d segments, wrote %d", j.Segments(), segments)
	}
	check(j)
	// Records go on in the last segment, numbered after the chain.
	r, err := j.Append("after reopen")
	if err != nil || r.Sequence != 61 {
		t.Fatalf("append after reopen: %+v %v", r, err)
	}
	_ = j.Dispose()

	read, err := OpenExisting(path, 2048, turn, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	defer read.Dispose()
	if read.Count() != 61 || read.Damage() != nil {
		t.Fatalf("read-only: %d records, damage %v", read.Count(), read.Damage())
	}
}

// A reader following the journal live crosses into a new segment without
// noticing it.
func TestLiveReaderFollowsARollover(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	j, err := Open(path, 1024, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	got := make(chan uint64, 64)
	go func() {
		for cursor := uint64(0); ; cursor++ {
			r, err := j.Next(context.Background(), cursor)
			if err != nil {
				close(got)
				return
			}
			got <- r.Sequence
		}
	}()
	fill(t, j, 0, 20)
	_ = j.Close()
	var seen []uint64
	for s := range got {
		seen = append(seen, s)
	}
	if len(seen) != 20 || seen[19] != 20 || j.Segments() < 2 {
		t.Fatalf("followed %v across %d segments", seen, j.Segments())
	}
}

// Without Rollover a journal is one file that is full at its budget, as
// before (command output relies on it).
func TestWithoutRolloverAJournalIsFullAtItsBudget(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	j, err := Open(path, 2048)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	var err2 error
	for i := 0; i < 30 && err2 == nil; i++ {
		_, err2 = j.Append(strings.Repeat("x", 160))
	}
	if !errors.Is(err2, ErrFull) || j.Segments() != 1 {
		t.Fatalf("%v with %d segments", err2, j.Segments())
	}
}

// The total budget bounds every segment together. A refused record changes
// nothing, and what is recorded stays readable.
func TestTotalBudgetBoundsTheChain(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	j, err := Open(path, 1024, Rollover(4096))
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	var full error
	for i := 0; i < 100 && full == nil; i++ {
		_, full = j.Append(strings.Repeat("x", 160))
	}
	if !errors.Is(full, ErrFull) {
		t.Fatal(full)
	}
	count := j.Count()
	if _, err = j.Append(strings.Repeat("y", 160)); !errors.Is(err, ErrFull) || j.Count() != count {
		t.Fatalf("%v, %d records", err, j.Count())
	}
	if _, size := fileRecords(t, path); size > 4096 {
		t.Fatalf("%d bytes recorded past a 4 KiB total", size)
	}
	if _, err = j.Range(0, count); err != nil {
		t.Fatal(err)
	}
	if _, err = Open(path+"2", 4096, Rollover(1024)); err == nil {
		t.Fatal("a total budget below the segment budget")
	}
	// One record larger than a segment's budget has a segment of its own:
	// an agent's oversized event is recorded, not refused for good.
	big, err := Open(path+"3", 1024, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	defer big.Dispose()
	fill(t, big, 1, 2)
	if _, err = big.Append(strings.Repeat("z", 2000)); err != nil || big.Segments() != 2 {
		t.Fatalf("%v, %d segments", err, big.Segments())
	}
	if _, err = big.Append("after"); err != nil || big.Segments() != 3 {
		t.Fatalf("%v, %d segments", err, big.Segments())
	}
	if got := replay(t, big); len(got) != 4 || len(got[2]) != 2000 || got[3] != "after" {
		t.Fatalf("replayed %d records", len(got))
	}
}

// A journal written before segments existed is segment 0: it opens with
// Rollover, even past the segment budget, and continues in segment 1.
func TestSingleFileJournalIsSegmentZero(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	old, err := Open(path, 1<<20, turn)
	if err != nil {
		t.Fatal(err)
	}
	fill(t, old, 0, 30)
	_ = old.Dispose()
	before, _ := os.ReadFile(path)

	j, err := Open(path, 1024, turn, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	if j.Count() != 30 || j.Segments() != 1 || len(j.Marks()) != 6 {
		t.Fatalf("%d records in %d segments, marks %v", j.Count(), j.Segments(), j.Marks())
	}
	fill(t, j, 30, 3)
	if j.Segments() != 2 {
		t.Fatalf("%d segments after the old file", j.Segments())
	}
	if now, _ := os.ReadFile(path); string(now) != string(before) {
		t.Fatal("the old journal changed")
	}
	if _, err = os.Stat(path + ".1"); err != nil {
		t.Fatal(err)
	}
	got := replay(t, j)
	_ = j.Dispose()
	if len(got) != 33 || !strings.HasPrefix(got[0], "turn 0") || !strings.HasPrefix(got[32], "reply 32") {
		t.Fatalf("replayed %d: %q … %q", len(got), got[0], got[len(got)-1])
	}
}

// A torn record at the end of the last segment is recovered like one in a
// single file.
func TestTornTailOfTheLastSegmentIsRecovered(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	j, err := Open(path, 1024, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	fill(t, j, 0, 12)
	count, segments := j.Count(), j.Segments()
	_ = j.Dispose()
	last := segmentPath(path, segments-1)
	f, err := os.OpenFile(last, os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		t.Fatal(err)
	}
	_, _ = f.WriteString(`{"sequence":`)
	f.Close()
	j, err = Open(path, 1024, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	if j.Count() != count {
		t.Fatalf("%d records, want %d", j.Count(), count)
	}
	if _, err = os.Stat(last + ".torn"); err != nil {
		t.Fatal(err)
	}
	if r, err := j.Append("next"); err != nil || r.Sequence != count+1 {
		t.Fatalf("%+v %v", r, err)
	}
}

// Remove deletes every segment and torn record of one journal, and nothing
// of another whose name starts the same.
func TestRemoveDeletesEverySegment(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "a.jsonl")
	j, err := Open(path, 1024, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	fill(t, j, 0, 12)
	_ = j.Dispose()
	keep := []string{"a.attach.json", "a.jsonlx", "a.jsonl.attach", "ab.jsonl", "ab.jsonl.1"}
	for _, name := range append([]string{"a.jsonl.torn", "a.jsonl.1.torn"}, keep...) {
		if err = os.WriteFile(filepath.Join(dir, name), []byte("x"), 0600); err != nil {
			t.Fatal(err)
		}
	}
	if err = Remove(path); err != nil {
		t.Fatal(err)
	}
	entries, _ := os.ReadDir(dir)
	var left []string
	for _, e := range entries {
		left = append(left, e.Name())
	}
	slices.Sort(keep)
	if !slices.Equal(left, keep) {
		t.Fatalf("left %v, want %v", left, keep)
	}
	if err = Remove(path); err != nil {
		t.Fatalf("removing a removed journal: %v", err)
	}
}

// Tail reads the end of the chain, across segments, and skips a record still
// being written.
func TestTailReadsAcrossSegments(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events.jsonl")
	j, err := Open(path, 1024, Rollover(0))
	if err != nil {
		t.Fatal(err)
	}
	fill(t, j, 0, 20)
	segments := j.Segments()
	_ = j.Dispose()
	f, _ := os.OpenFile(segmentPath(path, segments-1), os.O_APPEND|os.O_WRONLY, 0600)
	_, _ = f.WriteString(`{"sequence":21,`)
	f.Close()
	records, err := Tail(path, 8, 1<<20)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(sequences(records), []uint64{13, 14, 15, 16, 17, 18, 19, 20}) {
		t.Fatalf("tail %v", sequences(records))
	}
	records, err = Tail(path, 100, 1<<20)
	if err != nil || len(records) != 20 || records[0].Sequence != 1 {
		t.Fatalf("whole tail: %v %v", sequences(records), err)
	}
	if _, err = Tail(filepath.Join(t.TempDir(), "none.jsonl"), 8, 1<<20); !errors.Is(err, os.ErrNotExist) {
		t.Fatal(err)
	}
}

func mustFiles(t *testing.T, path string) []string {
	t.Helper()
	files, err := Files(path)
	if err != nil {
		t.Fatal(err)
	}
	return files
}
