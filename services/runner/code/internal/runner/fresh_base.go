package runner

import (
	"context"
	"errors"
	"fmt"
	"log"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
)

// RefreshBases is refreshBase for the raw CreateCollection and
// AddCollectionRepositories calls (`lazybox create-collection --spec`): each
// member without an explicit base is refreshed first; an explicit one is left
// alone.
func (b *Bench) RefreshBases(ctx context.Context, members []collections.MemberSpec) {
	if b == nil || b.Collections == nil {
		return
	}
	done := map[string]bool{}
	for _, m := range members {
		if (m.BaseRef != "" && m.BaseRef != "HEAD") || done[m.RepositoryID] {
			continue
		}
		done[m.RepositoryID] = true
		_ = b.refreshBase(ctx, "", m.RepositoryID, firstOf(m.Name, m.RepositoryID), func(string, string) error { return nil })
	}
}

// refreshBase fetches origin's default branch for a repository about to get a
// new workspace branch, so the branch starts from origin as it is now, not
// from a source copy's stale branch or the branch the person's checkout has
// out (collections/base.go resolves which ref that is). It runs for every
// repository added to a new or an existing workspace that starts from the
// default branch: New task, Add repository, Start from an issue, an agent's
// repository request and `lazybox new`. A branch chosen to start from
// instead is fetched by refreshStart (start_from.go).
//
// It never fails the workspace. When origin cannot be reached the branch
// starts from the last known default branch and progress says so, once:
// "Couldn't update api; started from main as of Nov 17 09:12". A repository
// without origin has nothing to update and says nothing.
func (b *Bench) refreshBase(ctx context.Context, identity, repositoryID, name string, progress func(string, string) error) error {
	path := ""
	for _, r := range b.Collections.Repositories() {
		if r.ID == repositoryID {
			path = r.Path
		}
	}
	if path == "" {
		return nil
	}
	if err := progress(identity, "Updating "+name+" from origin"); err != nil {
		return err
	}
	// The GitHub connection's credential, inside its scope only; without a
	// connection, the person's own Git setup (a credential helper) tries.
	err := b.withOrigin(ctx, path, func(token string) error {
		_, err := gitpublish.Git{}.RefreshDefault(ctx, path, token)
		return err
	})
	if err == nil || errors.Is(err, gitpublish.ErrNoOrigin) || ctx.Err() != nil {
		return nil
	}
	log.Printf("workspace base: could not update %s from origin: %v", name, err)
	base, e := b.Collections.DefaultBase(ctx, repositoryID)
	if e != nil {
		// Planning will explain why there is no usable starting commit.
		return nil
	}
	return progress(identity, fmt.Sprintf("Couldn't update %s; started from %s as of %s", name, firstOf(base.Branch, "its default branch"), base.AsOf.Local().Format("Jan 2 15:04")))
}
