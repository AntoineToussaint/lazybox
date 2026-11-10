package telemetry

import (
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	_ "modernc.org/sqlite"
	"net/url"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"
)

type SQLite struct {
	db   *sql.DB
	lock *os.File
}

func Open(path string) (*SQLite, error) {
	if !filepath.IsAbs(path) {
		return nil, errors.New("absolute telemetry database path required")
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
		return nil, errors.New("telemetry already owned by another Runner")
	}
	owned := false
	defer func() {
		if !owned {
			lock.Close()
		}
	}()
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, err
	}
	f.Close()
	u := url.URL{Scheme: "file", Path: path}
	db, err := sql.Open("sqlite", u.String()+"?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)&_pragma=synchronous(FULL)")
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	_, err = db.Exec(`CREATE TABLE IF NOT EXISTS usage (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, execution TEXT NOT NULL, body TEXT NOT NULL);
 CREATE INDEX IF NOT EXISTS usage_workspace ON usage(workspace); CREATE INDEX IF NOT EXISTS usage_execution ON usage(execution);
 CREATE TABLE IF NOT EXISTS contexts (id TEXT PRIMARY KEY, workspace TEXT NOT NULL, execution TEXT NOT NULL, body TEXT NOT NULL);
 CREATE INDEX IF NOT EXISTS usage_conversation ON usage(json_extract(body,'$.scope.conversation'));
 CREATE INDEX IF NOT EXISTS contexts_conversation ON contexts(json_extract(body,'$.scope.conversation'));
 CREATE TABLE IF NOT EXISTS baselines (id TEXT PRIMARY KEY, body TEXT NOT NULL);
 UPDATE usage SET body=json_set(body,'$.status','interrupted','$.reason','Runner stopped before final usage') WHERE json_extract(body,'$.status')='pending';`)
	if err != nil {
		db.Close()
		return nil, err
	}
	owned = true
	return &SQLite{db: db, lock: lock}, nil
}
func validScope(s Scope) bool {
	return s.Execution != "" && s.Conversation != "" && s.Profile != "" && (s.Provider == "claude" || s.Provider == "codex")
}
func validTokens(t Tokens) bool {
	for _, v := range []*int64{t.Input, t.Output, t.CacheRead, t.CacheWrite5m, t.CacheWrite1h} {
		if v != nil && (*v < 0 || *v > 1e12) {
			return false
		}
	}
	return true
}
func (s *SQLite) Put(ctx context.Context, r Record) error {
	if !validScope(r.Scope) || r.ID == "" || len(r.ID) > 1024 || !validTokens(r.Tokens) {
		return errors.New("invalid usage observation")
	}
	if r.ObservedAt.IsZero() {
		r.ObservedAt = time.Now().UTC()
	}
	data, err := json.Marshal(r)
	if err != nil {
		return err
	}
	// A late partial/duplicate stream cannot erase a committed terminal snapshot.
	_, err = s.db.ExecContext(ctx, `INSERT INTO usage VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET body=json_set(excluded.body,'$.scope',json_extract(usage.body,'$.scope')) WHERE json_extract(usage.body,'$.status')!='complete' OR (json_extract(excluded.body,'$.source')='sdk' AND json_extract(excluded.body,'$.tokens.output')>json_extract(usage.body,'$.tokens.output'))`, r.ID, r.Scope.Workspace, r.Scope.Execution, string(data))
	return err
}
func (s *SQLite) PutContext(ctx context.Context, c ContextSnapshot) error {
	if !validScope(c.Scope) || c.Agent == "" {
		return errors.New("invalid context scope")
	}
	for _, v := range []*int64{c.Used, c.Capacity, c.Reserve} {
		if v != nil && (*v < 0 || *v > 1e12) {
			return errors.New("invalid context count")
		}
	}
	c.ObservedAt = time.Now().UTC()
	data, err := json.Marshal(c)
	if err != nil {
		return err
	}
	id := c.Scope.Execution + "/" + c.Scope.Attempt + "/" + c.Agent
	_, err = s.db.ExecContext(ctx, `INSERT INTO contexts VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET body=excluded.body`, id, c.Scope.Workspace, c.Scope.Execution, string(data))
	return err
}

// PutCumulative atomically differences provider lifetime counters. A resumed
// thread with no baseline establishes one, rather than billing imported history.
func (s *SQLite) PutCumulative(ctx context.Context, r Record, resumed bool) error {
	if !validScope(r.Scope) || r.ResponseID == "" || !validTokens(r.Tokens) {
		return errors.New("invalid cumulative usage")
	}
	key := r.Scope.Conversation + "/" + r.Scope.Provider + "/" + r.Scope.Profile + "/" + r.ResponseID
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	var raw string
	err = tx.QueryRowContext(ctx, "SELECT body FROM baselines WHERE id=?", key).Scan(&raw)
	var before Tokens
	missing := errors.Is(err, sql.ErrNoRows)
	if err != nil && !missing {
		return err
	}
	if !missing {
		if err = json.Unmarshal([]byte(raw), &before); err != nil {
			return err
		}
	}
	totals, _ := json.Marshal(r.Tokens)
	r.ID = fmt.Sprintf("sdk-total:%s:%x", key, sha256.Sum256(totals))
	r.ObservedAt = time.Now().UTC()
	delta := func(now, old *int64) *int64 {
		if now == nil {
			return nil
		}
		if old == nil {
			old = number(0)
		}
		if *now < *old {
			return nil
		}
		return number(*now - *old)
	}
	if !(resumed && missing) {
		t := r.Tokens
		r.Tokens = Tokens{Input: delta(t.Input, before.Input), Output: delta(t.Output, before.Output), CacheRead: delta(t.CacheRead, before.CacheRead), CacheWrite5m: delta(t.CacheWrite5m, before.CacheWrite5m), CacheWrite1h: delta(t.CacheWrite1h, before.CacheWrite1h)}
		if r.Tokens.Input == nil || r.Tokens.Output == nil {
			return nil
		} // Ignore stale/regressing counters.
		// Lifetime deltas may span multiple models. Preserve counts but never price
		// them using whichever model happened to be selected at the last update.
		r.Nanos = nil
		r.Reason = "Provider cumulative usage; per-request model price unavailable"
		if *r.Tokens.Input+*r.Tokens.Output+value(r.Tokens.CacheRead) > 0 {
			body, _ := json.Marshal(r)
			if _, err = tx.ExecContext(ctx, "INSERT OR IGNORE INTO usage VALUES(?,?,?,?)", r.ID, r.Scope.Workspace, r.Scope.Execution, string(body)); err != nil {
				return err
			}
		}
	}
	if _, err = tx.ExecContext(ctx, "INSERT INTO baselines VALUES(?,?) ON CONFLICT(id) DO UPDATE SET body=excluded.body", key, string(totals)); err != nil {
		return err
	}
	return tx.Commit()
}
func value(v *int64) int64 {
	if v == nil {
		return 0
	}
	return *v
}
func sum(a **int64, b *int64) {
	if b != nil {
		if *a == nil {
			*a = number(0)
		}
		**a += *b
	}
}
func (s *SQLite) Report(ctx context.Context, q Query) (Report, error) {
	out := Report{}
	where := "execution=?"
	value := q.Execution
	if q.Workspace != "" {
		where = "workspace=?"
		value = q.Workspace
	}
	if q.Conversation != "" {
		where = "json_extract(body,'$.scope.conversation')=?"
		value = q.Conversation
	}
	if value == "" {
		return out, errors.New("workspace, conversation or execution required")
	}
	rows, err := s.db.QueryContext(ctx, "SELECT body FROM usage WHERE "+where+" ORDER BY json_extract(body,'$.status')='complete' DESC, json_extract(body,'$.source')='gateway' DESC", value)
	if err != nil {
		return out, err
	}
	groups := map[string]*Group{}
	seen := map[string]bool{}
	for rows.Next() {
		var raw string
		if err = rows.Scan(&raw); err != nil {
			break
		}
		var r Record
		if err = json.Unmarshal([]byte(raw), &r); err != nil {
			break
		}
		if !strings.HasPrefix(r.ID, "sdk-total:") && r.Status == "complete" && r.ResponseID != "" {
			key := r.Scope.Conversation + "/" + r.Scope.Provider + "/" + r.Scope.Profile + "/" + r.ResponseID
			if seen[key] {
				continue
			}
			seen[key] = true
		}
		if r.Nanos == nil && q.Prices != nil && (r.Reason == "" || r.Reason == "No exact model price" || r.Reason == "Incomplete rate card") {
			r.Reason = ""
			r = q.Prices.Price(r)
		}
		key := fmt.Sprintf("%s/%s/%s/%s/%s/%s", r.Scope.Execution, r.Scope.Profile, r.Scope.Attempt, r.Agent, r.Model, r.Source)
		g := groups[key]
		if g == nil {
			g = &Group{Scope: r.Scope, Agent: r.Agent, Model: r.Model, Source: r.Source}
			groups[key] = g
		}
		g.Records++
		if r.Status != "complete" {
			g.Incomplete++
		}
		if r.Nanos == nil {
			g.Unpriced++
		}
		sum(&g.Nanos, r.Nanos)
		sum(&g.Tokens.Input, r.Tokens.Input)
		sum(&g.Tokens.Output, r.Tokens.Output)
		sum(&g.Tokens.CacheRead, r.Tokens.CacheRead)
		sum(&g.Tokens.CacheWrite5m, r.Tokens.CacheWrite5m)
		sum(&g.Tokens.CacheWrite1h, r.Tokens.CacheWrite1h)
		out.Recent = append(out.Recent, r)
		sort.Slice(out.Recent, func(i, j int) bool { return out.Recent[i].ObservedAt.After(out.Recent[j].ObservedAt) })
		if len(out.Recent) > 100 {
			out.Recent = out.Recent[:100]
		}
	}
	if err == nil {
		err = rows.Err()
	}
	rows.Close()
	if err != nil {
		return out, err
	}
	keys := []string{}
	for k := range groups {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		out.Groups = append(out.Groups, *groups[k])
	}
	rows, err = s.db.QueryContext(ctx, "SELECT body FROM contexts WHERE "+where, value)
	if err != nil {
		return out, err
	}
	defer rows.Close()
	for rows.Next() {
		var raw string
		if err = rows.Scan(&raw); err != nil {
			return out, err
		}
		var c ContextSnapshot
		if err = json.Unmarshal([]byte(raw), &c); err != nil {
			return out, err
		}
		out.Contexts = append(out.Contexts, c)
	}
	sort.Slice(out.Contexts, func(i, j int) bool { return out.Contexts[i].ObservedAt.After(out.Contexts[j].ObservedAt) })
	return out, rows.Err()
}
func (s *SQLite) Close() error { err := s.db.Close(); _ = s.lock.Close(); return err }

// LatestModel returns the most recent model reported for the main agent only.
func (s *SQLite) LatestModel(ctx context.Context, execution string) (string, error) {
	var model string
	err := s.db.QueryRowContext(ctx, `SELECT json_extract(body,'$.model') FROM (SELECT body FROM contexts WHERE execution=? UNION ALL SELECT body FROM usage WHERE execution=?) WHERE json_extract(body,'$.agent')='root' AND COALESCE(json_extract(body,'$.model'),'')!='' ORDER BY json_extract(body,'$.observedAt') DESC LIMIT 1`, execution, execution).Scan(&model)
	if errors.Is(err, sql.ErrNoRows) {
		return "", nil
	}
	return model, err
}
