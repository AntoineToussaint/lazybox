package sessionstore

import (
	"context"
	"database/sql"
	"errors"
	"os"
	"path/filepath"
	"testing"
)

// Pins and archive marks belong to the conversation: a continuation shares
// them, they survive a restart of the Runner, and deleting the conversation
// takes them with it.
func TestConversationMarksBelongToTheConversation(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "sessions.db")
	store, err := OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	if err = store.Create(ctx, Record{ID: "root", ConversationID: "root", Title: "Fix the parser"}); err != nil {
		t.Fatal(err)
	}
	if err = store.Create(ctx, Record{ID: "next", ConversationID: "root", Previous: "root"}); err != nil {
		t.Fatal(err)
	}
	row, err := store.Get(ctx, "next")
	if err != nil || row.PinnedAt != 0 || row.ArchivedAt != 0 {
		t.Fatal("a new conversation is neither pinned nor archived", row, err)
	}
	if err = store.Pin(ctx, "next", true); err != nil {
		t.Fatal(err)
	}
	first, err := store.Get(ctx, "root")
	if err != nil || first.PinnedAt == 0 {
		t.Fatal("the pin is the conversation's, not one attempt's", first, err)
	}
	// Pinning again keeps the first pin, so the order of pins does not move.
	if err = store.Pin(ctx, "root", true); err != nil {
		t.Fatal(err)
	}
	if again, _ := store.Get(ctx, "next"); again.PinnedAt != first.PinnedAt {
		t.Fatal("a second pin moved the first", again.PinnedAt, first.PinnedAt)
	}
	// A pin does not touch the title, and a rename does not touch the pin.
	if first.DisplayTitle != "Fix the parser" {
		t.Fatal("pinning changed the title", first.DisplayTitle)
	}
	if err = store.Rename(ctx, "next", "Parser rewrite"); err != nil {
		t.Fatal(err)
	}
	if err = store.Archive(ctx, "root", true); err != nil {
		t.Fatal(err)
	}
	row, _ = store.Get(ctx, "next")
	if row.DisplayTitle != "Parser rewrite" || row.PinnedAt != first.PinnedAt || row.ArchivedAt == 0 {
		t.Fatal("marks and title interfere", row)
	}
	// History lists an archived conversation with its marks.
	rows, err := store.Query(ctx, Query{Limit: 10})
	if err != nil || len(rows) != 1 || rows[0].ID != "next" || rows[0].ArchivedAt == 0 || rows[0].PinnedAt == 0 {
		t.Fatal("history lost the marks", rows, err)
	}
	store.Close()

	store, err = OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	row, _ = store.Get(ctx, "next")
	if row.PinnedAt != first.PinnedAt || row.ArchivedAt == 0 {
		t.Fatal("marks did not survive a restart", row)
	}
	if err = store.Pin(ctx, "next", false); err != nil {
		t.Fatal(err)
	}
	if err = store.Archive(ctx, "next", false); err != nil {
		t.Fatal(err)
	}
	row, _ = store.Get(ctx, "root")
	if row.PinnedAt != 0 || row.ArchivedAt != 0 || row.DisplayTitle != "Parser rewrite" {
		t.Fatal("clearing marks", row)
	}
	if err = store.Pin(ctx, "missing", true); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("pinning an unknown conversation", err)
	}
	if err = store.Pin(ctx, "next", true); err != nil {
		t.Fatal(err)
	}
	if _, err = store.Delete(ctx, "next"); err != nil {
		t.Fatal(err)
	}
	var left int
	if err = store.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM conversation_metadata`).Scan(&left); err != nil || left != 0 {
		t.Fatal("deleting a conversation kept its marks", left, err)
	}
}

// A version 3 database (titles, no marks) keeps its titles and gains marks.
func TestVersionThreeMigrationKeepsTitlesAndAddsMarks(t *testing.T) {
	path := filepath.Join(t.TempDir(), "sessions.db")
	db, err := sql.Open("sqlite", path)
	if err != nil {
		t.Fatal(err)
	}
	_, err = db.Exec(`CREATE TABLE attempts(id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL,previous TEXT UNIQUE,created_at INTEGER NOT NULL,body BLOB NOT NULL);
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL); INSERT INTO metadata VALUES ('runner_id','r');
CREATE TABLE conversation_metadata (id TEXT PRIMARY KEY, title TEXT NOT NULL);
CREATE TABLE deleted_attempts(id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL);
INSERT INTO attempts VALUES('old','old',NULL,1,'{"ID":"old","ConversationID":"old","Title":"workspace"}');
INSERT INTO conversation_metadata VALUES('old','Named before');
PRAGMA user_version=3;`)
	if err != nil {
		t.Fatal(err)
	}
	db.Close()
	ctx := context.Background()
	store, err := OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	row, err := store.Get(ctx, "old")
	if err != nil || row.DisplayTitle != "Named before" || row.PinnedAt != 0 {
		t.Fatal(row, err)
	}
	if err = store.Pin(ctx, "old", true); err != nil {
		t.Fatal(err)
	}
	if row, _ = store.Get(ctx, "old"); row.PinnedAt == 0 || row.DisplayTitle != "Named before" {
		t.Fatal("pin after migration", row)
	}
}
