package repositorycatalog

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"
)

type fakeSource struct {
	key   string
	fail  bool
	calls int
}

func (f *fakeSource) Describe() SourceState {
	return SourceState{ID: "github", Key: f.key, State: "connected"}
}
func (f *fakeSource) Discover(context.Context) ([]Entry, error) {
	f.calls++
	if f.fail {
		return []Entry{{Identity: "partial"}}, errors.New("page two failed")
	}
	return []Entry{{Identity: "github:org/repo", Name: "org/repo", GitHubID: 42}}, nil
}
func TestPersistentCatalogScopeFailureAndRemoval(t *testing.T) {
	store := FileStore{filepath.Join(t.TempDir(), "catalog.json")}
	cache, err := New(store)
	if err != nil {
		t.Fatal(err)
	}
	cache.Now = func() time.Time { return time.Unix(1000, 0) }
	source := &fakeSource{key: "account-a"}
	rows, _, err := cache.Read(context.Background(), source, false)
	if err != nil || len(rows) != 0 || source.calls != 0 {
		t.Fatal(rows, err)
	}
	rows, state, err := cache.Read(context.Background(), source, true)
	if err != nil || len(rows) != 1 || state.UpdatedAt != 1000 {
		t.Fatal(rows, state, err)
	}
	cache, err = New(store)
	if err != nil {
		t.Fatal(err)
	}
	rows, _, err = cache.Read(context.Background(), source, false)
	if err != nil || len(rows) != 1 || source.calls != 1 {
		t.Fatal("restart lost discovery", rows, err)
	}
	source.fail = true
	rows, state, err = cache.Read(context.Background(), source, true)
	if err != nil || len(rows) != 1 || rows[0].Identity == "partial" || state.State != "error" {
		t.Fatal("partial failure replaced complete discovery", rows, state, err)
	}
	source.key = "account-b"
	rows, _, _ = cache.Read(context.Background(), source, false)
	if len(rows) != 0 {
		t.Fatal("cross-account cache leak")
	}
	source.key = "account-a"
	if err = cache.Remove("github:org/repo"); err != nil {
		t.Fatal(err)
	}
	rows, _, _ = cache.Read(context.Background(), source, false)
	if len(rows) != 0 {
		t.Fatal("remove failed")
	}
	info, _ := os.Stat(store.Path)
	if info.Mode().Perm() != 0600 {
		t.Fatal("catalog permissions")
	}
}
func TestMergeAndConfiguration(t *testing.T) {
	rows := Merge([]Entry{{Identity: "github:org/repo", Name: "org/repo", GitHubID: 42}}, []Entry{{Identity: "github:org/repo", Name: "repo", RepositoryID: "saved", LocalPath: "/source"}})
	if len(rows) != 1 || rows[0].GitHubID != 42 || rows[0].RepositoryID != "saved" || rows[0].LocalPath != "/source" {
		t.Fatal(rows)
	}
	off := false
	options := Options{AutoRefresh: &off, GitHubEnabled: &off, MaxPages: 4}
	if options.RefreshInterval() != 0 || options.GitHub() || options.PageLimit() != 4 {
		t.Fatal(options)
	}
	if (Options{}).RefreshInterval() != 300 || (Options{}).PageLimit() != 1000 || !(Options{}).GitHub() {
		t.Fatal("defaults")
	}
}
func TestInvalidStoreRejected(t *testing.T) {
	path := filepath.Join(t.TempDir(), "bad")
	if err := os.WriteFile(path, []byte(`{"Version":9}`), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err := New(FileStore{path}); err == nil {
		t.Fatal("accepted invalid version")
	}
}
