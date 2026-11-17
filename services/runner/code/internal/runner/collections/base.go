package collections

import (
	"context"
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
// used as given.

// Base is where a new workspace branch starts when none is chosen.
type Base struct {
	Ref    string    // the ref resolved (refs/remotes/origin/main…)
	Branch string    // origin's default branch name, empty when unknown
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
	base := m.defaultBase(ctx, r.Path)
	commit, err := m.git(ctx, r.Path, "rev-parse", "--verify", "--end-of-options", base.Ref+"^{commit}")
	if err != nil {
		return Base{}, err
	}
	base.Commit = commit
	if gitDir, err := m.git(ctx, r.Path, "rev-parse", "--path-format=absolute", "--git-common-dir"); err == nil {
		if info, err := os.Stat(gitDir + "/FETCH_HEAD"); err == nil {
			base.AsOf = info.ModTime()
		}
	}
	if base.AsOf.IsZero() {
		if seconds, err := m.git(ctx, r.Path, "log", "-1", "--format=%ct", commit); err == nil {
			if n, err := strconv.ParseInt(seconds, 10, 64); err == nil {
				base.AsOf = time.Unix(n, 0)
			}
		}
	}
	return base, nil
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
