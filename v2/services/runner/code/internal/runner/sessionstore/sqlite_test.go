package sessionstore

import (
	"context"
	"database/sql"
	"path/filepath"
	"testing"
)

func TestVersionOneMigrationRetainsRecordsAndCreatesStableIdentity(t *testing.T) {
	path := filepath.Join(t.TempDir(), "sessions.db")
	db, err := sql.Open("sqlite", path)
	if err != nil {
		t.Fatal(err)
	}
	_, err = db.Exec(`CREATE TABLE attempts(id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL,previous TEXT UNIQUE,created_at INTEGER NOT NULL,body BLOB NOT NULL);INSERT INTO attempts VALUES('old','old',NULL,1,'{"ID":"old","ConversationID":"old","Title":"Earlier work"}');PRAGMA user_version=1;`)
	if err != nil {
		t.Fatal(err)
	}
	db.Close()
	ctx := context.Background()
	store, err := OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	id, err := store.Identity(ctx)
	if err != nil || id == "" {
		t.Fatal(id, err)
	}
	row, err := store.Get(ctx, "old")
	if err != nil || row.Title != "Earlier work" {
		t.Fatal(row, err)
	}
	store.Close()
	store, err = OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	again, err := store.Identity(ctx)
	if err != nil || again != id {
		t.Fatal(again, err)
	}
}

func TestDeleteRemovesWholeConversationAndRejectsResurrection(t *testing.T) {
	ctx := context.Background()
	path := filepath.Join(t.TempDir(), "sessions.db")
	s, err := OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, r := range []Record{{ID: "root", ConversationID: "root"}, {ID: "next", ConversationID: "root", Previous: "root"}, {ID: "other", ConversationID: "other"}} {
		if err = s.Create(ctx, r); err != nil {
			t.Fatal(err)
		}
	}
	ids, err := s.Delete(ctx, "root")
	if err != nil || len(ids) != 2 {
		t.Fatal(ids, err)
	}
	rows, err := s.List(ctx, 100, 0)
	if err != nil || len(rows) != 1 || rows[0].ID != "other" {
		t.Fatal(rows, err)
	}
	s.Close()
	s, err = OpenSQLite(path)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	ids, err = s.Deleted(ctx, "next")
	if err != nil || len(ids) != 2 {
		t.Fatal(ids, err)
	}
	if err = s.Create(ctx, Record{ID: "late", ConversationID: "root", Previous: "next"}); err == nil {
		t.Fatal("in-flight resume resurrected deletion")
	}
	if err = s.Create(ctx, Record{ID: "root", ConversationID: "root"}); err == nil {
		t.Fatal("legacy import resurrected deletion")
	}
}
