package journal

import (
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
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

func TestPartialWriteIsReportedRatherThanReplayedAsComplete(t *testing.T) {
	path := filepath.Join(t.TempDir(), "events")
	if err := os.WriteFile(path, []byte(`{"sequence":1`), 0600); err != nil {
		t.Fatal(err)
	}
	if j, err := Open(path, 4096); err == nil {
		j.Dispose()
		t.Fatal("accepted a torn journal")
	}
}
