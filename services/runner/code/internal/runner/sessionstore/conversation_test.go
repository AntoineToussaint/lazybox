package sessionstore

import (
	"context"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
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

func TestGeneratedTitleNeverOverridesARename(t *testing.T) {
	ctx := context.Background()
	store, err := OpenSQLite(filepath.Join(t.TempDir(), "sessions.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	opening := "I want to start a plan, we want to look at SOTA but the goal is to write down a design"
	if err = store.Create(ctx, Record{ID: "root", ConversationID: "root", Title: "workspace", Snapshot: engine.Snapshot{Title: opening[:80], Opening: opening}}); err != nil {
		t.Fatal(err)
	}
	// Before a title is generated: the first message, cut at a word.
	row, err := store.Get(ctx, "root")
	if err != nil || row.DisplayTitle != "Start a plan, we want to look at SOTA but the goal…" || row.Opening != opening {
		t.Fatalf("%q %v", row.DisplayTitle, err)
	}
	if err = store.SetGeneratedTitle(ctx, "root", "Conversation title design"); err != nil {
		t.Fatal(err)
	}
	if err = store.Create(ctx, Record{ID: "next", ConversationID: "root", Previous: "root", Snapshot: engine.Snapshot{Title: "Follow-up"}}); err != nil {
		t.Fatal(err)
	}
	row, err = store.Get(ctx, "next")
	if err != nil || row.DisplayTitle != "Conversation title design" || row.GeneratedTitle != "Conversation title design" || row.UserTitle != "" {
		t.Fatal(row.DisplayTitle, row.GeneratedTitle, row.UserTitle, err)
	}
	rows, err := store.Query(ctx, Query{Limit: 10, Search: "title design"})
	if err != nil || len(rows) != 1 || rows[0].DisplayTitle != "Conversation title design" {
		t.Fatal(rows, err)
	}
	// The person renames it: their name wins, and a later generation does not change it.
	if err = store.Rename(ctx, "next", "Titles"); err != nil {
		t.Fatal(err)
	}
	if err = store.SetGeneratedTitle(ctx, "next", "Something else entirely"); err != nil {
		t.Fatal(err)
	}
	row, err = store.Get(ctx, "root")
	if err != nil || row.DisplayTitle != "Titles" || row.UserTitle != "Titles" {
		t.Fatal(row.DisplayTitle, row.UserTitle, err)
	}
	if err = store.SetGeneratedTitle(ctx, "missing", "x"); err == nil {
		t.Fatal("a generated title for an unknown conversation was accepted")
	}
	if err = store.SetGeneratedTitle(ctx, "root", "  "); err == nil {
		t.Fatal("an empty generated title was accepted")
	}
}

// A database from before generated titles opens and keeps its renames.
func TestGeneratedTitleMigrationKeepsRenames(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "sessions.db")
	store, err := OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	if err = store.Create(ctx, Record{ID: "root", ConversationID: "root", Snapshot: engine.Snapshot{Title: "Hello"}}); err != nil {
		t.Fatal(err)
	}
	if err = store.Rename(ctx, "root", "Kept"); err != nil {
		t.Fatal(err)
	}
	// Back to the previous schema, as an older Runner left it.
	if _, err = store.db.Exec(`ALTER TABLE conversation_metadata DROP COLUMN generated; ALTER TABLE conversation_metadata DROP COLUMN pinned_at; ALTER TABLE conversation_metadata DROP COLUMN archived_at; PRAGMA user_version=3`); err != nil {
		t.Fatal(err)
	}
	store.Close()
	if store, err = OpenSQLite(path); err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	row, err := store.Get(ctx, "root")
	if err != nil || row.DisplayTitle != "Kept" {
		t.Fatal(row.DisplayTitle, err)
	}
	if err = store.SetGeneratedTitle(ctx, "root", "Generated"); err != nil {
		t.Fatal(err)
	}
}
