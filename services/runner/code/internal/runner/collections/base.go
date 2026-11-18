package collections

import (
	"context"
	"errors"
	"os"
	"strconv"
	"strings"
	"time"
)

// A workspace branch with no explicit base starts from origin's default
// branch. The Runner fetches it first (gitpublish.RefreshDefault, called by
// the Bench before it creates or extends a workspace); this file only reads
// what the repository already has, so it never touches the network and works
// offline from the last fetch:
//
//  1. refs/remotes/origin/HEAD, which the fetch (or a normal clone) records;
//  2. in a source copy (bare clone), the branch its HEAD names, from
//     refs/remotes/origin/<branch> when fetched, else the copy's own branch,
//     which is origin's as of the clone;
//  3. HEAD, only for a repository with neither: one without origin.
//
// An explicit base (a branch, a commit, an orchestrator's parent branch) is
// used as given. A branch the person chose to start from ("Start from",
// runner/start_from.go) is fetched first as well and arrives here as
// refs/remotes/origin/<branch>; BranchBase is its offline fallback.

// Base is where a new workspace branch starts: the default when none is
// chosen (DefaultBase), or a chosen branch's last fetch (BranchBase).
type Base struct {
	Ref    string    // the ref resolved (refs/remotes/origin/main…)
	Branch string    // the branch's name (origin's default for DefaultBase), empty when unknown
	Commit string    // the commit it names
	AsOf   time.Time // when it was last updated: the last fetch, else its commit's time
}

// DefaultBase resolves the default base of a registered repository without
// contacting origin.
func (m *Manager) DefaultBase(ctx context.Context, repositoryID string) (Base, error) {
	m.mu.Lock()
	r, ok := m.repos[repositoryID]
	m.mu.Unlock()
	if !ok {
		return Base{}, os.ErrNotExist
	}
	base, err := m.resolveDefaultBase(ctx, r.Path)
	if err != nil {
		return Base{}, err
	}
	return m.stamp(ctx, r.Path, base), nil
}

// BranchBase resolves a branch the person chose to start from as last
// fetched (refs/remotes/origin/<branch>), without contacting origin: the
// offline fallback for a chosen branch, as DefaultBase is for the default.
// A repository without that remote-tracking branch has nothing to fall back
// on (os.ErrNotExist); one without origin uses its own branch.
func (m *Manager) BranchBase(ctx context.Context, repositoryID, branch string) (Base, error) {
	m.mu.Lock()
	r, ok := m.repos[repositoryID]
	m.mu.Unlock()
	if !ok || branch == "" || strings.HasPrefix(branch, "-") {
		return Base{}, os.ErrNotExist
	}
	if _, err := m.git(ctx, r.Path, "check-ref-format", "refs/heads/"+branch); err != nil {
		return Base{}, os.ErrNotExist
	}
	ref := "refs/remotes/origin/" + branch
	if remote, _ := m.git(ctx, r.Path, "config", "--get", "remote.origin.url"); remote == "" {
		ref = "refs/heads/" + branch
	}
	commit, err := m.git(ctx, r.Path, "rev-parse", "--verify", "--end-of-options", ref+"^{commit}")
	if err != nil {
		return Base{}, os.ErrNotExist
	}
	return m.stamp(ctx, r.Path, Base{Ref: ref, Branch: branch, Commit: commit}), nil
}

// resolveDefaultBase turns an unresolvable default into an actionable error.
func (m *Manager) resolveDefaultBase(ctx context.Context, path string) (Base, error) {
	base := m.defaultBase(ctx, path)
	commit, err := m.git(ctx, path, "rev-parse", "--verify", "--end-of-options", base.Ref+"^{commit}")
	if err == nil {
		base.Commit = commit
		return base, nil
	}
	refs, refsErr := m.git(ctx, path, "for-each-ref", "--count=1", "--format=%(refname)")
	if refsErr != nil {
		return Base{}, err
	}
	if refs == "" {
		return Base{}, errors.New("no starting commit is available. If this repository is empty, create an initial commit and push it if there is a remote, then retry. If it already has commits, check access to its remote and retry")
	}
	return Base{}, errors.New("no default starting commit is available. Use Start from to choose an existing branch")
}

// stamp records when the base was last updated.
func (m *Manager) stamp(ctx context.Context, path string, base Base) Base {
	if gitDir, err := m.git(ctx, path, "rev-parse", "--path-format=absolute", "--git-common-dir"); err == nil {
		if info, err := os.Stat(gitDir + "/FETCH_HEAD"); err == nil {
			base.AsOf = info.ModTime()
		}
	}
	if base.AsOf.IsZero() {
		if seconds, err := m.git(ctx, path, "log", "-1", "--format=%ct", base.Commit); err == nil {
			if n, err := strconv.ParseInt(seconds, 10, 64); err == nil {
				base.AsOf = time.Unix(n, 0)
			}
		}
	}
	return base
}

func (m *Manager) defaultBase(ctx context.Context, repo string) Base {
	resolves := func(ref string) bool {
		_, err := m.git(ctx, repo, "rev-parse", "--verify", "--quiet", "--end-of-options", ref+"^{commit}")
		return err == nil
	}
	if ref, err := m.git(ctx, repo, "symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"); err == nil && strings.HasPrefix(ref, "refs/remotes/origin/") && resolves(ref) {
		return Base{Ref: ref, Branch: strings.TrimPrefix(ref, "refs/remotes/origin/")}
	}
	if bare, _ := m.git(ctx, repo, "rev-parse", "--is-bare-repository"); bare == "true" {
		if head, err := m.git(ctx, repo, "symbolic-ref", "--quiet", "HEAD"); err == nil && strings.HasPrefix(head, "refs/heads/") {
			branch := strings.TrimPrefix(head, "refs/heads/")
			if remote := "refs/remotes/origin/" + branch; resolves(remote) {
				return Base{Ref: remote, Branch: branch}
			}
			if resolves(head) {
				return Base{Ref: head, Branch: branch}
			}
		}
	}
	return Base{Ref: "HEAD"}
}
