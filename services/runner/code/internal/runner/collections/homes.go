package collections

import (
	"errors"
	"os"
	"path/filepath"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
)

// Where a repository's home checkout comes from (Manager.Home).
const (
	// HomeConfigured: chosen in Settings (created or adopted).
	HomeConfigured = "configured"
	// HomeSource: the regular checkout the repository was registered from,
	// which is already the person's own folder.
	HomeSource = "source"
)

// Home is a repository's home checkout and where it comes from: the one
// chosen in Settings, else (for a repository registered from a regular
// checkout) that checkout. A bare source, such as a GitHub import's private
// copy, is never a home. Another registration of the same repository (an
// older alias a workspace member still names) shares its home. "" when none.
func (m *Manager) Home(id string) (path, kind string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.home(id)
}

func (m *Manager) home(id string) (string, string) {
	r, ok := m.repos[id]
	if !ok {
		return "", ""
	}
	if r.Home != "" {
		return r.Home, HomeConfigured
	}
	identity := gitidentity.Repository(m.common[id], r.SourceURL)
	for _, other := range m.repos {
		if other.Home != "" && gitidentity.Repository(m.common[other.ID], other.SourceURL) == identity {
			return other.Home, HomeConfigured
		}
	}
	if common := m.common[id]; common != "" && filepath.Clean(common) != filepath.Clean(r.Path) && !Within(m.root, r.Path) {
		return r.Path, HomeSource
	}
	return "", ""
}

// WorktreeRoot is the folder every workspace lives under.
func (m *Manager) WorktreeRoot() string { return m.root }

// SetHome records path (absolute, outside the workspaces folder) as the
// repository's home checkout, or forgets it when path is "". It only writes
// the library: creating, adopting and inspecting the folder are the homes
// package's, and no file in a forgotten home is touched.
func (m *Manager) SetHome(id, path string) (Repository, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return Repository{}, errors.New("workspace manager closed")
	}
	r, ok := m.repos[id]
	if !ok {
		return Repository{}, os.ErrNotExist
	}
	if path != "" {
		if !filepath.IsAbs(path) {
			return Repository{}, errors.New("a home checkout needs an absolute path")
		}
		if Within(m.root, path) {
			return Repository{}, errors.New("a home checkout cannot be inside Lazybox's workspaces folder")
		}
	}
	// One home per repository: aliases of the same repository forget theirs.
	identity := gitidentity.Repository(m.common[id], r.SourceURL)
	list := make([]Repository, 0, len(m.repos))
	for _, old := range m.repos {
		if old.ID == id {
			old.Home = path
		} else if old.Home != "" && gitidentity.Repository(m.common[old.ID], old.SourceURL) == identity {
			old.Home = ""
		}
		list = append(list, old)
	}
	if err := m.saveRepositories(list); err != nil {
		return Repository{}, err
	}
	for _, entry := range list {
		m.repos[entry.ID] = entry
	}
	return m.repos[id], nil
}

// Within reports whether path is root or inside it, lexically.
func Within(root, path string) bool {
	if root == "" || path == "" {
		return false
	}
	rel, err := filepath.Rel(filepath.Clean(root), filepath.Clean(path))
	return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator)) && !filepath.IsAbs(rel)
}
