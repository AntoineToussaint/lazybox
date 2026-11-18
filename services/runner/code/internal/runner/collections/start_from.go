package collections

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
)

// The branch each repository's new workspaces start from ("Start from" in New
// task, runner/start_from.go): when the person chooses a branch other than
// origin's default, the next workspace of that repository starts there
// without asking. Kept beside the records, in
// .lazybox-collections/start-from.json, by repository identity (so aliases of
// one repository share it), and it survives a Runner restart. Like the
// arrangement it is the person's convenience: an unreadable file is none.

const startsFile = "start-from.json"

// recentStarts is how many branches a repository remembers having started from.
const recentStarts = 8

// maxStarts bounds the file like the library bounds repositories.
const maxStarts = 512

// Start is one repository's remembered choice.
type Start struct {
	// Branch is the branch to start from; "" is origin's default branch.
	Branch string `json:"branch,omitempty"`
	// Recent are the branches other than the default chosen lately, newest first.
	Recent []string `json:"recent,omitempty"`
}

func (m *Manager) loadStarts() {
	m.starts = map[string]Start{}
	data, err := os.ReadFile(filepath.Join(m.root, ".lazybox-collections", startsFile))
	if err != nil || len(data) > 1<<20 {
		return
	}
	var starts map[string]Start
	if json.Unmarshal(data, &starts) != nil || len(starts) > maxStarts {
		return
	}
	for identity, s := range starts {
		if len(s.Recent) > recentStarts {
			s.Recent = s.Recent[:recentStarts]
		}
		m.starts[identity] = s
	}
}

// identityLocked is a registered repository's checkout-independent identity,
// or "". The caller holds m.mu.
func (m *Manager) identityLocked(id string) string {
	r, ok := m.repos[id]
	if !ok || m.common[id] == "" {
		return ""
	}
	return gitidentity.Repository(m.common[id], r.SourceURL)
}

// StartFrom is the branch a repository's new workspaces start from when the
// person does not choose one ("" for origin's default branch), and the
// branches chosen lately, newest first.
func (m *Manager) StartFrom(id string) Start {
	m.mu.Lock()
	defer m.mu.Unlock()
	s := m.starts[m.identityLocked(id)]
	return Start{Branch: s.Branch, Recent: slices.Clone(s.Recent)}
}

// RememberStartFrom records the branch the person chose to start a workspace
// of this repository from: the next one starts there without asking.
// isDefault (the branch is origin's default) forgets the choice, so the
// repository follows origin's default again.
func (m *Manager) RememberStartFrom(id, branch string, isDefault bool) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.closed {
		return errors.New("workspace manager closed")
	}
	identity := m.identityLocked(id)
	if identity == "" {
		return os.ErrNotExist
	}
	old := m.starts[identity]
	next := Start{Recent: old.Recent}
	if branch != "" && !isDefault {
		next.Branch = branch
		recent := append([]string{branch}, slices.DeleteFunc(slices.Clone(old.Recent), func(b string) bool { return b == branch })...)
		next.Recent = recent[:min(len(recent), recentStarts)]
	}
	if next.Branch == old.Branch && slices.Equal(next.Recent, old.Recent) {
		return nil
	}
	starts := make(map[string]Start, len(m.starts)+1)
	for k, v := range m.starts {
		starts[k] = v
	}
	if next.Branch == "" && len(next.Recent) == 0 {
		delete(starts, identity)
	} else {
		starts[identity] = next
	}
	if len(starts) > maxStarts {
		return errors.New("too many remembered start branches")
	}
	if err := m.saveStarts(starts); err != nil {
		return err
	}
	m.starts = starts
	return nil
}

// KnownBranch is a branch of origin as the repository last fetched it.
type KnownBranch struct {
	Name, Commit string
	// CommittedAt is its last commit's time in Unix milliseconds.
	CommittedAt int64
}

// KnownBranches are origin's branches as last fetched, without contacting
// origin: refs/remotes/origin/*, and in a source copy (a bare clone) its own
// branches, which are origin's as of the clone. For a repository without
// origin, its own branches.
func (m *Manager) KnownBranches(ctx context.Context, repositoryID string) []KnownBranch {
	m.mu.Lock()
	r, ok := m.repos[repositoryID]
	m.mu.Unlock()
	if !ok {
		return nil
	}
	patterns := []string{"refs/remotes/origin/"}
	remote, _ := m.git(ctx, r.Path, "config", "--get", "remote.origin.url")
	if bare, _ := m.git(ctx, r.Path, "rev-parse", "--is-bare-repository"); bare == "true" || remote == "" {
		patterns = append(patterns, "refs/heads/")
	}
	out, err := m.git(ctx, r.Path, append([]string{"for-each-ref", "--format=%(refname) %(objectname) %(committerdate:unix)"}, patterns...)...)
	if err != nil {
		return nil
	}
	seen := map[string]bool{}
	var list []KnownBranch
	for _, line := range strings.Split(out, "\n") {
		fields := strings.Fields(line)
		if len(fields) < 2 {
			continue
		}
		name := strings.TrimPrefix(strings.TrimPrefix(fields[0], "refs/remotes/origin/"), "refs/heads/")
		if name == "" || name == "HEAD" || seen[name] {
			continue
		}
		seen[name] = true
		known := KnownBranch{Name: name, Commit: fields[1]}
		if len(fields) > 2 {
			if n, err := strconv.ParseInt(fields[2], 10, 64); err == nil && n > 0 {
				known.CommittedAt = n * 1000
			}
		}
		list = append(list, known)
	}
	return list
}

// CommitTimes maps the commit at the tip of each of the repository's refs to
// its commit time in Unix milliseconds: how recently each known branch moved.
func (m *Manager) CommitTimes(ctx context.Context, repositoryID string) map[string]int64 {
	m.mu.Lock()
	r, ok := m.repos[repositoryID]
	m.mu.Unlock()
	times := map[string]int64{}
	if !ok {
		return times
	}
	out, err := m.git(ctx, r.Path, "for-each-ref", "--format=%(objectname) %(committerdate:unix)", "refs/heads/", "refs/remotes/")
	if err != nil {
		return times
	}
	for _, line := range strings.Split(out, "\n") {
		commit, seconds, ok := strings.Cut(line, " ")
		if n, err := strconv.ParseInt(seconds, 10, 64); ok && err == nil && n > 0 {
			times[commit] = n * 1000
		}
	}
	return times
}

// saveStarts writes the remembered starts atomically. The caller holds m.mu.
func (m *Manager) saveStarts(starts map[string]Start) error {
	data, err := json.Marshal(starts)
	if err != nil {
		return err
	}
	dir := filepath.Join(m.root, ".lazybox-collections")
	f, err := os.CreateTemp(dir, ".start-from-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if err = errors.Join(err, f.Close()); err != nil {
		return err
	}
	if err = os.Rename(f.Name(), filepath.Join(dir, startsFile)); err != nil {
		return err
	}
	d, err := os.Open(dir)
	if err != nil {
		return err
	}
	defer d.Close()
	return d.Sync()
}
