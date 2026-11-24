package collections

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// RegisterRepository enrolls a verified local Git repository without modifying
// its checkout, branches, authentication, or Git configuration.
func (m *Manager) RegisterRepository(ctx context.Context, r Repository) (Repository, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.registerRepository(ctx, r)
}
func (m *Manager) registerRepository(ctx context.Context, r Repository) (Repository, error) {
	if m.closed {
		return Repository{}, errors.New("collection manager closed")
	}
	if !component.MatchString(r.ID) || !filepath.IsAbs(r.Path) || r.Name != "" && !component.MatchString(r.Name) {
		return Repository{}, errors.New("safe repository identity, name, and absolute path required")
	}
	var err error
	r.Path, err = filepath.EvalSymlinks(r.Path)
	if err != nil {
		return Repository{}, err
	}
	r = m.resolveRepositorySource(ctx, r)
	common, err := m.git(ctx, r.Path, "rev-parse", "--path-format=absolute", "--git-common-dir")
	if err != nil {
		return Repository{}, err
	}
	common, err = filepath.EvalSymlinks(common)
	if err != nil {
		return Repository{}, err
	}
	// A hidden entry is the source of the workspaces made from it: their
	// members name its Git directory. Registering a different clone under the
	// same ID would strand them, so that clone gets an ID of its own.
	if old, ok := m.repos[r.ID]; ok && old.Hidden && m.common[old.ID] != common {
		r.ID += "-" + strings.TrimPrefix(gitidentity.ID(common), "local-")[:12]
	}
	// The home is the person's setting, not part of a registration: one
	// survives registering the same repository again.
	if old, ok := m.repos[r.ID]; ok && r.Home == "" {
		r.Home = old.Home
	}
	if old, ok := m.repos[r.ID]; ok && !old.Hidden {
		if old == r {
			return old, nil
		}
		if old.Unavailable != "" {
			return Repository{}, fmt.Errorf("repository %s is unavailable (%s); restore its folder and restart the Runner, or remove it first", old.ID, old.Unavailable)
		}
		return Repository{}, errors.New("repository identity already has a different registration")
	}
	if len(m.repos) >= 512 {
		return Repository{}, errors.New("repository library capacity reached")
	}
	identity := gitidentity.Repository(common, r.SourceURL)
	for _, old := range m.repositories() {
		if m.identity(old.ID) == identity {
			return old, nil
		}
	}
	list := []Repository{r}
	for _, old := range m.repos {
		if old.ID == r.ID {
			continue
		}
		list = append(list, old)
	}
	if err = m.saveRepositories(list); err != nil {
		return Repository{}, err
	}
	m.repos[r.ID] = r
	m.common[r.ID] = common
	return r, nil
}

// ReplaceRepository makes r the library's entry for its repository in place of
// the current one, a fresh copy of a repository whose history changed. The
// entries it replaces are hidden, never removed: workspaces made from them
// keep their worktrees and still find their source copy (as RemoveRepository
// keeps them). Every other workspace and new one uses r. It answers the
// registrations it hid.
func (m *Manager) ReplaceRepository(ctx context.Context, r Repository) (Repository, []Repository, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return Repository{}, nil, errors.New("collection manager closed")
	}
	if _, ok := m.repos[r.ID]; ok {
		return Repository{}, nil, errors.New("a replacement needs a registration of its own")
	}
	name := gitidentity.GitHubName(r.SourceURL)
	if name == "" {
		return Repository{}, nil, errors.New("only a GitHub repository's copy is replaced")
	}
	identity := "github:" + name
	var replaced []Repository
	previous := map[string]Repository{}
	for _, old := range m.repositories() {
		if m.identity(old.ID) != identity {
			continue
		}
		previous[old.ID] = old
		hidden := old
		hidden.Hidden = true
		m.repos[old.ID] = hidden
		replaced = append(replaced, old)
	}
	// Registering saves the library with the old entries hidden, in one write.
	added, err := m.registerRepository(ctx, r)
	if err != nil {
		for id, old := range previous {
			m.repos[id] = old
		}
		return Repository{}, nil, err
	}
	return added, replaced, nil
}

// RepositoryUsers are the workspaces (not deleted) with a member made from
// repository id.
func (m *Manager) RepositoryUsers(id string) []Collection {
	m.mu.Lock()
	defer m.mu.Unlock()
	var out []Collection
	for _, c := range m.items {
		if c.State == "deleted" {
			continue
		}
		for _, member := range c.Members {
			if member.RepositoryID == id {
				out = append(out, clone(c))
				break
			}
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Name < out[j].Name })
	return out
}

// AddLocalRepository registers an existing checkout using its canonical Git
// provider identity when available, with local Git identity as a fallback.
// Independent clones and worktrees of the same GitHub repo return one entry.
func (m *Manager) AddLocalRepository(ctx context.Context, path, name string) (Repository, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return Repository{}, errors.New("workspace manager closed")
	}
	if !filepath.IsAbs(path) {
		return Repository{}, errors.New("choose an absolute repository path on the Runner")
	}
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil {
		return Repository{}, errors.New("repository path does not exist on the Runner")
	}
	bare, err := m.git(ctx, canonical, "rev-parse", "--is-bare-repository")
	if err != nil {
		return Repository{}, errors.New("path is not an accessible Git repository on the Runner")
	}
	if bare != "true" {
		top, err := m.git(ctx, canonical, "rev-parse", "--show-toplevel")
		if err != nil {
			return Repository{}, errors.New("choose a Git checkout root, not its .git directory")
		}
		top, err = filepath.EvalSymlinks(top)
		if err != nil || top != canonical {
			return Repository{}, errors.New("choose the repository root, not a subfolder")
		}
	}
	common, err := m.git(ctx, canonical, "rev-parse", "--path-format=absolute", "--git-common-dir")
	if err != nil {
		return Repository{}, errors.New("cannot resolve the repository identity")
	}
	common, err = filepath.EvalSymlinks(common)
	if err != nil {
		return Repository{}, err
	}
	source := m.resolveRepositorySource(ctx, Repository{Path: canonical})
	identity := gitidentity.Repository(common, source.SourceURL)
	for _, existing := range m.repositories() {
		if m.identity(existing.ID) == identity {
			return existing, nil
		}
	}
	if _, err = m.git(ctx, canonical, "rev-parse", "--verify", "HEAD^{commit}"); err != nil {
		return Repository{}, errors.New("repository needs an initial commit before creating workspaces")
	}
	if name == "" {
		name = strings.TrimSuffix(filepath.Base(canonical), ".git")
		if fullName := gitidentity.GitHubName(source.SourceURL); fullName != "" {
			_, name, _ = strings.Cut(fullName, "/")
		}
	}
	if !component.MatchString(name) {
		return Repository{}, errors.New("enter a repository name using letters, numbers, dots, hyphens or underscores")
	}
	id := gitidentity.ID(common)
	if source.SourceURL != "" {
		id = "github-" + strings.TrimPrefix(gitidentity.ID(identity), "local-")
	}
	return m.registerRepository(ctx, Repository{ID: id, Name: name, Path: canonical, SourceURL: source.SourceURL})
}

// RepositoryIdentity exposes the checkout-independent identity of any library
// entry, including configured aliases and GitHub imports.
// Repository returns one registered repository by ID, hidden historical
// registrations included: the source checkout behind a workspace member.
func (m *Manager) Repository(id string) (Repository, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	r, ok := m.repos[id]
	return r, ok
}

// identity is registration id's checkout-independent identity. A repository
// whose Git directory could not be read at startup (Unavailable) and has no
// GitHub origin has none to share, so it is only ever itself. The caller
// holds m.mu.
func (m *Manager) identity(id string) string {
	r := m.repos[id]
	if m.common[id] == "" && gitidentity.GitHubName(r.SourceURL) == "" {
		return "unavailable:" + id
	}
	return gitidentity.Repository(m.common[id], r.SourceURL)
}

func (m *Manager) RepositoryIdentity(id string) string {
	m.mu.Lock()
	defer m.mu.Unlock()
	if common := m.common[id]; common != "" {
		return gitidentity.Repository(common, m.repos[id].SourceURL)
	}
	return ""
}

// resolveRepositorySource reads origin metadata only; it never contacts GitHub
// or changes checkout configuration. Existing provider metadata is authoritative.
func (m *Manager) resolveRepositorySource(ctx context.Context, r Repository) Repository {
	remote := r.SourceURL
	if remote == "" {
		remote, _ = m.git(ctx, r.Path, "config", "--get", "remote.origin.url")
	}
	if name := gitidentity.GitHubName(remote); name != "" {
		r.SourceURL = "https://github.com/" + name
	}
	return r
}

// RemoveRepository removes a library entry, retaining source metadata needed by
// existing worktrees. It never deletes or changes Git files.
func (m *Manager) RemoveRepository(id string) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return errors.New("workspace manager closed")
	}
	repo, ok := m.repos[id]
	if !ok {
		return os.ErrNotExist
	}
	if repo.Hidden {
		return nil
	}
	identity := m.identity(id)
	list := []Repository{}
	for _, r := range m.repos {
		if m.identity(r.ID) == identity {
			r.Hidden = true
		}
		list = append(list, r)
	}
	if err := m.saveRepositories(list); err != nil {
		return err
	}
	for _, r := range list {
		m.repos[r.ID] = r
	}
	return nil
}
func (m *Manager) saveRepositories(list []Repository) error {
	sort.Slice(list, func(i, j int) bool { return list[i].ID < list[j].ID })
	data, err := json.Marshal(list)
	if err != nil {
		return err
	}
	dir := filepath.Join(m.root, ".lazybox-collections")
	f, err := os.CreateTemp(dir, ".library-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	err = errors.Join(err, f.Close())
	if err != nil {
		return err
	}
	if err = os.Rename(f.Name(), filepath.Join(dir, "repositories.json")); err != nil {
		return err
	}
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer d.Close()
	if err = d.Sync(); err != nil {
		return err
	}

	return nil
}
