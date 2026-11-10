package sessionstore

import (
	"context"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/engine"
	"path/filepath"
	"testing"
)

func TestConversationTitleAndSearchSurviveContinuation(t *testing.T) {
	ctx := context.Background()
	store, err := OpenSQLite(filepath.Join(t.TempDir(), "sessions.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	if err = store.Create(ctx, Record{ID: "root", ConversationID: "root", Title: "workspace", Snapshot: engine.Snapshot{Title: "Original objective"}}); err != nil {
		t.Fatal(err)
	}
	if err = store.Create(ctx, Record{ID: "next", ConversationID: "root", Previous: "root", Title: "Original objective", Snapshot: engine.Snapshot{Title: "Follow-up question"}}); err != nil {
		t.Fatal(err)
	}
	row, err := store.Get(ctx, "next")
	if err != nil || row.DisplayTitle != "Original objective" {
		t.Fatal(row, err)
	}
	rows, err := store.Query(ctx, Query{Limit: 30, Search: "Original objective"})
	if err != nil || len(rows) != 1 || rows[0].ID != "next" {
		t.Fatal(rows, err)
	}
	rows, err = store.Query(ctx, Query{Limit: 30, Search: "Follow-up question"})
	if err != nil || len(rows) != 0 {
		t.Fatal(rows, err)
	}
	if err = store.Rename(ctx, "next", "My chosen name"); err != nil {
		t.Fatal(err)
	}
	row, err = store.Get(ctx, "root")
	if err != nil || row.DisplayTitle != "My chosen name" {
		t.Fatal(row, err)
	}
}
