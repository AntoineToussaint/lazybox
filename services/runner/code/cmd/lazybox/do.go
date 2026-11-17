package main

import (
	"slices"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// do runs an action of the app's registry in the open window (`lazybox do
// layout.maximize`), or one of the person's own actions by its id. --list
// prints the registry: every action, its default keys, its argument, and
// whether a hook may run it (POWER.md, "One command plane").
func (c *cli) do(args []string) error {
	f := c.flags("do")
	list := f.Bool("list", false, "list the actions (words after it filter)")
	workspaceRef := f.String("workspace", "", "show this workspace first")
	conversationRef := f.String("conversation", "", "show this conversation first")
	timeout := f.Duration("timeout", 30*time.Second, "how long to wait for the window")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	registry := power.Registry()
	if *list {
		return c.listRegistry(positional)
	}
	if len(positional) < 1 || len(positional) > 2 {
		return fail(exitUsage, "usage: lazybox do <action> [argument] [--workspace W] [--conversation C]; lazybox do --list")
	}
	id, argument := positional[0], ""
	if len(positional) == 2 {
		argument = positional[1]
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	request := &pb.UiRequest{Kind: power.UIDo, ActionId: id, Argument: argument}
	if a, isApp := registry.Action(id); !isApp {
		// One of the person's own actions (actions.yaml) runs the same way.
		r, err := got(cl.GetUserActions(c.ctx, connect.NewRequest(&pb.GetUserActionsRequest{})))
		if err != nil || !slices.ContainsFunc(r.Actions, func(u *pb.UserAction) bool { return u.Id == id }) {
			hint := ""
			if near := power.Suggest(id, registry.ActionIDs()); near != "" {
				hint = " (did you mean " + near + "?)"
			}
			return fail(exitNotFound, "the app has no action %q%s; lazybox do --list lists them", id, hint)
		}
		if argument != "" {
			return fail(exitUsage, "your actions take no argument")
		}
		request.Kind = power.UIAction
	} else if argument != "" && a.Argument == "" {
		return fail(exitUsage, "%s takes no argument", id)
	}
	if *workspaceRef != "" || *conversationRef != "" {
		k, err := loadCatalog(c.ctx, cl)
		if err != nil {
			return err
		}
		if *workspaceRef != "" {
			w, err := k.findWorkspace(*workspaceRef)
			if err != nil {
				return err
			}
			request.CollectionId = w.Id
		}
		if *conversationRef != "" {
			v, err := k.findConversation(*conversationRef)
			if err != nil {
				return err
			}
			request.CollectionId, request.SessionId = v.Head.CollectionId, v.Head.Id
		}
	}
	answer, err := c.ask(cl, request, *timeout)
	if err != nil {
		return err
	}
	if c.json {
		return c.printJSON(map[string]any{"id": answer.Id, "action": id, "detail": answer.Detail})
	}
	detail := answer.Detail
	if detail == "" {
		detail = "Ran " + id + "."
	}
	c.out("%s\n", detail)
	return nil
}

// listRegistry prints the app's actions by group, filtered by any words given.
func (c *cli) listRegistry(words []string) error {
	registry := power.Registry()
	keymap := registry.Keymap("vim")
	match := func(a power.RegistryAction) bool {
		text := strings.ToLower(a.ID + " " + a.Title + " " + a.Group)
		for _, w := range words {
			if !strings.Contains(text, strings.ToLower(w)) {
				return false
			}
		}
		return true
	}
	if c.json {
		out := []map[string]any{}
		for _, a := range registry.Actions {
			if match(a) {
				out = append(out, map[string]any{"id": a.ID, "title": a.Title, "group": a.Group, "keys": keymap[a.ID], "safe": a.Safe, "argument": a.Argument})
			}
		}
		return c.printJSON(out)
	}
	group := ""
	for _, a := range registry.Actions {
		if !match(a) {
			continue
		}
		if a.Group != group {
			group = a.Group
			c.out("%s\n", c.paint(bold, group))
		}
		extra := ""
		if a.Argument != "" {
			extra = c.paint(dim, "  ["+a.Argument+"]")
		}
		safe := "    "
		if a.Safe {
			safe = c.paint(green, "safe")
		}
		c.out("  %-28s %s  %-46s %s%s\n", a.ID, safe, oneLine(a.Title, 46), c.paint(cyan, keymap[a.ID]), extra)
	}
	c.out("%s\n", c.paint(dim, "safe: a hook with the respond grant may run it. Keys are the defaults; yours: lazybox config get keys ID."))
	return nil
}
