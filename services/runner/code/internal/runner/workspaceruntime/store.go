package workspaceruntime

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sync"
	"time"
)

// LoadJSON loads a JSON file into v; a missing file leaves v as it is.
func LoadJSON(path string, v any) error { return readJSON(path, v) }

// SaveJSON replaces a JSON file atomically, readable by its owner only.
func SaveJSON(path string, v any) error { return writeJSON(path, v) }

// readJSON loads path into v; a missing file leaves v as it is.
func readJSON(path string, v any) error {
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return err
	}
	return json.Unmarshal(data, v)
}

// writeJSON replaces path atomically, readable by its owner only.
func writeJSON(path string, v any) error {
	data, err := json.MarshalIndent(v, "", "  ")
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".*")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	if _, err = tmp.Write(data); err == nil {
		err = tmp.Sync()
	}
	if closeErr := tmp.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(tmp.Name(), path)
}

// maxApprovedVersions bounds the approved hashes kept per repository: the
// versions on branches a person still has workspaces for.
const maxApprovedVersions = 32

type approval struct {
	// Hashes the person approved, newest last.
	Hashes []string `json:"hashes"`
	// Entries of the newest approved version, to show what changed since.
	Entries    map[string]string `json:"entries,omitempty"`
	ApprovedAt time.Time         `json:"approvedAt"`
}

// Approvals remembers which configurations a person approved, per
// repository. An approval covers one exact configuration (its Hash): any
// change to what would run needs a new one.
type Approvals struct {
	mu    sync.Mutex
	path  string
	repos map[string]approval
}

// OpenApprovals loads the approvals kept at path.
func OpenApprovals(path string) (*Approvals, error) {
	a := &Approvals{path: path, repos: map[string]approval{}}
	if err := readJSON(path, &a.repos); err != nil {
		return nil, err
	}
	return a, nil
}

// Status is "approved" when the person approved exactly c for repository,
// "changed" when they approved another version, else "required". Changed
// lists the entries that differ from the newest approved version.
func (a *Approvals) Status(repository string, c Config) (string, []string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	r, ok := a.repos[repository]
	if !ok || len(r.Hashes) == 0 {
		return "required", nil
	}
	hash := c.Hash()
	for _, h := range r.Hashes {
		if h == hash {
			return "approved", nil
		}
	}
	return "changed", Changed(r.Entries, c.Entries())
}

// Approve records that the person approved c for repository.
func (a *Approvals) Approve(repository string, c Config) error {
	a.mu.Lock()
	defer a.mu.Unlock()
	hash := c.Hash()
	r := a.repos[repository]
	hashes := []string{}
	for _, h := range r.Hashes {
		if h != hash {
			hashes = append(hashes, h)
		}
	}
	hashes = append(hashes, hash)
	if len(hashes) > maxApprovedVersions {
		hashes = hashes[len(hashes)-maxApprovedVersions:]
	}
	next := map[string]approval{}
	for k, v := range a.repos {
		next[k] = v
	}
	next[repository] = approval{Hashes: hashes, Entries: c.Entries(), ApprovedAt: time.Now().UTC()}
	if err := writeJSON(a.path, next); err != nil {
		return err
	}
	a.repos = next
	return nil
}
