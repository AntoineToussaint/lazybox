// Package repositorycatalog retains discovery metadata independently of checkouts.
// Providers discover identities; preparation is an explicit Runner operation.
package repositorycatalog

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"
)

type Entry struct {
	Identity, Name, RepositoryID, LocalPath string
	GitHubID                                int64
}
type SourceState struct {
	ID, Key, State, Detail string
	UpdatedAt              int64
}
type Source interface {
	Describe() SourceState
	Discover(context.Context) ([]Entry, error)
}
type Snapshot struct {
	Version int
	Sources map[string]Cached
}
type Cached struct {
	Key       string
	Entries   []Entry
	UpdatedAt int64
}
type Store interface {
	Load() (Snapshot, error)
	Save(Snapshot) error
}
type Options struct {
	AutoRefresh    *bool `json:"autoRefresh,omitempty"`
	RefreshSeconds int   `json:"refreshSeconds,omitempty"`
	MaxPages       int   `json:"maxPages,omitempty"`
	GitHubEnabled  *bool `json:"githubEnabled,omitempty"`
}

func (o Options) RefreshInterval() int {
	if o.AutoRefresh != nil && !*o.AutoRefresh {
		return 0
	}
	if o.RefreshSeconds <= 0 {
		return 300
	}
	return o.RefreshSeconds
}
func (o Options) PageLimit() int {
	if o.MaxPages <= 0 {
		return 1000
	}
	return o.MaxPages
}
func (o Options) GitHub() bool { return o.GitHubEnabled == nil || *o.GitHubEnabled }

type Cache struct {
	refresh sync.Mutex
	mu      sync.Mutex
	store   Store
	data    Snapshot
	Now     func() time.Time
}

func New(store Store) (*Cache, error) {
	s, err := store.Load()
	if errors.Is(err, os.ErrNotExist) {
		s = Snapshot{Version: 1, Sources: map[string]Cached{}}
	} else if err != nil {
		return nil, err
	}
	if s.Version != 1 {
		return nil, errors.New("unsupported repository catalog")
	}
	if s.Sources == nil {
		s.Sources = map[string]Cached{}
	}
	return &Cache{store: store, data: s, Now: time.Now}, nil
}
func (c *Cache) cached(source SourceState) Cached {
	c.mu.Lock()
	defer c.mu.Unlock()
	v := c.data.Sources[source.ID]
	if v.Key != source.Key {
		return Cached{}
	}
	v.Entries = append([]Entry(nil), v.Entries...)
	return v
}

// Read retains the last complete result on provider failure. It never replaces
// a complete catalog with partial pagination and never crosses account scopes.
func (c *Cache) Read(ctx context.Context, source Source, refresh bool) ([]Entry, SourceState, error) {
	state := source.Describe()
	old := c.cached(state)
	state.UpdatedAt = old.UpdatedAt
	if !refresh || state.State != "connected" {
		return old.Entries, state, nil
	}
	c.refresh.Lock()
	defer c.refresh.Unlock()
	state = source.Describe()
	old = c.cached(state)
	state.UpdatedAt = old.UpdatedAt
	if state.State != "connected" {
		return old.Entries, state, nil
	}
	rows, err := source.Discover(ctx)
	if err != nil {
		state.State = "error"
		state.Detail = err.Error()
		return old.Entries, state, nil
	}
	if source.Describe().Key != state.Key {
		return nil, SourceState{ID: state.ID, State: "changed", Detail: "Connection changed; refresh repositories."}, nil
	}
	rows = Merge(rows)
	next := Cached{Key: state.Key, Entries: rows, UpdatedAt: c.Now().Unix()}
	c.mu.Lock()
	defer c.mu.Unlock()
	snapshot := Snapshot{Version: 1, Sources: map[string]Cached{}}
	for id, v := range c.data.Sources {
		snapshot.Sources[id] = v
	}
	snapshot.Sources[state.ID] = next
	if err = c.store.Save(snapshot); err != nil {
		return old.Entries, state, err
	}
	c.data = snapshot
	state.UpdatedAt = next.UpdatedAt
	return append([]Entry(nil), rows...), state, nil
}

// Merge prefers prepared sources and local checkouts without duplicating a GitHub identity.
func Merge(groups ...[]Entry) []Entry {
	byID := map[string]Entry{}
	for _, rows := range groups {
		for _, row := range rows {
			if row.Identity == "" {
				continue
			}
			previous, ok := byID[row.Identity]
			if !ok {
				byID[row.Identity] = row
				continue
			}
			if previous.GitHubID == 0 {
				previous.GitHubID = row.GitHubID
			}
			if previous.Name == "" {
				previous.Name = row.Name
			}
			if previous.RepositoryID == "" && row.RepositoryID != "" {
				previous.RepositoryID = row.RepositoryID
				previous.LocalPath = row.LocalPath
			} else if previous.LocalPath == "" {
				previous.LocalPath = row.LocalPath
			}
			byID[row.Identity] = previous
		}
	}
	out := make([]Entry, 0, len(byID))
	for _, row := range byID {
		out = append(out, row)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

type FileStore struct{ Path string }

func (s FileStore) Load() (Snapshot, error) {
	var out Snapshot
	info, err := os.Lstat(s.Path)
	if err != nil {
		return out, err
	}
	if !info.Mode().IsRegular() || info.Mode().Perm()&0077 != 0 || info.Size() > 32<<20 {
		return out, errors.New("invalid repository catalog file")
	}
	b, err := os.ReadFile(s.Path)
	if err == nil {
		err = json.Unmarshal(b, &out)
	}
	return out, err
}
func (s FileStore) Save(value Snapshot) error {
	b, err := json.Marshal(value)
	if err != nil {
		return err
	}
	if len(b) > 32<<20 {
		return errors.New("repository catalog exceeds storage limit")
	}
	if err = os.MkdirAll(filepath.Dir(s.Path), 0700); err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(s.Path), ".repository-catalog-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(b)
	if err == nil {
		err = f.Sync()
	}
	err = errors.Join(err, f.Close())
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), s.Path)
}

// Remove forgets discovery metadata, never a checkout. Explicit discovery may find it again.
func (c *Cache) Remove(identity string) error {
	c.refresh.Lock()
	defer c.refresh.Unlock()
	c.mu.Lock()
	defer c.mu.Unlock()
	next := Snapshot{Version: 1, Sources: map[string]Cached{}}
	for id, source := range c.data.Sources {
		rows := []Entry{}
		for _, entry := range source.Entries {
			if entry.Identity != identity {
				rows = append(rows, entry)
			}
		}
		source.Entries = rows
		next.Sources[id] = source
	}
	if err := c.store.Save(next); err != nil {
		return err
	}
	c.data = next
	return nil
}
