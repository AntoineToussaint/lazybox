package collections

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
)

type DeletionPreview struct {
	Active                   []string
	Dirty                    []string
	FilesOutsideRepositories bool
	Blocked                  string
}

func (m *Manager) deletionPreview(ctx context.Context, c Collection, active func(string) bool) (DeletionPreview, error) {
	p := DeletionPreview{}
	if c.OwnershipUncertain {
		p.Blocked = "Previous process ownership is unconfirmed; reconcile the workspace before deleting it."
		return p, nil
	}
	for _, id := range c.OwnerExecutionIDs {
		if active(id) {
			p.Active = append(p.Active, id)
		}
	}
	root, err := filepath.EvalSymlinks(c.Root)
	if errors.Is(err, os.ErrNotExist) && c.State == "deleting" {
		return p, nil
	}
	if err != nil || root != c.Root || !validRoot(m.root, c) {
		return p, errors.New("workspace root moved or replaced; cleanup refused")
	}
	names := map[string]bool{}
	for _, member := range c.Members {
		names[member.Name] = true
		if _, err = os.Lstat(member.Path); errors.Is(err, os.ErrNotExist) && c.State == "deleting" {
			continue
		}
		if err = m.validate(ctx, member); err != nil {
			return p, fmt.Errorf("%s: %w", member.Name, err)
		}
		// Include ignored build artifacts: they are user bytes too.
		dirty, err := m.git(ctx, member.Path, "status", "--porcelain", "--untracked-files=normal", "--ignored")
		if err != nil {
			return p, err
		}
		if dirty != "" {
			p.Dirty = append(p.Dirty, member.Name)
		}
	}
	entries, err := os.ReadDir(c.Root)
	if err != nil {
		return p, err
	}
	for _, entry := range entries {
		if !names[entry.Name()] {
			p.FilesOutsideRepositories = true
		}
	}
	return p, nil
}
func (m *Manager) PreviewDeletion(ctx context.Context, id string, active func(string) bool) (DeletionPreview, error) {
	c, err := m.Get(id)
	if err != nil {
		return DeletionPreview{}, err
	}
	if c.State == "deleted" {
		return DeletionPreview{}, nil
	}
	return m.deletionPreview(ctx, c, active)
}

// BeginDeletion durably fences new attachments before any process is stopped.
func (m *Manager) BeginDeletion(ctx context.Context, id, name string, discard, stop bool, active func(string) bool) (Collection, error) {
	c, err := m.Get(id)
	if err != nil {
		return Collection{}, err
	}
	if name != c.Name {
		return Collection{}, errors.New("workspace name confirmation does not match")
	}
	if c.State == "deleted" {
		return c, nil
	}
	p, err := m.deletionPreview(ctx, c, active)
	if err != nil {
		return Collection{}, err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed || m.finishingDeletion[id] || !reflect.DeepEqual(c, clone(m.items[id])) {
		return Collection{}, errors.New("workspace changed while checking deletion; retry")
	}
	if p.Blocked != "" {
		return Collection{}, errors.New(p.Blocked)
	}
	if len(p.Active) > 0 && !stop {
		return Collection{}, errors.New("explicitly stop workspace sessions and commands before deletion")
	}
	if (len(p.Dirty) > 0 || p.FilesOutsideRepositories) && !discard {
		return Collection{}, errors.New("workspace contains changed, untracked or ignored files; explicitly choose discard to delete them")
	}
	c.State = "deleting"
	c.Error = ""
	if err = m.save(c); err != nil {
		return Collection{}, err
	}
	return clone(c), nil
}

// FinishDeletion removes Git registrations through Git. Branches and saved
// conversation evidence are retained; credentials and source clones are untouched.
func (m *Manager) FinishDeletion(ctx context.Context, id string, discard bool, active func(string) bool) (Collection, error) {
	m.mu.Lock()
	c, ok := m.items[id]
	if !ok {
		m.mu.Unlock()
		return Collection{}, os.ErrNotExist
	}
	c = clone(c)
	if c.State == "deleted" {
		m.mu.Unlock()
		return c, nil
	}
	if m.closed || c.State != "deleting" || m.finishingDeletion[id] {
		m.mu.Unlock()
		return Collection{}, errors.New("workspace deletion has not begun or cleanup is already running")
	}
	if m.finishingDeletion == nil {
		m.finishingDeletion = make(map[string]bool)
	}
	m.finishingDeletion[id] = true
	m.deletionWorkers.Add(1)
	m.mu.Unlock()
	defer func() {
		m.mu.Lock()
		delete(m.finishingDeletion, id)
		m.mu.Unlock()
		m.deletionWorkers.Done()
	}()
	fail := func(err error) (Collection, error) {
		m.mu.Lock()
		defer m.mu.Unlock()
		current := m.items[id]
		current.Error = err.Error()
		return clone(current), errors.Join(err, m.save(current))
	}
	p, err := m.deletionPreview(ctx, c, active)
	if err != nil {
		return fail(err)
	}
	if p.Blocked != "" || len(p.Active) > 0 {
		return fail(errors.New("workspace still has active or unconfirmed processes"))
	}
	if (len(p.Dirty) > 0 || p.FilesOutsideRepositories) && !discard {
		return fail(errors.New("workspace files changed during shutdown; review and explicitly discard before retrying"))
	}
	// Persist release before removing files so a crash cannot leave stale
	// owners attached to an already-removed checkout.
	m.mu.Lock()
	current := m.items[id]
	current.OwnerExecutionIDs = nil
	current.syncOwners()
	err = m.save(current)
	m.mu.Unlock()
	if err != nil {
		return Collection{}, err
	}
	for _, member := range c.Members {
		m.mu.Lock()
		repo, ok := m.repos[member.RepositoryID]
		verified := ok && m.common[repo.ID] == member.CommonDir
		m.mu.Unlock()
		if !verified {
			return fail(errors.New("source repository changed; cleanup refused"))
		}
		if err = m.removeRegisteredWorktree(ctx, repo, member, discard); err != nil {
			return fail(err)
		}
	}
	if discard {
		err = os.RemoveAll(c.Root)
	} else {
		err = os.Remove(c.Root)
	}
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return fail(err)
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	c = m.items[id]
	c.State = "deleted"
	c.Error = ""
	if err = m.save(c); err != nil {
		return Collection{}, err
	}
	return clone(c), nil
}

// removeWorktree also accepts an already-removed worktree so callers can retry
// after Git succeeds and saving the workspace record fails.
func (m *Manager) removeWorktree(ctx context.Context, member Member, discard bool) error {
	repo, ok := m.repos[member.RepositoryID]
	if !ok || m.common[repo.ID] != member.CommonDir {
		return errors.New("source repository changed; cleanup refused")
	}
	return m.removeRegisteredWorktree(ctx, repo, member, discard)
}

func (m *Manager) removeRegisteredWorktree(ctx context.Context, repo Repository, member Member, discard bool) error {
	listing, err := m.git(ctx, repo.Path, "worktree", "list", "--porcelain")
	if err != nil {
		return err
	}
	registered := false
	for _, line := range strings.Split(listing, "\n") {
		if line == "worktree "+member.Path {
			registered = true
		}
	}
	if !registered {
		if _, err = os.Lstat(member.Path); !errors.Is(err, os.ErrNotExist) {
			return errors.New("worktree registration disappeared but its path remains")
		}
		return nil
	}
	args := []string{"worktree", "remove"}
	if discard {
		args = append(args, "--force")
	}
	args = append(args, "--", member.Path)
	if _, err = m.git(ctx, repo.Path, args...); err != nil {
		return err
	}
	return nil
}
