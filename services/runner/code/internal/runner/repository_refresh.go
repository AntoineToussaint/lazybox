package runner

import (
	"context"
	"errors"
	"fmt"
	"log"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sources/github"
)

// A GitHub repository is known to Lazybox by its name (github:owner/name) and,
// once listed or copied, by GitHub's ID for it. When the name comes to mean
// another repository (it was renamed away and a new one took the name), or
// Lazybox's copy was made from a history GitHub no longer has, making a
// workspace from that copy would silently mix two different histories. That
// is refused (RepositoryReplaced); the way forward is to replace Lazybox's
// copy (RefreshRepositoryIdentity), from New task's error or Settings →
// Repositories (WORKSPACES.md, "When a repository's history changes").

// RepositoryReplaced is the refusal to prepare a repository whose identity on
// GitHub is no longer the one Lazybox listed or copied.
type RepositoryReplaced struct {
	// Identity is the catalog's (github:owner/name); Name as people read it.
	Identity, Name string
	// HasCopy: Lazybox has a copy of the old repository.
	HasCopy bool
}

func (e *RepositoryReplaced) Error() string {
	if e.HasCopy {
		return "the repository's history changed on GitHub since Lazybox copied it; replace Lazybox's copy to continue"
	}
	return "on GitHub this name now belongs to another repository than the one Lazybox listed; use the repository GitHub has now to continue"
}

// RepositoryRefresh is what replacing Lazybox's copy of a repository changes,
// or changed.
type RepositoryRefresh struct {
	Identity string
	// Name is the repository's owner/name as GitHub has it now.
	Name string
	// Reason is "replaced" when the name now belongs to another GitHub
	// repository than Lazybox's copy or listing; "" when it is the same
	// repository (a fresh copy still replaces one whose history was rewritten).
	Reason string
	// HasCopy: Lazybox has a copy to replace; Workspaces are made from it.
	HasCopy    bool
	Workspaces []string
	// Replaced: done; RepositoryID is the new copy's registration.
	Replaced     bool
	RepositoryID string
}

// RefreshRepositoryIdentity replaces Lazybox's copy of the GitHub repository
// identity names with a fresh one of the repository GitHub has under that name
// now. Without confirm it only says what that would change. Workspaces made
// from the old copy keep their worktrees, which stay on the old copy (hidden,
// never deleted); new workspaces use the fresh copy.
func (b *Bench) RefreshRepositoryIdentity(ctx context.Context, identity string, confirm bool, progress func(string) error) (RepositoryRefresh, error) {
	if b.Collections == nil || b.RepositoryImporter == nil {
		return RepositoryRefresh{}, errors.New("workspace storage unavailable")
	}
	name, ok := strings.CutPrefix(identity, "github:")
	if !ok || gitidentity.GitHubName("https://github.com/"+name) != name {
		return RepositoryRefresh{}, errors.New("only a GitHub repository's copy can be replaced")
	}
	refresh := RepositoryRefresh{Identity: identity, Name: name}
	var current *collections.Repository
	for _, r := range b.Collections.Repositories() {
		if b.Collections.RepositoryIdentity(r.ID) == identity {
			r := r
			current = &r
		}
	}
	if current != nil && !b.RepositoryImporter.Owns(*current) {
		return refresh, fmt.Errorf("Lazybox has no copy of %s to replace: it uses your checkout at %s. Update that checkout with Git", name, current.Path)
	}
	listed := int64(0)
	if rows, _, err := b.RepositoryCatalogSnapshot(ctx, false); err == nil {
		for _, row := range rows {
			if row.Identity == identity {
				listed = row.GitHubID
			}
		}
	}
	var remote github.Repository
	called := false
	err := b.GitHub.WithCredential(ctx, func(token string) error {
		called = true
		var err error
		if remote, err = b.GitHubAPI.RepositoryNamed(ctx, token, name); err != nil {
			return err
		}
		if "github:"+strings.ToLower(remote.FullName) != identity {
			return fmt.Errorf("GitHub answers %s for %s", remote.FullName, name)
		}
		if current != nil {
			refresh.HasCopy = true
			for _, w := range b.Collections.RepositoryUsers(current.ID) {
				refresh.Workspaces = append(refresh.Workspaces, w.Name)
			}
		}
		refresh.Name = remote.FullName
		if (current != nil && current.GitHubID != 0 && current.GitHubID != remote.ID) || (listed != 0 && listed != remote.ID) {
			refresh.Reason = "replaced"
		}
		if !confirm {
			return nil
		}
		report := func(message string) error {
			if progress == nil {
				return nil
			}
			return progress(message)
		}
		added, _, err := b.RepositoryImporter.Replace(ctx, remote, token, report)
		if err != nil {
			return err
		}
		refresh.Replaced, refresh.RepositoryID = true, added.ID
		return nil
	})
	if !called {
		return refresh, fmt.Errorf("connect GitHub to replace Lazybox's copy of %s: %w", name, err)
	}
	if err != nil {
		return refresh, fmt.Errorf("could not replace Lazybox's copy of %s: %w", name, err)
	}
	if refresh.Replaced {
		log.Printf("repositories: replaced Lazybox's copy of %s with %s (workspaces kept on the old copy: %d)", name, refresh.RepositoryID, len(refresh.Workspaces))
	}
	return refresh, nil
}

// replacedCopy refuses a registered copy of a GitHub repository whose name
// now belongs to another repository: fetching into it would mix the new
// repository's history into the old one's copy. It answers nil when it cannot
// tell (no connection, GitHub unreachable): only a proved change refuses.
func (b *Bench) replacedCopy(ctx context.Context, identity, name, repositoryID string) error {
	repo, ok := b.Collections.Repository(repositoryID)
	if !ok || repo.GitHubID == 0 || b.GitHub == nil || b.GitHubAPI == nil || !strings.HasPrefix(identity, "github:") {
		return nil
	}
	var remote github.Repository
	err := b.GitHub.WithCredential(ctx, func(token string) error {
		var err error
		remote, err = b.GitHubAPI.RepositoryNamed(ctx, token, strings.TrimPrefix(identity, "github:"))
		return err
	})
	if err != nil || remote.ID == 0 || remote.ID == repo.GitHubID {
		return nil
	}
	return &RepositoryReplaced{Identity: identity, Name: name, HasCopy: true}
}
