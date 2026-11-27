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
	DiscardReasons           []string
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
		if _, err = os.Lstat(member.Path); errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err = m.validate(ctx, member); err != nil {
			m.mu.Lock()
			repo, ok := m.repos[member.RepositoryID]
			verified := ok && m.common[repo.ID] == member.CommonDir
			m.mu.Unlock()
			if verified && member.Path == filepath.Join(c.Root, member.Name) && missingWorktreeLink(member.Path, member.CommonDir) {
				// A pruned registration leaves ordinary files in the owned
				// workspace. Both cases require explicit discard consent.
				p.Dirty = append(p.Dirty, member.Name)
				p.DiscardReasons = append(p.DiscardReasons, member.Name+": files remain after Git worktree registration was removed.")
				continue
			}
			return p, fmt.Errorf("%s: %w", member.Name, err)
		}
		changed, err := m.git(ctx, member.Path, "status", "--porcelain", "--untracked-files=normal", "--ignore-submodules=none")
		if err != nil {
			return p, err
		}
		// Apply the generated-directory exclusions only to ignored files. Tracked changes
		// and untracked dependencies still appear in the ordinary status above.
		args := []string{"ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "--no-empty-directory", "--", "."}
		for _, dir := range []string{"node_modules", "build", "dist", "target", "coverage", ".next", ".cache"} {
			args = append(args, ":(exclude,glob)**/"+dir+"/**")
		}
		ignored, err := m.git(ctx, member.Path, args...)
		if err != nil {
			return p, err
		}
		var reason string
		switch {
		case changed != "" && ignored != "":
			reason = "uncommitted or untracked files and Git-ignored files"
		case changed != "":
			reason = "uncommitted or untracked files"
		case ignored != "":
			reason = "Git-ignored files"
		default:
			if reason, err = m.unsavedHistory(ctx, member.Path); err != nil {
				return p, err
			}
		}
		if reason != "" {
			p.Dirty = append(p.Dirty, member.Name)
			p.DiscardReasons = append(p.DiscardReasons, member.Name+": "+reason+".")
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

// inProgressMarkers are the Git administrative paths that exist while an
// operation that stopped part-way (rebase, merge, cherry-pick, revert, bisect)
// waits for the user. Its state lives in the worktree's own Git directory, so
// removing the worktree throws the operation away.
var inProgressMarkers = []struct{ path, operation string }{
	{"rebase-merge", "rebase"}, {"rebase-apply", "rebase"}, {"MERGE_HEAD", "merge"},
	{"CHERRY_PICK_HEAD", "cherry-pick"}, {"REVERT_HEAD", "revert"}, {"sequencer", "cherry-pick or revert"},
	{"BISECT_LOG", "bisect"},
}

// unsavedHistory names what removing the checkout at path would lose that
// `git status` does not show: commits on no branch, or an operation in
// progress. It returns "" when HEAD is on a branch and nothing is in progress.
// Removing a worktree deletes its HEAD reflog, so a commit only HEAD reaches
// becomes garbage.
func (m *Manager) unsavedHistory(ctx context.Context, path string) (string, error) {
	args := []string{"rev-parse"}
	for _, marker := range inProgressMarkers {
		args = append(args, "--git-path", marker.path)
	}
	out, err := m.git(ctx, path, args...)
	if err != nil {
		return "", err
	}
	paths := strings.Split(out, "\n")
	if len(paths) != len(inProgressMarkers) {
		return "", fmt.Errorf("git rev-parse --git-path returned %d paths for %d markers", len(paths), len(inProgressMarkers))
	}
	for i, marker := range paths {
		if !filepath.IsAbs(marker) {
			marker = filepath.Join(path, marker)
		}
		if _, err := os.Lstat(marker); err == nil {
			return "a " + inProgressMarkers[i].operation + " is in progress", nil
		} else if !errors.Is(err, os.ErrNotExist) {
			return "", err
		}
	}
	// A branch always contains the HEAD that names it, so a symbolic HEAD
	// leaves nothing unreachable. A detached HEAD is treated as unsaved even
	// when a ref happens to contain it today, as RemoveRepositoryMember does.
	if _, err := m.git(ctx, path, "symbolic-ref", "-q", "HEAD"); err != nil {
		return "HEAD is detached; its commits may be on no branch", nil
	}
	return "", nil
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
func (m *Manager) BeginDeletion(ctx context.Context, id, name string, discard, stop, keepArtifacts bool, active func(string) bool) (Collection, error) {
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
		return Collection{}, errors.New("workspace contains changed, untracked or ignored files, commits on no branch, or an unfinished rebase or merge; explicitly choose discard to delete them")
	}
	if c.State == "deleting" && c.KeepArtifacts != keepArtifacts {
		return Collection{}, errors.New("artifact choice cannot change once workspace deletion has begun")
	}
	c.KeepArtifacts = keepArtifacts
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
		if discard && member.Path == filepath.Join(c.Root, member.Name) && missingWorktreeLink(member.Path, member.CommonDir) {
			registered, err := m.registeredWorktree(ctx, repo, member.Path)
			if err != nil || !missingWorktreeLink(member.Path, member.CommonDir) {
				return fail(errors.New("damaged worktree changed during deletion; retry to check it again"))
			}
			if !registered {
				// Only the workspace files remain for RemoveAll below.
				continue
			}
			// Git can recreate a missing link from its registration. It can
			// emit a diagnostic for the broken link even after repairing it.
			_, repairErr := m.git(ctx, repo.Path, "worktree", "repair", "--", member.Path)
			if err := m.validate(ctx, member); err != nil {
				return fail(errors.Join(errors.New("could not repair the member's missing Git link"), repairErr, err))
			}
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

// A missing link can be recreated. Existing Git metadata, symlinks and
// non-directories are left alone even if the path was once registered. A
// .git file still counts as missing when it names a removed administrative
// directory of this repository (<common>/worktrees/<name>).
func missingWorktreeLink(path, commonDir string) bool {
	canonical, err := filepath.EvalSymlinks(path)
	if err != nil || canonical != path {
		return false
	}
	info, err := os.Stat(path)
	if err != nil || !info.IsDir() {
		return false
	}
	// A Git administrative directory (including a bare repository) can
	// itself occupy this path without a .git entry. Its HEAD and object
	// store or commondir must never be treated as disposable checkout files.
	missing := func(name string) bool {
		_, err := os.Lstat(filepath.Join(path, name))
		return errors.Is(err, os.ErrNotExist)
	}
	if !missing("HEAD") && (!missing("objects") || !missing("commondir")) {
		return false
	}
	return missing(".git") || danglingGitFile(filepath.Join(path, ".git"), commonDir)
}

// danglingGitFile reports whether link is a regular .git file whose gitdir is
// a removed <commonDir>/worktrees/<name>. A file pointing anywhere else, or at
// a directory that still exists, is somebody's live metadata.
func danglingGitFile(link, commonDir string) bool {
	info, err := os.Lstat(link)
	if err != nil || !info.Mode().IsRegular() || info.Size() > 4096 {
		return false
	}
	data, err := os.ReadFile(link)
	if err != nil {
		return false
	}
	target, ok := strings.CutPrefix(strings.TrimSpace(string(data)), "gitdir: ")
	if !ok || target == "" || strings.ContainsAny(target, "\n\r") {
		return false
	}
	if !filepath.IsAbs(target) {
		target = filepath.Join(filepath.Dir(link), target)
	}
	target = filepath.Clean(target)
	if filepath.Base(filepath.Dir(target)) != "worktrees" || filepath.Base(target) == "worktrees" {
		return false
	}
	common, err := filepath.EvalSymlinks(filepath.Dir(filepath.Dir(target)))
	if err != nil || common != commonDir {
		return false
	}
	_, err = os.Lstat(target)
	return errors.Is(err, os.ErrNotExist)
}

func (m *Manager) registeredWorktree(ctx context.Context, repo Repository, path string) (bool, error) {
	field := worktreeField{want: "worktree " + path}
	err := m.gitOutput(ctx, repo.Path, &field, "worktree", "list", "--porcelain", "-z")
	return field.found, err
}

// Match a complete NUL-terminated porcelain field across arbitrary writes.
// A lock reason or a long list of unrelated worktrees uses no extra memory.
type worktreeField struct {
	want    string
	matched int
	found   bool
}

func (f *worktreeField) Write(data []byte) (int, error) {
	for _, ch := range data {
		if ch == 0 {
			f.found = f.found || f.matched == len(f.want)
			f.matched = 0
		} else if f.matched >= 0 && f.matched < len(f.want) && f.want[f.matched] == ch {
			f.matched++
		} else {
			f.matched = -1
		}
	}
	return len(data), nil
}

func (m *Manager) removeRegisteredWorktree(ctx context.Context, repo Repository, member Member, discard bool) error {
	registered, err := m.registeredWorktree(ctx, repo, member.Path)
	if err != nil {
		return err
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
