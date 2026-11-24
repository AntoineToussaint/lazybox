package collections

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
)

// RemoveRepositoryMember removes a clean worktree from a ready workspace and
// retains its branch and source repository. The manager lock fences attachment.
// If provided, forgetRuntime clears checkout-specific runtime records after
// Git removes the checkout and before the workspace record is committed.
func (m *Manager) RemoveRepositoryMember(ctx context.Context, id, name string, active func(string) bool, forgetRuntime func() error) (Collection, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return Collection{}, os.ErrNotExist
	}
	if m.closed || c.State != "ready" || c.OwnershipUncertain {
		return Collection{}, errors.New("workspace must be ready with confirmed process ownership")
	}
	i := slices.IndexFunc(c.Members, func(member Member) bool { return member.Name == name })
	if i < 0 {
		return Collection{}, fmt.Errorf("worktree %q: %w", name, os.ErrNotExist)
	}
	for _, owner := range c.OwnerExecutionIDs {
		if active(owner) {
			return Collection{}, errors.New("end active workspace sessions and commands before removing a repository")
		}
	}
	root, err := filepath.EvalSymlinks(c.Root)
	if err != nil || root != c.Root || !validRoot(m.root, c) {
		return Collection{}, errors.New("workspace root moved or replaced; cleanup refused")
	}
	member := c.Members[i]
	if _, err := os.Lstat(member.Path); !errors.Is(err, os.ErrNotExist) {
		if err = m.validate(ctx, member); err != nil {
			return Collection{}, err
		}
		branch, err := m.git(ctx, member.Path, "symbolic-ref", "HEAD")
		if err != nil || !strings.HasPrefix(branch, "refs/heads/") {
			return Collection{}, errors.New("switch to a local branch before removing this worktree")
		}
		unsaved, err := m.unsavedHistory(ctx, member.Path)
		if err != nil {
			return Collection{}, err
		}
		if unsaved != "" {
			return Collection{}, fmt.Errorf("%s; finish or abort it before removing this worktree", unsaved)
		}
		dirty, err := m.git(ctx, member.Path, "status", "--porcelain", "--untracked-files=all", "--ignored", "--ignore-submodules=none")
		if err != nil {
			return Collection{}, err
		}
		if dirty != "" {
			return Collection{}, errors.New("worktree contains changed, untracked or ignored files; move or commit them before removing this repository")
		}
	}
	if err := m.removeWorktree(ctx, member, false); err != nil {
		return Collection{}, err
	}
	if forgetRuntime != nil {
		if err := forgetRuntime(); err != nil {
			return Collection{}, err
		}
	}
	c = clone(c)
	c.Members = slices.Delete(c.Members, i, i+1)
	if err := m.save(c); err != nil {
		return Collection{}, err
	}
	return c, nil
}
