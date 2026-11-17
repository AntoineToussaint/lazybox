package main

import (
	"fmt"
	"strings"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox new --from <url|owner/name#N> ["message"]` starts a workspace from
// a work item (PROJECTS_AND_INTEGRATIONS.md): the workspace in the item's own
// repository, on a branch named after it, with the item as its origin; then a
// conversation whose first message carries the item's text and recent
// comments, quoted as external content, after the person's own words.

type fromItem struct {
	reference, name, branch, profile, permission string
	repos                                        []string
	another, noComments                          bool
	message                                      string
}

func (c *cli) newFromItem(o fromItem) error {
	switch o.permission {
	case "ask", "read-only", "yolo":
	default:
		return fail(exitUsage, "--permission is ask, read-only or yolo")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	ref, err := got(cl.ResolveItemUrl(c.ctx, connect.NewRequest(&pb.ResolveItemUrlRequest{Url: o.reference})))
	if err != nil {
		return cl.explain(err)
	}
	comments := uint32(0)
	if o.noComments {
		comments = 1 // the fewest; the context without comments is what is sent
	}
	detail, err := got(cl.GetItem(c.ctx, connect.NewRequest(&pb.ItemRequest{Ref: ref, Comments: comments})))
	if err != nil {
		return err
	}
	item := detail.Item
	// Already started? Say where, unless another is asked for.
	started, err := got(cl.FindItemWorkspaces(c.ctx, connect.NewRequest(&pb.ItemRequest{Ref: ref})))
	if err != nil {
		return err
	}
	if len(started.Workspaces) > 0 && !o.another {
		w := started.Workspaces[0]
		return fail(exitFailure, "%s already has a workspace, “%s” (lazybox open %s). Pass --another to start another", ref.Key, w.Name, w.Id)
	}
	// The repository: the item's own unless --repo names others.
	catalog, err := got(cl.GetRepositoryCatalog(c.ctx, connect.NewRequest(&pb.GetRepositoryCatalogRequest{})))
	if err != nil {
		return err
	}
	var identities []string
	for _, r := range o.repos {
		entry, err := findRepository(catalog.Entries, r)
		if err != nil {
			return err
		}
		identities = append(identities, entry.Identity)
	}
	if len(identities) == 0 && item.Repository != "" {
		known := false
		for _, e := range catalog.Entries {
			known = known || e.Identity == item.Repository
		}
		if !known {
			return fail(exitNotFound, "%s is not on this Runner. Add it in Settings → Repositories (or refresh the GitHub list there), or name another with --repo", strings.TrimPrefix(item.Repository, "github:"))
		}
		identities = []string{item.Repository}
	}
	profile := o.profile
	if profile == "" {
		info, err := got(cl.Info(c.ctx, connect.NewRequest(&pb.InfoRequest{})))
		if err != nil {
			return err
		}
		profile = defaultProfile(info.Profiles)
		if profile == "" {
			return fail(exitFailure, "no agent subscription on this Runner: add one in Settings → Subscriptions")
		}
	}
	stream, err := cl.PrepareWorkspace(c.ctx, connect.NewRequest(&pb.PrepareWorkspaceRequest{RequestKey: commands.ID(), Name: o.name, RepositoryIdentities: identities, Branch: o.branch, Item: ref}))
	if err != nil {
		return cl.explain(err)
	}
	defer stream.Close()
	var created *pb.Collection
	for stream.Receive() {
		m := stream.Msg()
		if m.Message != "" {
			c.note("%s\n", m.Message)
		}
		if m.Collection != nil {
			created = m.Collection
		}
	}
	if err = stream.Err(); err != nil {
		return cl.explain(err)
	}
	if created == nil {
		return fail(exitFailure, "the Runner did not report the new workspace")
	}
	// The agent asks before acting unless the person chose otherwise: item text is untrusted.
	opened, err := got(cl.OpenSession(c.ctx, connect.NewRequest(&pb.OpenSessionRequest{ProfileId: profile, CollectionId: created.Id, Options: &pb.RunnerOptions{Permission: o.permission, Workflow: "execute"}})))
	if err != nil {
		return err
	}
	context := detail.Context
	if o.noComments {
		context = detail.ContextWithoutComments
	}
	text := itemMessage(o.message, item, context)
	if _, err = got(cl.Apply(c.ctx, connect.NewRequest(&pb.ApplyRequest{SessionId: opened.Session.Id, OperationId: commands.ID(), Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: text}}}))); err != nil {
		return err
	}
	branch := ""
	for _, m := range created.Members {
		if m.Spec != nil && m.Spec.Branch != "" {
			branch = m.Spec.Branch
		}
	}
	if c.json {
		return c.printJSON(map[string]any{"id": created.Id, "name": created.Name, "root": created.Root, "repositories": repositories(created), "branch": branch, "item": ref.Key, "session": opened.Session.Id})
	}
	c.out("%s\n", created.Id)
	c.note("Started “%s” from %s on %s; the agent has the issue. Follow it with: lazybox watch %s\n", created.Name, ref.Key, branch, opened.Session.Id)
	return nil
}

// itemMessage is the first message: the person's words (or a plain request to
// work on the item), then the item's quoted context.
func itemMessage(message string, item *pb.Item, context string) string {
	message = strings.TrimSpace(message)
	if message == "" {
		noun := "issue"
		if item.Kind == "pull_request" {
			noun = "pull request"
		}
		message = fmt.Sprintf("Work on %s %s: %s.", noun, item.Ref.Key, strings.TrimRight(item.Title, "."))
	}
	if context == "" {
		return message
	}
	return message + "\n\n" + context
}

// defaultProfile is the agent subscription a start uses without --profile:
// Claude's managed one, else Claude's first, else the first.
func defaultProfile(profiles []*pb.Profile) string {
	for _, p := range profiles {
		if p.Provider == "claude" && p.Managed {
			return p.Id
		}
	}
	for _, p := range profiles {
		if p.Provider == "claude" {
			return p.Id
		}
	}
	if len(profiles) > 0 {
		return profiles[0].Id
	}
	return ""
}
