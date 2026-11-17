package runner

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/gitidentity"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/collections"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/homes"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
)

// RepositoryHome is a repository's home checkout (WORKSPACES.md, "Home
// checkouts") and what it holds now.
type RepositoryHome struct {
	RepositoryID, Name string
	// Path is "" when the repository has no home.
	Path string
	// Kind is collections.HomeConfigured, collections.HomeSource or "".
	Kind      string
	Suggested string
	homes.Status
}

// homeRoot is where default homes go: the Runner's homeCheckoutRoot, else
// ~/lazybox/repos.
func (b *Bench) homeRoot() string {
	if b.Config.HomeCheckoutRoot != "" {
		return b.Config.HomeCheckoutRoot
	}
	return homes.DefaultRoot()
}

// HomeRoot is the folder default homes go in.
func (b *Bench) HomeRoot() string { return b.homeRoot() }

// repositoryName is owner/repo for a GitHub repository, else its name.
func repositoryName(r collections.Repository) string {
	if full := gitidentity.GitHubName(r.SourceURL); full != "" {
		return full
	}
	if r.Name != "" {
		return r.Name
	}
	return r.ID
}

func (b *Bench) describeHome(ctx context.Context, r collections.Repository, inspect bool) RepositoryHome {
	path, kind := b.Collections.Home(r.ID)
	h := RepositoryHome{RepositoryID: r.ID, Name: repositoryName(r), Path: path, Kind: kind, Suggested: homes.Suggest(b.homeRoot(), r.SourceURL, r.Name)}
	if path != "" && inspect {
		h.Status = homes.Inspect(ctx, path)
	}
	return h
}

// RepositoryHomes lists the homes of ids (every library repository when
// empty), with their branch and changed files read from local Git.
func (b *Bench) RepositoryHomes(ctx context.Context, ids []string) ([]RepositoryHome, error) {
	if b.Collections == nil {
		return nil, errors.New("repository library unavailable")
	}
	var list []collections.Repository
	if len(ids) == 0 {
		list = b.Collections.Repositories()
	} else {
		for _, id := range ids {
			r, ok := b.Collections.Repository(id)
			if !ok {
				return nil, fmt.Errorf("repository %s: %w", id, os.ErrNotExist)
			}
			list = append(list, r)
		}
	}
	out := make([]RepositoryHome, 0, len(list))
	for _, r := range list {
		out = append(out, b.describeHome(ctx, r, true))
	}
	return out, nil
}

// homePath makes a path the person typed absolute (~ is their home) and
// refuses one inside Lazybox's own folders: the workspaces and the Runner's
// state, where the source copies live. A home is never Lazybox's storage.
func (b *Bench) homePath(path string) (string, error) {
	path = strings.TrimSpace(path)
	if path == "~" || strings.HasPrefix(path, "~/") {
		home, err := os.UserHomeDir()
		if err != nil {
			return "", errors.New("cannot expand ~; give an absolute path")
		}
		path = filepath.Join(home, strings.TrimPrefix(path, "~"))
	}
	if !filepath.IsAbs(path) {
		return "", errors.New("a home checkout needs an absolute path")
	}
	path = filepath.Clean(path)
	for _, own := range []string{b.Collections.WorktreeRoot(), b.Config.State} {
		resolved := own
		if r, err := filepath.EvalSymlinks(own); err == nil {
			resolved = r
		}
		if collections.Within(own, path) || collections.Within(resolved, path) {
			return "", errors.New("a home checkout cannot be inside Lazybox's own folders (its workspaces and its Runner state)")
		}
	}
	return path, nil
}

// SetRepositoryHome creates (clones) a home, adopts an existing checkout of
// the repository as its home, or forgets the home (action clear; no file
// changes). Creating never overwrites a folder, and nothing here deletes one.
func (b *Bench) SetRepositoryHome(ctx context.Context, id, action, path string) (RepositoryHome, error) {
	if b.Collections == nil {
		return RepositoryHome{}, errors.New("repository library unavailable")
	}
	r, ok := b.Collections.Repository(id)
	if !ok {
		return RepositoryHome{}, fmt.Errorf("repository %s: %w", id, os.ErrNotExist)
	}
	b.homeMu.Lock()
	defer b.homeMu.Unlock()
	switch action {
	case "clear":
		if _, err := b.Collections.SetHome(id, ""); err != nil {
			return RepositoryHome{}, err
		}
	case "create":
		if path == "" {
			path = homes.Suggest(b.homeRoot(), r.SourceURL, r.Name)
		}
		dest, err := b.homePath(path)
		if err != nil {
			return RepositoryHome{}, err
		}
		if err = homes.Create(ctx, r.Path, dest); err != nil {
			return RepositoryHome{}, err
		}
		if dest, err = filepath.EvalSymlinks(dest); err != nil {
			return RepositoryHome{}, err
		}
		if _, err = b.Collections.SetHome(id, dest); err != nil {
			return RepositoryHome{}, err
		}
	case "adopt":
		dest, err := b.homePath(path)
		if err != nil {
			return RepositoryHome{}, err
		}
		if dest, err = filepath.EvalSymlinks(dest); err != nil {
			return RepositoryHome{}, errors.New("that folder does not exist")
		}
		if dest, err = b.homePath(dest); err != nil {
			return RepositoryHome{}, err
		}
		identity, top, err := homes.Identity(ctx, dest)
		if err != nil {
			return RepositoryHome{}, err
		}
		if top != dest {
			return RepositoryHome{}, fmt.Errorf("choose the checkout's top folder (%s), not a folder inside it", top)
		}
		// The same repository: the same GitHub project or Git history, or a
		// clone of the registered folder itself (a repository with no remote).
		if want := b.Collections.RepositoryIdentity(id); identity != want && homes.OriginFolder(ctx, dest) != r.Path {
			if other, ok := strings.CutPrefix(identity, "github:"); ok {
				return RepositoryHome{}, fmt.Errorf("that folder is a checkout of %s, not %s", other, repositoryName(r))
			}
			return RepositoryHome{}, fmt.Errorf("that folder is a checkout of another repository, not %s", repositoryName(r))
		}
		if dest == r.Path {
			// The registered folder is the home already; nothing to record.
			dest = ""
		}
		if _, err = b.Collections.SetHome(id, dest); err != nil {
			return RepositoryHome{}, err
		}
	default:
		return RepositoryHome{}, errors.New("action must be create, adopt or clear")
	}
	r, _ = b.Collections.Repository(id)
	return b.describeHome(ctx, r, true), nil
}

// OpenRepositoryHome shows a repository's home in Finder or an editor on the
// Runner's computer. The path is the Runner's record, never the caller's.
func (b *Bench) OpenRepositoryHome(ctx context.Context, id, app string) error {
	if b.Collections == nil {
		return errors.New("repository library unavailable")
	}
	path, _ := b.Collections.Home(id)
	if path == "" {
		return errors.New("this repository has no home checkout yet: create one in Settings → Repositories")
	}
	if info, err := os.Stat(path); err != nil || !info.IsDir() {
		return fmt.Errorf("the home checkout %s is missing", path)
	}
	return homes.Open(ctx, path, app)
}

// OpenPlaceTerminal starts the person's login shell outside the workspace:
// in a repository's home checkout, or (userHome) in their home folder. The
// terminal still belongs to the workspace: it shows in its Terminal panel,
// gets the workspace's terminal credential (so `lazybox log` there feeds this
// workspace's console) and closes with it.
func (b *Bench) OpenPlaceTerminal(collectionID, repositoryID string, userHome bool, cols, rows uint16, requestKey string) (*terminals.Terminal, error) {
	if b.Collections == nil {
		return nil, errors.New("workspaces unavailable")
	}
	c, err := b.Collections.Get(collectionID)
	if err != nil {
		return nil, err
	}
	if c.State == "deleting" || c.State == "deleted" {
		return nil, errors.New("workspace is being deleted")
	}
	var cwd, place, title string
	switch {
	case repositoryID != "" && userHome:
		return nil, errors.New("choose a repository home or your home folder, not both")
	case repositoryID != "":
		r, ok := b.Collections.Repository(repositoryID)
		if !ok {
			return nil, fmt.Errorf("repository %s: %w", repositoryID, os.ErrNotExist)
		}
		if cwd, _ = b.Collections.Home(repositoryID); cwd == "" {
			return nil, errors.New("this repository has no home checkout yet: create one in Settings → Repositories")
		}
		place, title = "repository-home", r.Name+" home"
		if r.Name == "" {
			title = repositoryName(r) + " home"
		}
	case userHome:
		if cwd, err = os.UserHomeDir(); err != nil {
			return nil, errors.New("your home folder is unknown")
		}
		place, title = "user-home", "~"
	default:
		return nil, errors.New("choose where the terminal starts")
	}
	if info, err := os.Stat(cwd); err != nil || !info.IsDir() {
		return nil, fmt.Errorf("%s is missing", cwd)
	}
	shell, err := terminals.Shell(os.Getenv("SHELL"))
	if err != nil {
		return nil, err
	}
	env := b.withCLI(terminals.Environment(os.Environ(), collectionID))
	return b.Terminals.Start(terminals.Spec{CollectionID: collectionID, Place: place, RepositoryID: repositoryID, Cwd: cwd, Title: title, Shell: shell, Env: env, Cols: cols, Rows: rows, Prepare: b.terminalCredential(collectionID)}, requestKey)
}

// homesBriefing is the line of a new conversation's workspace context that
// names each member repository's home checkout, or "" when none has one.
// The rules for a home are the shared briefing's (command_output.go).
func (b *Bench) homesBriefing(c collections.Collection) string {
	var lines []string
	for _, m := range c.Members {
		if home, _ := b.Collections.Home(m.RepositoryID); home != "" {
			lines = append(lines, "- "+m.Name+": "+home)
		}
	}
	if len(lines) == 0 {
		return ""
	}
	return "Home checkouts (the person's own clones, outside this workspace; read-only for you, never commit, reset or switch branches there; use them, not a temporary clone, to run a repository's own tools at its latest main; tell the person to run logins and long-lived commands there):\n" + strings.Join(lines, "\n")
}

// UpdateRepositoryHome fetches a home's origin and fast-forwards its default
// branch when that is safe (homes.Update); message says what happened or why
// not. Nothing in the home is ever discarded.
func (b *Bench) UpdateRepositoryHome(ctx context.Context, id string) (RepositoryHome, bool, string, error) {
	if b.Collections == nil {
		return RepositoryHome{}, false, "", errors.New("repository library unavailable")
	}
	r, ok := b.Collections.Repository(id)
	if !ok {
		return RepositoryHome{}, false, "", fmt.Errorf("repository %s: %w", id, os.ErrNotExist)
	}
	path, _ := b.Collections.Home(id)
	if path == "" {
		return RepositoryHome{}, false, "", errors.New("this repository has no home checkout yet: create one in Settings → Repositories")
	}
	b.homeMu.Lock()
	updated, message, err := homes.Update(ctx, path)
	b.homeMu.Unlock()
	if err != nil {
		return RepositoryHome{}, false, "", err
	}
	return b.describeHome(ctx, r, true), updated, message, nil
}
