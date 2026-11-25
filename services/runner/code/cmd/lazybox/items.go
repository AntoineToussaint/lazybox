package main

import (
	"fmt"
	"strings"
	"time"
	"unicode"

	"connectrpc.com/connect"
	workitems "github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/items"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox link URL` and `lazybox items`: the pull requests a workspace
// delivers (GITHUB.md "Tracked pull requests"). An agent links a pull request
// it opened in a way Lazybox could not see, with its own scoped credential,
// to its own workspace only; it never unlinks, reacts or merges. A person
// names the workspace with --workspace.

const itemsUsage = `Usage:
  lazybox link <pull request or issue URL> [--role delivers|mentions] [--workspace W]
  lazybox items [--workspace W | --all]`

// looksLikeItemURL tells `lazybox link URL` from `lazybox link` (the CLI on
// PATH): a URL anywhere, with or without its scheme, or an item flag. Linking
// the CLI takes no URL, so its refusal outside a release never meets one.
func looksLikeItemURL(args []string) bool {
	for _, a := range args {
		name, _, _ := strings.Cut(strings.TrimLeft(a, "-"), "=")
		if strings.HasPrefix(a, "-") && (name == "role" || name == "workspace") {
			return true
		}
		if !strings.HasPrefix(a, "-") && (strings.Contains(a, "://") || strings.HasPrefix(strings.TrimPrefix(a, "www."), "github.com/")) {
			return true
		}
	}
	return false
}

// itemURL is the URL as the Runner resolves it: https, with its scheme.
func itemURL(arg string) string {
	arg = strings.TrimSpace(arg)
	if rest, ok := strings.CutPrefix(arg, "http://"); ok {
		arg = "https://" + rest
	}
	if !strings.Contains(arg, "://") {
		arg = "https://" + arg
	}
	return strings.Replace(arg, "https://www.github.com/", "https://github.com/", 1)
}

// insideLazybox: an agent, whose `lazybox link` can only mean an item (a
// person in a workspace terminal may still put the CLI on PATH).
func (c *cli) insideLazybox() bool {
	return c.getenv("LAZYBOX_V2_SESSION_ID") != ""
}

// itemScope is who asks: an agent's own execution, or the workspace a person names.
func (c *cli) itemScope(cl *client, workspace string) (session, collection string, err error) {
	if workspace != "" {
		k, err := loadCatalog(c.ctx, cl)
		if err != nil {
			return "", "", err
		}
		w, err := k.findWorkspace(workspace)
		if err != nil {
			return "", "", err
		}
		return "", w.Id, nil
	}
	if session := c.getenv("LAZYBOX_V2_SESSION_ID"); session != "" {
		return session, "", nil
	}
	if collection := c.getenv("LAZYBOX_V2_COLLECTION_ID"); collection != "" {
		return "", collection, nil
	}
	return "", "", fail(exitUsage, "name a workspace with --workspace (lazybox ls lists them); inside a Lazybox agent it is the one it works in")
}

func (c *cli) linkItem(args []string) error {
	f := c.flags("link")
	role := f.String("role", "", "delivers (work this workspace produced; tracked) or mentions (listed only); the default is delivers for a pull request")
	workspace := f.String("workspace", "", "the workspace to link it to (default: the one this agent works in)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		fmt.Fprintln(c.stderr, itemsUsage)
		return fail(exitUsage, "lazybox link takes one URL")
	}
	switch *role {
	case "", "delivers", "mentions":
	default:
		return fail(exitUsage, "--role is delivers or mentions")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	session, collection, err := c.itemScope(cl, *workspace)
	if err != nil {
		return err
	}
	url := itemURL(positional[0])
	links, err := got(cl.LinkItem(c.ctx, connect.NewRequest(&pb.LinkItemRequest{SessionId: session, CollectionId: collection, Url: url, Role: *role})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(links)
	}
	resolved, _ := workitems.ResolveGitHub(url)
	for _, l := range links.Links {
		if l.Item.GetRef().GetKey() == resolved.Ref.Key && l.Item.GetRef().GetProvider() == resolved.Ref.Provider {
			verb := "Tracking"
			if l.Role == "mentions" {
				verb = "Listed (not tracked)"
			}
			fmt.Fprintf(c.stdout, "%s %s in this workspace.\n", verb, itemName(l.Item))
			return nil
		}
	}
	fmt.Fprintf(c.stdout, "Linked %s.\n", url)
	return nil
}

func (c *cli) items(args []string) error {
	f := c.flags("items")
	workspace := f.String("workspace", "", "the workspace (default: the one this agent works in)")
	all := f.Bool("all", false, "all visible tracked items (inside an agent: its current workspace)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) > 0 {
		fmt.Fprintln(c.stderr, itemsUsage)
		return fail(exitUsage, "unexpected %q", strings.Join(positional, " "))
	}
	if *all && *workspace != "" {
		return fail(exitUsage, "choose either --all or --workspace")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	scopedAll := *all && c.insideLazybox()
	request := &pb.ListLinksRequest{All: *all && !scopedAll}
	if !request.All {
		if request.SessionId, request.CollectionId, err = c.itemScope(cl, *workspace); err != nil {
			return err
		}
	}
	links, err := got(cl.ListLinks(c.ctx, connect.NewRequest(request)))
	if err != nil {
		return err
	}
	if scopedAll {
		fmt.Fprintln(c.stderr, "Showing all items visible to this agent in its current workspace.")
	}
	if c.json {
		return c.printProtoJSON(links)
	}
	var delivered, mentioned []*pb.ItemLink
	for _, l := range links.Links {
		if l.Role == "mentions" {
			mentioned = append(mentioned, l)
		} else {
			delivered = append(delivered, l)
		}
	}
	if len(delivered) == 0 && len(mentioned) == 0 {
		fmt.Fprintln(c.stdout, "No pull requests tracked here yet. Lazybox finds the ones opened from this workspace's branches; `lazybox link URL` adds one.")
	}
	// Titles, check names and URLs are written by whoever can push to the
	// pull request: what reaches the terminal is printable text only.
	for _, l := range delivered {
		fmt.Fprintf(c.stdout, "%s  %s\n", itemName(l.Item), itemState(l.Item))
		if title := printable(l.Item.GetTitle()); title != "" {
			fmt.Fprintf(c.stdout, "    %s\n", title)
		}
		for _, check := range l.Item.GetPullRequest().GetFailingChecks() {
			fmt.Fprintf(c.stdout, "    ✕ %s %s\n", printable(check.Name), printable(check.Url))
		}
		where := l.Source
		if request.All && l.CollectionName != "" {
			where = l.CollectionName + " · " + where
		}
		fmt.Fprintf(c.stdout, "    %s  (%s)\n", printable(l.Item.GetUrl()), printable(where))
	}
	if len(mentioned) > 0 {
		fmt.Fprintln(c.stdout, "Mentioned (not tracked):")
		for _, l := range mentioned {
			name := l.Item.GetUrl()
			if name == "" {
				name = itemName(l.Item)
			}
			fmt.Fprintf(c.stdout, "  %s\n", printable(name))
		}
	}
	if links.TrackingError != "" {
		fmt.Fprintf(c.stderr, "[lazybox] %s\n", printable(links.TrackingError))
	}
	return nil
}

func itemName(item *pb.Item) string {
	return printable(item.GetRef().GetKey())
}

// printable is s as one line of plain text for a terminal: control
// characters (escape sequences, carriage returns, C1 controls) and
// bidirectional overrides are dropped, line and paragraph separators become
// spaces, so text from GitHub cannot move the cursor, rewrite earlier lines
// or change the terminal's state.
func printable(s string) string {
	return strings.Map(func(r rune) rune {
		switch {
		case r == '\t' || r == '\n' || r == '\u2028' || r == '\u2029':
			return ' '
		case unicode.IsControl(r), unicode.Is(unicode.Bidi_Control, r):
			return -1
		}
		return r
	}, s)
}

// itemState says, in words, where a pull request stands.
func itemState(item *pb.Item) string {
	pr := item.GetPullRequest()
	if pr == nil || item.GetState() == nil {
		return "not read yet"
	}
	var parts []string
	switch item.GetState().GetCategory() {
	case "merged":
		return "merged" + closedSince(pr.ClosedAtUnixMillis)
	case "closed":
		return "closed" + closedSince(pr.ClosedAtUnixMillis)
	case "started":
		parts = append(parts, "draft")
	default:
		parts = append(parts, "open")
	}
	switch pr.Checks {
	case "failure":
		parts = append(parts, fmt.Sprintf("checks failing (%d of %d)", len(pr.FailingChecks), pr.ChecksTotal))
	case "pending":
		parts = append(parts, "checks running")
	case "success":
		parts = append(parts, "checks passed")
	}
	switch pr.Review {
	case "changes_requested":
		parts = append(parts, "changes requested")
	case "approved":
		parts = append(parts, "approved")
	case "review_required":
		parts = append(parts, "review required")
	}
	if pr.MergeableState == "dirty" {
		parts = append(parts, "conflicts")
	}
	return strings.Join(parts, " · ")
}

func closedSince(at int64) string {
	if at <= 0 {
		return ""
	}
	return " " + time.UnixMilli(at).Local().Format("Jan 2 15:04")
}
