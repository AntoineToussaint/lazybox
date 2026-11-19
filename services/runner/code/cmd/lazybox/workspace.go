package main

import (
	"errors"
	"fmt"
	"path/filepath"
	"strings"
	"text/tabwriter"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox workspace` is how an agent works with its workspace's
// repositories: list them, and ask the person to add one it needs instead of
// reaching outside the workspace (WORKSPACES.md, "When an agent needs another
// repository"). An agent's credential reaches only its own workspace, and can
// ask but never add.

const workspaceUsage = `Usage:
  lazybox workspace repos [WORKSPACE]
  lazybox workspace request-repo <name|owner/repo|path> --reason "why you need it" [--no-wait] [--timeout 90s]`

// defaultRequestWait stays under an agent's usual command timeout (Claude
// Code's Bash tool stops a command after two minutes).
const defaultRequestWait = 90 * time.Second

func (c *cli) workspace(args []string) error {
	if c.subcommandHelp(args, workspaceUsage) {
		return nil
	}
	switch args[0] {
	case "repos":
		return c.workspaceRepos(args[1:])
	case "request-repo":
		return c.requestRepo(args[1:])
	}
	fmt.Fprintln(c.stderr, workspaceUsage)
	return fail(exitUsage, "unknown workspace command %q", args[0])
}

func (c *cli) workspaceRepos(args []string) error {
	f := c.flags("workspace")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) > 1 {
		return fail(exitUsage, "usage: lazybox workspace repos [WORKSPACE]")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	workspace := c.getenv("LAZYBOX_V2_COLLECTION_ID")
	if len(positional) == 1 {
		k, err := loadCatalog(c.ctx, cl)
		if err != nil {
			return err
		}
		w, err := k.findWorkspace(positional[0])
		if err != nil {
			return err
		}
		workspace = w.Id
	}
	if workspace == "" {
		return fail(exitUsage, "name a workspace (lazybox ls lists them); inside a Lazybox agent or terminal it is the one you work in")
	}
	list, err := got(cl.ListWorkspaceRepositories(c.ctx, connect.NewRequest(&pb.ListWorkspaceRepositoriesRequest{CollectionId: workspace})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(list)
	}
	c.out("%s · branch %s\n", c.paint(bold, list.Name), list.Branch)
	if len(list.Repositories) == 0 {
		c.out("  No repositories yet.\n")
	}
	tw := tabwriter.NewWriter(c.stdout, 0, 0, 2, ' ', 0)
	for _, r := range list.Repositories {
		fmt.Fprintf(tw, "  %s\t%s\t%s\n", r.Name, r.Path, r.Branch)
	}
	_ = tw.Flush()
	homes := []string{}
	for _, r := range list.Repositories {
		if r.Home != "" {
			homes = append(homes, "  "+r.Name+"\t"+r.Home)
		}
	}
	if len(homes) > 0 {
		c.out("Home checkouts (the person's own clones; read-only for agents):\n")
		tw = tabwriter.NewWriter(c.stdout, 0, 0, 2, ' ', 0)
		fmt.Fprintln(tw, strings.Join(homes, "\n"))
		_ = tw.Flush()
	}
	if len(list.Requests) > 0 {
		c.out("Waiting for the person to add:\n")
		for _, r := range list.Requests {
			c.out("  %s · asked %s · %s\n", requestedName(r), ago(c.now(), time.UnixMilli(r.CreatedAtUnixMillis)), c.paint(dim, r.Id))
		}
	}
	return nil
}

// requestedName is what a request asks for: its best match, or the words.
func requestedName(r *pb.RepositoryRequest) string {
	if len(r.Candidates) > 0 {
		return r.Candidates[0].Name
	}
	return "“" + r.Query + "”"
}

// repositoryQuery makes a path an absolute one (the Runner resolves paths
// on this machine); anything else is a name, owner/repo or URL as given.
func (c *cli) repositoryQuery(query string) (string, error) {
	query = strings.TrimSpace(query)
	home := c.getenv("HOME")
	switch {
	case query == "~" || strings.HasPrefix(query, "~/"):
		if home == "" {
			return "", fail(exitUsage, "cannot expand ~ without HOME; give an absolute path")
		}
		return filepath.Join(home, strings.TrimPrefix(query, "~")), nil
	case query == "." || query == ".." || strings.HasPrefix(query, "./") || strings.HasPrefix(query, "../"):
		return filepath.Abs(query)
	case filepath.IsAbs(query):
		return filepath.Clean(query), nil
	}
	return query, nil
}

func (c *cli) requestRepo(args []string) error {
	f := c.flags("workspace")
	reason := f.String("reason", "", "why you need it: the person decides from this")
	noWait := f.Bool("no-wait", false, "ask and return at once; the answer comes later as a note in this conversation")
	wait := f.Duration("timeout", defaultRequestWait, "how long to wait for the person's answer")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox workspace request-repo <name|owner/repo|path> --reason \"why you need it\"")
	}
	if strings.TrimSpace(*reason) == "" {
		return fail(exitUsage, "--reason is required: say what you need the repository for; the person decides from it")
	}
	session := c.getenv("LAZYBOX_V2_SESSION_ID")
	if session == "" {
		return fail(exitUsage, "request-repo is for agents: run it from a Lazybox conversation (it asks the person on that conversation). To add a repository yourself, use Workspace → Add repositories")
	}
	query, err := c.repositoryQuery(positional[0])
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	r, err := got(cl.RequestRepository(c.ctx, connect.NewRequest(&pb.RequestRepositoryRequest{SessionId: session, Query: query, Reason: *reason})))
	if err != nil {
		return err
	}
	if r.State == "added" {
		return c.requestOutcome(r)
	}
	if !c.json {
		c.out("Asked the person to add %s to this workspace (branch %s). Request %s.\n", requestedName(r), r.Branch, r.Id)
		if len(r.Candidates) == 0 {
			c.out("No repository in their catalog matches %q; they can choose one.\n", r.Query)
		} else if len(r.Candidates) > 1 {
			c.out("Other matches they can pick instead: %s.\n", candidateNames(r.Candidates[1:]))
		}
	}
	if *noWait {
		if c.json {
			return c.printProtoJSON(r)
		}
		c.out("Not waiting. You will be told the answer in this conversation; `lazybox workspace repos` shows what is still waiting.\n")
		return nil
	}
	if !c.json {
		c.out("Waiting for their answer (up to %s)…\n", *wait)
	}
	deadline := c.now().Add(*wait)
	for r.State == "pending" {
		left := deadline.Sub(c.now())
		if left <= 0 {
			break
		}
		seconds := uint32(min(left, 55*time.Second) / time.Second)
		next, err := got(cl.WaitRepositoryRequest(c.ctx, connect.NewRequest(&pb.WaitRepositoryRequestRequest{RequestId: r.Id, TimeoutSeconds: max(seconds, 1)})))
		if err != nil {
			// The Runner may be restarting: the request outlives it.
			var ce *connect.Error
			if errors.As(err, &ce) && ce.Code() == connect.CodeUnavailable && c.ctx.Err() == nil {
				time.Sleep(min(time.Second, max(left, 0)))
				continue
			}
			return err
		}
		r = next
	}
	return c.requestOutcome(r)
}

func candidateNames(cs []*pb.RepositoryCandidate) string {
	names := make([]string, len(cs))
	for i, candidate := range cs {
		names[i] = candidate.Name
	}
	return strings.Join(names, ", ")
}

// requestOutcome prints the answer, with the exit status a script can test:
// 0 added, 4 declined, 5 no answer yet.
func (c *cli) requestOutcome(r *pb.RepositoryRequest) error {
	if c.json {
		if err := c.printProtoJSON(r); err != nil {
			return err
		}
	}
	switch r.State {
	case "added":
		if !c.json {
			if r.Already {
				c.out("%s is already in this workspace:\n", requestedName(r))
			} else {
				c.out("The person added it to this workspace:\n")
			}
			tw := tabwriter.NewWriter(c.stdout, 0, 0, 2, ' ', 0)
			for _, a := range r.Added {
				fmt.Fprintf(tw, "  %s\t%s\t(branch %s)\n", a.Name, a.Path, a.Branch)
			}
			_ = tw.Flush()
			if r.Already && len(r.Candidates) > 1 {
				c.out("If you meant another repository, ask for it by its full name: %s.\n", candidateNames(r.Candidates[1:]))
			}
		}
		return nil
	case "declined":
		why := ""
		if r.DeclineReason != "" {
			why = " Their reason: " + r.DeclineReason
		}
		return fail(exitDenied, "the person declined adding %s.%s\nDo not read or change it outside the workspace; continue without it, or ask them how to proceed.", requestedName(r), why)
	case "expired":
		return fail(exitTimeout, "the request for %s expired without an answer; ask again if you still need it", requestedName(r))
	}
	return fail(exitTimeout, "wait ended with no answer yet for request %s. The repository request remains pending; when the person answers you will be told in this conversation. This output describes the state when the wait ended. Check current status with lazybox workspace repos; do not submit a duplicate request.", r.Id)
}
