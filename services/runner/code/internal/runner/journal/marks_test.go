package journal

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

var turn = Marking(func(value json.RawMessage) bool { return strings.Contains(string(value), "turn") })

// Records Marking selects are indexed as they are appended and again, the
// same, when the journal is read at open: a reader finds a conversation's turn
// starts without parsing the file.
func TestMarksAreIndexedOnAppendAndRebuiltOnOpen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events")
	j, err := Open(path, 1<<16, turn)
	if err != nil {
		t.Fatal(err)
	}
	for _, text := range []string{"turn 1", "reply", "reply", "turn 2", "reply", "turn 3"} {
		if _, err = j.Append(text); err != nil {
			t.Fatal(err)
		}
	}
	want := []uint64{1, 4, 6}
	if got := j.Marks(); !slices.Equal(got, want) {
		t.Fatalf("marks while appending %v, want %v", got, want)
	}
	// An append the journal refuses is not indexed.
	if _, err = j.Append(strings.Repeat("turn", 1<<16)); !errors.Is(err, ErrFull) {
		t.Fatal(err)
	}
	if got := j.Marks(); !slices.Equal(got, want) {
		t.Fatalf("a refused append changed the marks: %v", got)
	}
	_ = j.Dispose()
	for _, open := range []func(string, int64, ...Option) (*Journal, error){Open, OpenExisting} {
		if j, err = open(path, 1<<16, turn); err != nil {
			t.Fatal(err)
		}
		if got := j.Marks(); !slices.Equal(got, want) {
			t.Fatalf("marks rebuilt at open %v, want %v", got, want)
		}
		_ = j.Dispose()
	}
	// Without Marking, nothing is indexed.
	if j, err = Open(path, 1<<16); err != nil || len(j.Marks()) != 0 {
		t.Fatal("an unmarked journal has marks", err)
	}
	_ = j.Dispose()
}

// No index is built from a line that is not a record. A journal whose last
// line was torn (a crash mid-append) opens without it; one with a line that
// is not a record is refused for writing and salvaged up to it.
func TestMarkedJournalIndexesNoTornOrCorruptLine(t *testing.T) {
	for name, tail := range map[string]string{"torn last line": `{"sequence":3,"value":"tu`, "corrupt line": "not a record\n"} {
		path := filepath.Join(t.TempDir(), "events")
		j, err := Open(path, 1<<16, turn)
		if err != nil {
			t.Fatal(err)
		}
		for _, text := range []string{"turn 1", "reply"} {
			if _, err = j.Append(text); err != nil {
				t.Fatal(err)
			}
		}
		_ = j.Dispose()
		f, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
		if err != nil {
			t.Fatal(err)
		}
		_, _ = f.WriteString(tail)
		_ = f.Close()
		read, err := OpenExisting(path, 1<<16, turn, Salvage())
		if err != nil {
			t.Fatal(name, err)
		}
		if read.Count() != 2 || !slices.Equal(read.Marks(), []uint64{1}) || (read.Damage() != nil) != (name == "corrupt line") {
			t.Fatalf("%s: read %d records, marks %v, damage %v", name, read.Count(), read.Marks(), read.Damage())
		}
		_ = read.Dispose()
		j, err = Open(path, 1<<16, turn)
		if name == "corrupt line" {
			if err == nil {
				j.Dispose()
				t.Fatal("appending after a corrupt line")
			}
			continue
		}
		if err != nil || j.Count() != 2 || !slices.Equal(j.Marks(), []uint64{1}) {
			t.Fatalf("%s: %v", name, err)
		}
		_ = j.Dispose()
	}
}

// Range reads a run of records in one pass; Span and Fit say how much of the
// file a run takes, so a page is bounded before it is read.
func TestRangeSpanAndFit(t *testing.T) {
	j, err := Open(filepath.Join(t.TempDir(), "events"), 1<<16)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Dispose()
	sizes := []int{10, 200, 30, 4000, 50}
	for _, n := range sizes {
		if _, err = j.Append(strings.Repeat("x", n)); err != nil {
			t.Fatal(err)
		}
	}
	all, err := j.Range(0, 5)
	if err != nil || len(all) != 5 {
		t.Fatal(len(all), err)
	}
	for i, r := range all {
		var text string
		if err = json.Unmarshal(r.Value, &text); err != nil || r.Sequence != uint64(i+1) || len(text) != sizes[i] {
			t.Fatalf("record %d: %+v %v", i, r, err)
		}
		next, _ := j.Next(context.Background(), uint64(i))
		if string(next.Value) != string(r.Value) || !next.Time.Equal(r.Time) {
			t.Fatalf("Range and Next disagree at %d", i+1)
		}
	}
	if part, err := j.Range(1, 3); err != nil || len(part) != 2 || part[0].Sequence != 2 || part[1].Sequence != 3 {
		t.Fatalf("Range(1, 3): %+v %v", part, err)
	}
	if part, err := j.Range(3, 3); err != nil || len(part) != 0 {
		t.Fatal("an empty range", err)
	}
	if _, err = j.Range(0, 6); !errors.Is(err, ErrCursor) {
		t.Fatal("a range past the end", err)
	}
	total := j.Span(0, 5)
	if j.Span(0, 2)+j.Span(2, 5) != total || j.Span(3, 3) != 0 {
		t.Fatal("spans do not add up")
	}
	// The last two records fit in their own span; one byte less keeps only the last.
	two := j.Span(3, 5)
	if after := j.Fit(5, two); after != 3 {
		t.Fatalf("Fit(5, %d) = %d, want 3", two, after)
	}
	if after := j.Fit(5, two-1); after != 4 {
		t.Fatalf("Fit(5, %d) = %d, want 4", two-1, after)
	}
	if after := j.Fit(5, total); after != 0 {
		t.Fatal("everything fits", after)
	}
	// Not even the last record fits: nothing.
	if after := j.Fit(4, 10); after != 4 {
		t.Fatal("an oversized record fit", after)
	}
}
