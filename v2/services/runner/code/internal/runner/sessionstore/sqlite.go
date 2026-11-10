package sessionstore

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/AntoineToussaint/lazybox/v2/internal/runner/engine"
	_ "modernc.org/sqlite"
)

type SQLite struct {
	db   *sql.DB
	lock *os.File
}

// OpenSQLite takes exclusive Runner ownership. A second Runner must not recover
// records while the first still owns their processes.
func OpenSQLite(path string) (*SQLite, error) {
	if !filepath.IsAbs(path) {
		return nil, errors.New("absolute session database path required")
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return nil, err
	}
	lock, err := os.OpenFile(path+".lock", os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, err
	}
	if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		lock.Close()
		return nil, errors.New("session state already owned by another Runner")
	}
	fail := func(err error) (*SQLite, error) { lock.Close(); return nil, err }
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return fail(err)
	}
	f.Close()
	uri := url.URL{Scheme: "file", Path: path}
	db, err := sql.Open("sqlite", uri.String()+"?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)&_pragma=synchronous(FULL)")
	if err != nil {
		return fail(err)
	}
	db.SetMaxOpenConns(1)
	var version int
	if err = db.QueryRow("PRAGMA user_version").Scan(&version); err != nil {
		db.Close()
		return fail(err)
	}
	if version > 3 {
		db.Close()
		return fail(errors.New("session database requires a newer Runner"))
	}
	if version == 0 {
		_, err = db.Exec(`BEGIN; CREATE TABLE attempts (id TEXT PRIMARY KEY, conversation_id TEXT NOT NULL, previous TEXT UNIQUE, created_at INTEGER NOT NULL, body BLOB NOT NULL); CREATE INDEX conversations ON attempts(conversation_id,created_at); PRAGMA user_version=1; COMMIT;`)
		if err != nil {
			db.Close()
			return fail(err)
		}
	}
	if version < 2 {
		_, err = db.Exec(`BEGIN; CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE conversation_metadata (id TEXT PRIMARY KEY, title TEXT NOT NULL); INSERT INTO metadata VALUES ('runner_id', ?); PRAGMA user_version=2; COMMIT;`, rand.Text())
		if err != nil {
			db.Close()
			return fail(err)
		}
	}
	if version < 3 {
		_, err = db.Exec(`BEGIN; CREATE TABLE deleted_attempts(id TEXT PRIMARY KEY,conversation_id TEXT NOT NULL); CREATE INDEX deleted_conversations ON deleted_attempts(conversation_id); PRAGMA user_version=3; COMMIT;`)
		if err != nil {
			db.Close()
			return fail(err)
		}
	}
	return &SQLite{db: db, lock: lock}, nil
}
func (s *SQLite) Create(ctx context.Context, r Record) error {
	if r.ID == "" || r.ConversationID == "" || r.ID == r.Previous {
		return errors.New("valid conversation and attempt identities required")
	}
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	var deleted int
	if err = tx.QueryRowContext(ctx, `SELECT COUNT(*) FROM deleted_attempts WHERE id IN (?,?,?)`, r.ID, r.Previous, r.ConversationID).Scan(&deleted); err != nil {
		return err
	}
	if deleted > 0 {
		return errors.New("conversation was deleted")
	}
	if r.Previous != "" {
		var conversation string
		if err = tx.QueryRowContext(ctx, "SELECT conversation_id FROM attempts WHERE id=?", r.Previous).Scan(&conversation); err != nil {
			return err
		}
		if conversation != r.ConversationID {
			return errors.New("successor must keep conversation identity")
		}
	}
	if r.CreatedAt.IsZero() {
		r.CreatedAt = time.Now().UTC()
	}
	r.UpdatedAt = r.CreatedAt
	body, err := json.Marshal(r)
	if err != nil {
		return err
	}
	var previous any
	if r.Previous != "" {
		previous = r.Previous
	}
	_, err = tx.ExecContext(ctx, "INSERT INTO attempts(id,conversation_id,previous,created_at,body) VALUES(?,?,?,?,?)", r.ID, r.ConversationID, previous, r.CreatedAt.UnixMilli(), body)
	if err != nil {
		return fmt.Errorf("create execution record: %w", err)
	}
	return tx.Commit()
}
func decode(row *sql.Row) (Record, error) {
	var data []byte
	err := row.Scan(&data)
	if errors.Is(err, sql.ErrNoRows) {
		return Record{}, os.ErrNotExist
	}
	if err != nil {
		return Record{}, err
	}
	var r Record
	err = json.Unmarshal(data, &r)
	return r, err
}
func (s *SQLite) Get(ctx context.Context, id string) (Record, error) {
	return decode(s.db.QueryRowContext(ctx, selectedRecord+" WHERE a.id=?", id))
}
func (s *SQLite) Successor(ctx context.Context, id string) (Record, error) {
	return decode(s.db.QueryRowContext(ctx, selectedRecord+" WHERE a.previous=?", id))
}
func (s *SQLite) SaveState(ctx context.Context, id string, state engine.Snapshot, settings json.RawMessage) error {
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	r, err := decode(tx.QueryRowContext(ctx, "SELECT body FROM attempts WHERE id=?", id))
	if err != nil {
		return err
	}
	r.Snapshot = state
	r.ProviderSettings = append(json.RawMessage(nil), settings...)
	r.UpdatedAt = time.Now().UTC()
	body, err := json.Marshal(r)
	if err != nil {
		return err
	}
	if _, err = tx.ExecContext(ctx, "UPDATE attempts SET body=? WHERE id=?", body, id); err != nil {
		return err
	}
	return tx.Commit()
}

const selectedRecord = `SELECT json_set(a.body, '$.UserTitle', COALESCE(m.title,''), '$.DisplayTitle', COALESCE(NULLIF(m.title,''), NULLIF(json_extract(root.body,'$.Snapshot.title'),''),json_extract(root.body,'$.Title'),'')) FROM attempts a LEFT JOIN conversation_metadata m ON m.id=a.conversation_id LEFT JOIN attempts root ON root.id=a.conversation_id`

func (s *SQLite) Identity(ctx context.Context) (string, error) {
	var id string
	err := s.db.QueryRowContext(ctx, "SELECT value FROM metadata WHERE key='runner_id'").Scan(&id)
	return id, err
}

// Rename changes conversation presentation without touching execution state.
func (s *SQLite) Rename(ctx context.Context, id, title string) error {
	title = strings.TrimSpace(title)
	if title == "" || !utf8.ValidString(title) || utf8.RuneCountInString(title) > 120 || strings.IndexFunc(title, unicode.IsControl) >= 0 {
		return errors.New("conversation name must contain 1..120 characters without controls")
	}
	result, err := s.db.ExecContext(ctx, `INSERT INTO conversation_metadata(id,title) SELECT conversation_id,? FROM attempts WHERE id=? ON CONFLICT(id) DO UPDATE SET title=excluded.title`, title, id)
	if err != nil {
		return err
	}
	count, err := result.RowsAffected()
	if err == nil && count == 0 {
		return os.ErrNotExist
	}
	return err
}
func (s *SQLite) List(ctx context.Context, limit, offset int) ([]Record, error) {
	return s.Query(ctx, Query{Limit: limit, Offset: offset})
}
func (s *SQLite) Query(ctx context.Context, q Query) ([]Record, error) {
	limit, offset := q.Limit, q.Offset
	if len(q.Search) > 512 {
		return nil, errors.New("history search is too long")
	}

	if limit < 1 || limit > 100 || offset < 0 {
		return nil, errors.New("history limit must be 1..100 and offset nonnegative")
	}
	where := ` WHERE NOT EXISTS(SELECT 1 FROM attempts b WHERE b.previous=a.id)`
	args := []any{}
	if q.Workspace != nil {
		where += ` AND COALESCE(json_extract(a.body,'$.CollectionID'),'')=?`
		args = append(args, *q.Workspace)
	}
	if q.Search != "" {
		where += ` AND instr(lower(COALESCE(NULLIF(m.title,''),NULLIF(json_extract(root.body,'$.Snapshot.title'),''),json_extract(root.body,'$.Title'),'')),lower(?))>0`
		args = append(args, q.Search)
	}
	args = append(args, limit, offset)
	rows, err := s.db.QueryContext(ctx, selectedRecord+where+` ORDER BY (SELECT MIN(c.created_at) FROM attempts c WHERE c.conversation_id=a.conversation_id) DESC,a.conversation_id DESC LIMIT ? OFFSET ?`, args...)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	result := []Record{}
	for rows.Next() {
		var body []byte
		if err = rows.Scan(&body); err != nil {
			return nil, err
		}
		var r Record
		if err = json.Unmarshal(body, &r); err != nil {
			return nil, err
		}
		result = append(result, r)
	}
	return result, rows.Err()
}
func (s *SQLite) Close() error { return errors.Join(s.db.Close(), s.lock.Close()) }
