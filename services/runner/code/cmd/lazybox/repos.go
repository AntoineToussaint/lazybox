package main

import (
	"errors"
	"fmt"
	"strings"
	"text/tabwriter"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox repos` and `lazybox repo home` show each repository's home
// checkout: the person's own clone, outside every workspace, where logins,
// long-lived dev servers and manual tests run (WORKSPACES.md, "Home
// checkouts"). The person sees every repository of the library; an agent,
// whose credential reaches only its own workspace, sees that workspace's.

const repoUsage = `Usage:
  lazybox repos
  lazybox repo home <name|owner/repo> [--create [--path PATH] | --adopt PATH | --forget | --update]`

// homeRow is one repository and its home, whichever way it was read.
type homeRow struct {
	Name, Path, Kind, Branch, Error string
	Dirty                           uint32
	Detached, Exists, Inspected     bool
	RepositoryID                    string
}

// homes reads the repositories this caller may see, with their homes: the
// whole library for the person, the workspace's repositories for an agent.
func (c *cli) homes(cl *client) ([]homeRow, error) {
	list, err := got(cl.ListRepositoryHomes(c.ctx, connect.NewRequest(&pb.ListRepositoryHomesRequest{})))
	if err == nil {
		rows := make([]homeRow, 0, len(list.Homes))
		for _, h := range list.Homes {
			rows = append(rows, homeRow{Name: h.Name, Path: h.Path, Kind: h.Kind, Branch: h.Branch, Error: h.Error, Dirty: h.Dirty, Detached: h.Detached, Exists: h.Exists, Inspected: true, RepositoryID: h.RepositoryId})
		}
		return rows, nil
	}
	workspace := c.getenv("LAZYBOX_V2_COLLECTION_ID")
	if connect.CodeOf(err) != connect.CodePermissionDenied || workspace == "" {
		return nil, err
	}
	members, err := got(cl.ListWorkspaceRepositories(c.ctx, connect.NewRequest(&pb.ListWorkspaceRepositoriesRequest{CollectionId: workspace})))
	if err != nil {
		return nil, err
	}
	rows := make([]homeRow, 0, len(members.Repositories))
	for _, r := range members.Repositories {
		name := strings.TrimPrefix(r.Identity, "github:")
		if !strings.Contains(name, "/") {
			name = r.Name
		}
		rows = append(rows, homeRow{Name: name, Path: r.Home, RepositoryID: r.RepositoryId})
	}
	return rows, nil
}

func (c *cli) repos(args []string) error {
	f := c.flags("repos")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) > 0 {
		return fail(exitUsage, "usage: lazybox repos")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	rows, err := c.homes(cl)
	if err != nil {
		return err
	}
	if c.json {
		return c.printJSON(rows)
	}
	if len(rows) == 0 {
		c.out("No repositories yet. Add one in Lazybox: New workspace, or Settings → Repositories.\n")
		return nil
	}
	tw := tabwriter.NewWriter(c.stdout, 0, 0, 2, ' ', 0)
	fmt.Fprintf(tw, "%s\t%s\t%s\n", c.paint(dim, "REPOSITORY"), c.paint(dim, "HOME CHECKOUT"), c.paint(dim, "STATE"))
	for _, r := range rows {
		home := r.Path
		if home == "" {
			home = c.paint(dim, "none")
		}
		fmt.Fprintf(tw, "%s\t%s\t%s\n", r.Name, home, homeState(r))
	}
	_ = tw.Flush()
	c.out("%s\n", c.paint(dim, "A home checkout is the person's own clone: read-only for agents. Run logins and long-lived commands there."))
	return nil
}

// homeState says what a home holds: its branch and changed files.
func homeState(r homeRow) string {
	switch {
	case r.Path == "" || !r.Inspected:
		return ""
	case r.Error != "":
		return r.Error
	}
	branch := r.Branch
	if r.Detached {
		branch = "detached HEAD"
	}
	if r.Dirty > 0 {
		return fmt.Sprintf("%s · %d changed", branch, r.Dirty)
	}
	return branch + " · clean"
}

// findHome is the row a name, owner/repo or repository id names.
func findHome(rows []homeRow, query string) (homeRow, error) {
	query = strings.TrimSuffix(strings.TrimSpace(query), ".git")
	var matches []homeRow
	for _, r := range rows {
		_, short, _ := strings.Cut(r.Name, "/")
		if strings.EqualFold(r.Name, query) || strings.EqualFold(short, query) || r.RepositoryID == query {
			matches = append(matches, r)
		}
	}
	switch len(matches) {
	case 1:
		return matches[0], nil
	case 0:
		return homeRow{}, fail(exitNotFound, "no repository %q: lazybox repos lists them", query)
	}
	names := make([]string, 0, len(matches))
	for _, r := range matches {
		names = append(names, r.Name)
	}
	return homeRow{}, fail(exitUsage, "%q names several repositories (%s): give owner/repo", query, strings.Join(names, ", "))
}

func (c *cli) repo(args []string) error {
	if len(args) == 0 || args[0] != "home" {
		fmt.Fprintln(c.stderr, repoUsage)
		return fail(exitUsage, "lazybox repo needs home")
	}
	f := c.flags("repo")
	create := f.Bool("create", false, "clone a home checkout (at --path, or the suggested place)")
	path := f.String("path", "", "where --create clones it")
	adopt := f.String("adopt", "", "use this existing checkout of the repository as its home")
	forget := f.Bool("forget", false, "forget the home checkout (no file changes)")
	update := f.Bool("update", false, "fetch, then fast-forward the default branch when it is checked out and clean (never discards anything)")
	positional, err := c.parse(f, args[1:])
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox repo home <name|owner/repo>")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	rows, err := c.homes(cl)
	if err != nil {
		return err
	}
	row, err := findHome(rows, positional[0])
	if err != nil {
		return err
	}
	if *update {
		if *create || *adopt != "" || *forget {
			return fail(exitUsage, "choose one of --create, --adopt, --forget and --update")
		}
		u, err := got(cl.UpdateRepositoryHome(c.ctx, connect.NewRequest(&pb.UpdateRepositoryHomeRequest{RepositoryId: row.RepositoryID})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(u)
		}
		// The path on stdout, as always; what happened on stderr.
		c.note("%s\n", u.Message)
		c.out("%s\n", u.Home.GetPath())
		return nil
	}
	action, target := "", ""
	switch {
	case *create && *adopt == "" && !*forget:
		action, target = "create", *path
	case *adopt != "" && !*create && !*forget:
		if action, target = "adopt", *adopt; target != "" {
			if target, err = c.repositoryQuery(target); err != nil {
				return err
			}
		}
	case *forget && !*create && *adopt == "":
		action = "clear"
	case !*create && *adopt == "" && !*forget:
	default:
		return fail(exitUsage, "choose one of --create, --adopt and --forget")
	}
	if action != "" {
		h, err := got(cl.SetRepositoryHome(c.ctx, connect.NewRequest(&pb.SetRepositoryHomeRequest{RepositoryId: row.RepositoryID, Action: action, Path: target})))
		if err != nil {
			return err
		}
		row = homeRow{Name: h.Name, Path: h.Path, Kind: h.Kind, Branch: h.Branch, Error: h.Error, Dirty: h.Dirty, Detached: h.Detached, Exists: h.Exists, Inspected: true, RepositoryID: h.RepositoryId}
		if action == "clear" && row.Path == "" {
			c.note("Forgot the home checkout of %s; its files are unchanged.\n", row.Name)
			return nil
		}
	}
	if c.json {
		return c.printJSON(row)
	}
	if row.Path == "" {
		return exitError{exitNotFound, errors.New(row.Name + " has no home checkout: create one in Settings → Repositories, or lazybox repo home " + row.Name + " --create")}
	}
	// The path alone, so `cd "$(lazybox repo home api)"` works.
	c.out("%s\n", row.Path)
	return nil
}
