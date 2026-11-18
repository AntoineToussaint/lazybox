package runner

import (
	"context"
	"errors"
	"fmt"
	"log"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitpublish"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// "Start from" (WORKSPACES.md): a repository's new workspace branch can start
// from a branch of origin other than its default, such as a long-lived
// feature branch the person develops on. The branch is fetched first and the
// workspace branches from origin/<branch> as it is now, with the same offline
// fallback as the default branch (fresh_base.go). The choice is remembered
// per repository (collections/start_from.go): New task, Add repository, Start
// from an issue, an agent's repository request and `lazybox new` then start
// there without asking, until the person chooses origin's default again.

// maxStartFrom bounds a request's choices like its repositories.
const maxStartFrom = 64

// startPlan is where one repository of a preparation starts, and whether the
// person chose it in this request (so it is remembered once the workspace is made).
type startPlan struct {
	repositoryID string
	branch       string
	chosen       bool
	isDefault    bool
}

// resolveStart fetches what a repository's new branch starts from and returns
// the base ref for its member: "HEAD" for origin's default branch (refreshed
// by refreshBase, resolved by collections/base.go), else refs/remotes/origin/<branch>.
// branch is the person's choice in this request when chosen, else the
// repository's remembered one ("" for none).
func (b *Bench) resolveStart(ctx context.Context, identity, repositoryID, name, branch string, chosen bool, progress func(string, string) error) (string, startPlan, error) {
	plan := startPlan{repositoryID: repositoryID, branch: branch, chosen: chosen}
	if branch == "" {
		plan.isDefault = true
		return "HEAD", plan, b.refreshBase(ctx, identity, repositoryID, name, progress)
	}
	ref, isDefault, err := b.refreshStart(ctx, identity, repositoryID, name, branch, progress)
	if err != nil && !chosen && errors.Is(err, gitpublish.ErrNoBranch) {
		// The remembered branch is gone from origin (merged and deleted):
		// origin's default again, said once, and forgotten.
		if err = progress(identity, fmt.Sprintf("%s is no longer on origin; %s starts from its default branch", branch, name)); err != nil {
			return "", plan, err
		}
		plan.branch, plan.chosen, plan.isDefault = "", true, true
		return "HEAD", plan, b.refreshBase(ctx, identity, repositoryID, name, progress)
	}
	if err != nil {
		return "", plan, err
	}
	plan.isDefault = isDefault
	return ref, plan, nil
}

// refreshStart fetches a chosen branch from origin. It fails only when the
// branch cannot be a start: origin has no such branch, or origin cannot be
// reached and the Runner never fetched it. Offline with an earlier fetch, the
// workspace starts from that and progress says so, once, as for the default
// branch: "Couldn't update api; started from feat/x as of Nov 17 09:12".
func (b *Bench) refreshStart(ctx context.Context, identity, repositoryID, name, branch string, progress func(string, string) error) (string, bool, error) {
	path := b.repositoryPath(repositoryID)
	if path == "" {
		return "", false, fmt.Errorf("repository %s is not on this Runner", name)
	}
	if err := gitpublish.ValidBranch(ctx, path, branch); err != nil {
		return "", false, err
	}
	if err := progress(identity, "Updating "+name+" from origin ("+branch+")"); err != nil {
		return "", false, err
	}
	var refreshed gitpublish.Refreshed
	err := b.withOrigin(ctx, path, func(token string) error {
		var e error
		refreshed, e = gitpublish.Git{}.RefreshBranch(ctx, path, token, branch)
		return e
	})
	switch {
	case err == nil:
		return refreshed.Ref, refreshed.Default == branch, nil
	case errors.Is(err, gitpublish.ErrNoBranch):
		return "", false, noBranch{branch: branch, name: name}
	case ctx.Err() != nil:
		return "", false, ctx.Err()
	}
	// No origin: the repository's own branch. Unreachable: the last fetch.
	base, e := b.Collections.BranchBase(ctx, repositoryID, branch)
	if errors.Is(err, gitpublish.ErrNoOrigin) {
		if e != nil {
			return "", false, fmt.Errorf("%s has no branch %s", name, branch)
		}
		return base.Ref, false, nil
	}
	log.Printf("workspace base: could not update %s (%s) from origin: %v", name, branch, err)
	if e != nil {
		return "", false, fmt.Errorf("couldn't reach origin for %s, and this Runner has never fetched %s: try again online, or start from another branch", name, branch)
	}
	isDefault := false
	if def, err := b.Collections.DefaultBase(ctx, repositoryID); err == nil {
		isDefault = def.Branch == branch
	}
	return base.Ref, isDefault, progress(identity, fmt.Sprintf("Couldn't update %s; started from %s as of %s", name, branch, base.AsOf.Local().Format("Jan 2 15:04")))
}

// noBranch: origin answered and has no branch of that name.
type noBranch struct{ branch, name string }

func (e noBranch) Error() string {
	return fmt.Sprintf("%s has no branch %s on origin: choose another to start from", e.name, e.branch)
}
func (e noBranch) Unwrap() error { return gitpublish.ErrNoBranch }

// rememberStarts records the choices of a preparation that made its workspace.
func (b *Bench) rememberStarts(plans []startPlan) {
	for _, p := range plans {
		if !p.chosen {
			continue
		}
		if err := b.Collections.RememberStartFrom(p.repositoryID, p.branch, p.isDefault); err != nil {
			log.Printf("workspace base: could not remember %s for %s: %v", p.branch, p.repositoryID, err)
		}
	}
}

// repositoryPath is a registered repository's source path, or "".
func (b *Bench) repositoryPath(repositoryID string) string {
	if r, ok := b.Collections.Repository(repositoryID); ok {
		return r.Path
	}
	return ""
}

// withOrigin runs fetch with the credential for path's origin: the GitHub
// connection's, inside its scope only, for an HTTPS GitHub origin; without a
// connection (or for another origin), the person's own Git setup.
func (b *Bench) withOrigin(ctx context.Context, path string, fetch func(token string) error) error {
	if origin, e := (gitpublish.Git{}).Origin(ctx, path); e == nil && origin.HTTPS && b.GitHub != nil {
		called := false
		err := b.GitHub.WithCredential(ctx, func(token string) error { called = true; return fetch(token) })
		if !called {
			err = fetch("")
		}
		return err
	}
	return fetch("")
}

// checkStarts validates a request's start choices.
func checkStarts(starts map[string]string) error {
	if len(starts) > maxStartFrom {
		return fmt.Errorf("choose a start branch for at most %d repositories", maxStartFrom)
	}
	for _, branch := range starts {
		if len(branch) > 255 || strings.ContainsAny(branch, "\x00\n") {
			return errors.New("a start branch is not a branch name")
		}
	}
	return nil
}

// RepositoryBranches answers the "Start from" picker for one repository of
// the catalog: origin's branches (asked now when remote; else as last
// fetched), its default branch and the remembered choice. A repository not
// on this Runner yet has none to list; a branch typed for it is checked when
// it is prepared.
func (b *Bench) RepositoryBranches(ctx context.Context, identity string, remote bool) (*pb.RepositoryBranches, error) {
	out := &pb.RepositoryBranches{RepositoryIdentity: identity}
	if b.Collections == nil {
		return nil, errors.New("workspace storage unavailable")
	}
	rows, _, err := b.RepositoryCatalogSnapshot(ctx, false)
	if err != nil {
		return nil, err
	}
	repositoryID := ""
	known := false
	for _, row := range rows {
		if row.Identity == identity {
			known, repositoryID = true, row.RepositoryID
		}
	}
	if !known {
		return nil, fmt.Errorf("repository %s is not in the catalog: %w", identity, os.ErrNotExist)
	}
	path := ""
	if repositoryID != "" {
		path = b.repositoryPath(repositoryID)
	}
	if path == "" {
		out.Unregistered = true
		return out, nil
	}
	start := b.Collections.StartFrom(repositoryID)
	out.StartFrom, out.Recent = start.Branch, start.Recent
	if def, err := b.Collections.DefaultBase(ctx, repositoryID); err == nil {
		out.DefaultBranch = def.Branch
	}
	git := gitpublish.Git{}
	var listed []gitpublish.RemoteBranch
	if remote {
		ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
		defer cancel()
		err = b.withOrigin(ctx, path, func(token string) error {
			var e error
			var def string
			listed, def, e = git.RemoteBranches(ctx, path, token)
			if def != "" {
				out.DefaultBranch = def
			}
			return e
		})
		switch {
		case err == nil:
			out.Remote = true
		case errors.Is(err, gitpublish.ErrNoOrigin):
			listed = nil
		default:
			out.Error = "Couldn't reach origin; these are the branches as last fetched."
			listed = nil
		}
	}
	// How recently each branch moved: its commit's time when the Runner has
	// it, else its last fetched tip's (origin moved since: at least that recent).
	times := b.Collections.CommitTimes(ctx, repositoryID)
	fetchedAt := map[string]int64{}
	for _, k := range b.Collections.KnownBranches(ctx, repositoryID) {
		fetchedAt[k.Name] = k.CommittedAt
		if !out.Remote {
			listed = append(listed, gitpublish.RemoteBranch{Name: k.Name, Commit: k.Commit})
		}
	}
	for _, branch := range listed {
		at := times[branch.Commit]
		if at == 0 {
			at = fetchedAt[branch.Name]
		}
		out.Branches = append(out.Branches, &pb.RepositoryBranch{Name: branch.Name, Commit: branch.Commit, CommittedAtUnixMillis: at})
	}
	sort.SliceStable(out.Branches, func(i, j int) bool {
		a, c := out.Branches[i], out.Branches[j]
		if a.CommittedAtUnixMillis != c.CommittedAtUnixMillis {
			return a.CommittedAtUnixMillis > c.CommittedAtUnixMillis
		}
		return a.Name < c.Name
	})
	return out, nil
}
