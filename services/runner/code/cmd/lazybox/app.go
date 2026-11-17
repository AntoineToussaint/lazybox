package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Verbs that reach the person's open Lazybox window, the actions file, hooks,
// missions and workspaces.

// ask posts a request to the window in front and waits for its answer.
func (c *cli) ask(cl *client, request *pb.UiRequest, timeout time.Duration) (*pb.UiRequest, error) {
	posted, err := cl.PostUiRequest(c.ctx, connect.NewRequest(request))
	if err != nil {
		// No window open: Unavailable, which explain maps to exit 6 with the Runner's words.
		return nil, cl.explain(err)
	}
	deadline := c.now().Add(timeout)
	for {
		left := time.Until(deadline)
		if left <= 0 {
			return nil, fail(exitTimeout, "the Lazybox window did not answer within %s", timeout)
		}
		wait := min(left, 2*time.Minute)
		got, err := got(cl.WaitUiRequest(c.ctx, connect.NewRequest(&pb.WaitUiRequestRequest{Id: posted.Msg.Id, TimeoutMs: uint32(wait / time.Millisecond)})))
		if err != nil {
			return nil, err
		}
		switch got.State {
		case power.UIDone:
			return got, nil
		case power.UIFailed:
			return got, fail(exitFailure, "%s", got.Error)
		}
	}
}

// fileTarget matches "path/to/file:12" or "file.go".
var fileTarget = regexp.MustCompile(`^(.*?)(?::(\d+))?$`)

func (c *cli) open(args []string) error {
	f := c.flags("open")
	panel := f.String("panel", "", "also show this panel ("+strings.Join(panelKinds, ", ")+")")
	workspace := f.String("workspace", "", "the workspace a file belongs to (default: the one the window shows)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox open <workspace|conversation|file[:line]> [--panel KIND]")
	}
	if *panel != "" && !slices.Contains(panelKinds, *panel) {
		return fail(exitUsage, "--panel is one of %s", strings.Join(panelKinds, ", "))
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	k, err := loadCatalog(c.ctx, cl)
	if err != nil {
		return err
	}
	ref := positional[0]
	request := &pb.UiRequest{Kind: power.UIOpen, Panel: *panel}
	if *workspace != "" {
		w, err := k.findWorkspace(*workspace)
		if err != nil {
			return err
		}
		request.CollectionId = w.Id
	}
	if w, err := k.findWorkspace(ref); err == nil && !strings.Contains(ref, "/") {
		request.CollectionId = w.Id
	} else if v, convErr := k.findConversation(ref); convErr == nil {
		request.CollectionId, request.SessionId = v.Head.CollectionId, v.Head.Id
	} else if looksLikeFile(ref) {
		m := fileTarget.FindStringSubmatch(ref)
		request.File = m[1]
		if m[2] != "" {
			line, _ := strconv.Atoi(m[2])
			request.Line = uint32(line)
		}
		// A file belongs to a workspace: --workspace, this terminal's, or the only one.
		if request.CollectionId == "" {
			request.CollectionId = c.getenv("LAZYBOX_V2_COLLECTION_ID")
		}
		if request.CollectionId == "" && len(k.Workspaces) == 1 {
			request.CollectionId = k.Workspaces[0].Id
		}
		if request.CollectionId == "" {
			return fail(exitUsage, "which workspace is %s in? Add --workspace NAME", request.File)
		}
	} else {
		var coded exitError
		if errors.As(convErr, &coded) && coded.code == exitUsage {
			return convErr
		}
		return fail(exitNotFound, "no workspace, conversation or file %q", ref)
	}
	got, err := c.ask(cl, request, 30*time.Second)
	if err != nil {
		return err
	}
	if c.json {
		return c.printJSON(map[string]any{"id": got.Id, "detail": got.Detail, "workspace": request.CollectionId, "session": request.SessionId, "file": request.File, "line": request.Line})
	}
	detail := got.Detail
	if detail == "" {
		detail = "Shown in the Lazybox window."
	}
	c.out("%s\n", detail)
	return nil
}

func looksLikeFile(ref string) bool {
	return strings.ContainsAny(ref, "/.") || regexp.MustCompile(`:\d+$`).MatchString(ref)
}

func (c *cli) newWorkspace(args []string) error {
	f := c.flags("new")
	var repos multiFlag
	f.Var(&repos, "repo", "a repository: owner/name, its catalog name, identity or path (repeat for several)")
	branch := f.String("branch", "", "the branch for every repository (default: the Runner's)")
	from := f.String("from", "", "start from an issue: its link or owner/name#N (the workspace, its branch and the agent's first message come from it)")
	name := f.String("name", "", "with --from: the workspace's name (default: the issue's title)")
	profile := f.String("profile", "", "with --from: the agent subscription (default: Claude's)")
	permission := f.String("permission", "ask", "with --from: ask, read-only or yolo")
	another := f.Bool("another", false, "with --from: start another workspace when the issue already has one")
	noComments := f.Bool("no-comments", false, "with --from: leave the issue's comments out of the agent's first message")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if *from != "" {
		if len(positional) > 1 {
			return fail(exitUsage, "usage: lazybox new --from <url|owner/name#N> [MESSAGE] (quote the message)")
		}
		message := ""
		if len(positional) == 1 {
			message = positional[0]
		}
		return c.newFromItem(fromItem{reference: *from, name: *name, branch: *branch, profile: *profile, permission: *permission, repos: repos, another: *another, noComments: *noComments, message: message})
	}
	if len(positional) != 1 || len(repos) == 0 {
		return fail(exitUsage, "usage: lazybox new <workspace-name> --repo R [--repo R…] [--branch B]")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	catalog, err := got(cl.GetRepositoryCatalog(c.ctx, connect.NewRequest(&pb.GetRepositoryCatalogRequest{})))
	if err != nil {
		return err
	}
	var identities []string
	for _, r := range repos {
		entry, err := findRepository(catalog.Entries, r)
		if err != nil {
			return err
		}
		identities = append(identities, entry.Identity)
	}
	stream, err := cl.PrepareWorkspace(c.ctx, connect.NewRequest(&pb.PrepareWorkspaceRequest{RequestKey: commands.ID(), Name: positional[0], RepositoryIdentities: identities, Branch: *branch}))
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
	if c.json {
		return c.printJSON(map[string]any{"id": created.Id, "name": created.Name, "root": created.Root, "repositories": repositories(created)})
	}
	c.out("%s\n", created.Id)
	c.note("Created workspace “%s” at %s. Start a conversation in the app, or: lazybox open %s\n", created.Name, created.Root, created.Id)
	return nil
}

type multiFlag []string

func (m *multiFlag) String() string     { return strings.Join(*m, ",") }
func (m *multiFlag) Set(v string) error { *m = append(*m, v); return nil }

func findRepository(entries []*pb.RepositoryCatalogEntry, ref string) (*pb.RepositoryCatalogEntry, error) {
	lower := strings.ToLower(strings.TrimSpace(ref))
	if abs, err := filepath.Abs(ref); err == nil && (strings.HasPrefix(ref, ".") || strings.HasPrefix(ref, "/")) {
		lower = strings.ToLower(abs)
	}
	var matches []*pb.RepositoryCatalogEntry
	for _, e := range entries {
		for _, candidate := range []string{e.Identity, e.Name, e.RepositoryId, strings.TrimPrefix(e.Identity, "github:"), strings.TrimPrefix(e.Identity, "local:")} {
			if candidate != "" && strings.ToLower(candidate) == lower {
				matches = append(matches, e)
				break
			}
		}
	}
	if len(matches) == 0 {
		for _, e := range entries {
			if strings.HasSuffix(strings.ToLower(e.Name), "/"+lower) {
				matches = append(matches, e)
			}
		}
	}
	switch len(matches) {
	case 0:
		return nil, fail(exitNotFound, "no repository %q in the Runner's catalog (Settings → Repositories adds one)", ref)
	case 1:
		return matches[0], nil
	}
	names := make([]string, len(matches))
	for i, m := range matches {
		names[i] = m.Identity
	}
	return nil, fail(exitUsage, "%q names several repositories: %s", ref, strings.Join(names, ", "))
}

func (c *cli) missions(args []string) error {
	f := c.flags("missions")
	brief := f.String("brief", "", "new: what the mission is about")
	note := f.String("note", "", "accept/reject: a note recorded with the decision")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	sub := "ls"
	if len(positional) > 0 {
		sub, positional = positional[0], positional[1:]
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	switch sub {
	case "ls":
		r, err := got(cl.ListMissions(c.ctx, connect.NewRequest(&pb.ListMissionsRequest{})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(r)
		}
		if len(r.Missions) == 0 {
			c.out("No missions yet. lazybox missions new TITLE --brief …\n")
			return nil
		}
		for _, m := range r.Missions {
			counts := map[string]int{}
			waiting := 0
			for _, t := range m.Tasks {
				counts[t.Status]++
				if t.Gate != "" {
					waiting++
				}
			}
			var parts []string
			for _, s := range sortedKeys(counts) {
				parts = append(parts, fmt.Sprintf("%d %s", counts[s], s))
			}
			extra := ""
			if waiting > 0 {
				extra = c.paint(yellow, fmt.Sprintf("  %d awaiting acceptance", waiting))
			}
			if m.Canceled {
				extra += c.paint(dim, "  canceled")
			}
			c.out("%s  %s  %s%s\n", c.paint(dim, m.Id), c.paint(bold, m.Title), strings.Join(parts, " · "), extra)
		}
		return nil
	case "show":
		if len(positional) != 1 {
			return fail(exitUsage, "usage: lazybox missions show ID")
		}
		m, err := got(cl.GetMission(c.ctx, connect.NewRequest(&pb.GetMissionRequest{MissionId: positional[0]})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(m)
		}
		c.out("%s  %s\n", c.paint(bold, m.Title), c.paint(dim, m.Id))
		if m.Brief != "" {
			c.out("%s\n", m.Brief)
		}
		for _, t := range m.Tasks {
			line := fmt.Sprintf("  %-10s %s  %s", t.Status, t.Title, c.paint(dim, t.Id))
			if t.Gate != "" {
				line += c.paint(yellow, "  awaiting "+t.Gate+": lazybox missions accept "+m.Id+" "+t.Id)
			}
			c.out("%s\n", line)
			if t.Reason != "" {
				c.out("             %s\n", c.paint(dim, t.Reason))
			}
		}
		return nil
	case "new":
		if len(positional) == 0 {
			return fail(exitUsage, "usage: lazybox missions new TITLE --brief …")
		}
		m, err := got(cl.CreateMission(c.ctx, connect.NewRequest(&pb.CreateMissionRequest{RequestKey: commands.ID(), Title: strings.Join(positional, " "), Brief: *brief})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(m)
		}
		c.out("%s\n", m.Id)
		return nil
	case "accept", "reject":
		if len(positional) != 2 {
			return fail(exitUsage, "usage: lazybox missions %s MISSION TASK [--note …]", sub)
		}
		r := &pb.UpdateMissionRequest{MissionId: positional[0], OperationId: commands.ID()}
		decision := &pb.GateDecision{TaskId: positional[1], Note: *note}
		if sub == "accept" {
			r.Change = &pb.UpdateMissionRequest_Accept{Accept: decision}
		} else {
			r.Change = &pb.UpdateMissionRequest_Reject{Reject: decision}
		}
		m, err := got(cl.UpdateMission(c.ctx, connect.NewRequest(r)))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(m)
		}
		c.out("%s task %s of “%s”.\n", map[string]string{"accept": "Accepted", "reject": "Rejected"}[sub], positional[1], m.Title)
		return nil
	}
	return fail(exitUsage, "usage: lazybox missions [ls|show ID|new TITLE|accept ID TASK|reject ID TASK]")
}

func sortedKeys(m map[string]int) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	slices.Sort(keys)
	return keys
}

func (c *cli) shot(args []string) error {
	f := c.flags("shot")
	out := f.String("out", "", "where to write the PNG (default ./lazybox-shot-<time>.png; - for stdout)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	panel := ""
	if len(positional) > 1 {
		return fail(exitUsage, "usage: lazybox shot [window|PANEL] [--out FILE]")
	}
	if len(positional) == 1 && positional[0] != "window" {
		panel = positional[0]
		if !slices.Contains(panelKinds, panel) {
			return fail(exitUsage, "%q is not a panel; one of window, %s", panel, strings.Join(panelKinds, ", "))
		}
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	got, err := c.ask(cl, &pb.UiRequest{Kind: power.UIShot, Panel: panel}, 30*time.Second)
	if err != nil {
		return err
	}
	if len(got.Png) == 0 {
		return fail(exitFailure, "the window returned no image")
	}
	path := *out
	if path == "-" {
		_, err = c.stdout.Write(got.Png)
		return err
	}
	if path == "" {
		path = "lazybox-shot-" + c.now().Format("20060102-150405") + ".png"
	}
	if err = os.WriteFile(path, got.Png, 0o600); err != nil {
		return err
	}
	abs, _ := filepath.Abs(path)
	if c.json {
		return c.printJSON(map[string]any{"path": abs, "bytes": len(got.Png), "panel": panel, "detail": got.Detail})
	}
	c.out("%s\n", abs)
	return nil
}

// defaultActionsFile is the person's file when no Runner says otherwise.
func (c *cli) defaultActionsFile() string { return filepath.Join(configDir(c.getenv), "actions.yaml") }

func (c *cli) actions(args []string) error {
	f := c.flags("actions")
	conversationRef := f.String("conversation", "", "run: the conversation the action works on (default: the one the window shows)")
	timeout := f.Duration("timeout", 5*time.Minute, "run: how long to wait for the action to finish")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	sub := "ls"
	if len(positional) > 0 {
		sub, positional = positional[0], positional[1:]
	}
	switch sub {
	case "schema":
		_, err := io.WriteString(c.stdout, power.Schema)
		return err
	case "validate":
		return c.validateActions(positional)
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	switch sub {
	case "path":
		r, err := got(cl.GetUserActions(c.ctx, connect.NewRequest(&pb.GetUserActionsRequest{})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printJSON(map[string]any{"path": r.Path, "exists": r.Exists, "schemaPath": r.SchemaPath})
		}
		c.out("%s\n", r.Path)
		return nil
	case "init":
		r, err := got(cl.InitUserActions(c.ctx, connect.NewRequest(&pb.GetUserActionsRequest{})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(r)
		}
		c.out("%s\n", r.Path)
		c.note("Your actions file is ready, with its JSON Schema beside it (%s). Lazybox reloads it when you save.\n", r.SchemaPath)
		return nil
	case "ls":
		r, err := got(cl.GetUserActions(c.ctx, connect.NewRequest(&pb.GetUserActionsRequest{})))
		if err != nil {
			return err
		}
		if c.json {
			r.Schema = ""
			return c.printProtoJSON(r)
		}
		c.printActions(r)
		return nil
	case "run":
		if len(positional) != 1 {
			return fail(exitUsage, "usage: lazybox actions run ID [--conversation C]")
		}
		request := &pb.UiRequest{Kind: power.UIAction, ActionId: positional[0]}
		if *conversationRef != "" {
			k, err := loadCatalog(c.ctx, cl)
			if err != nil {
				return err
			}
			v, err := k.findConversation(*conversationRef)
			if err != nil {
				return err
			}
			request.SessionId, request.CollectionId = v.Head.Id, v.Head.CollectionId
		}
		got, err := c.ask(cl, request, *timeout)
		if err != nil {
			return err
		}
		if c.json {
			return c.printJSON(map[string]any{"id": got.Id, "action": positional[0], "detail": got.Detail})
		}
		detail := got.Detail
		if detail == "" {
			detail = "Ran " + positional[0] + "."
		}
		c.out("%s\n", detail)
		return nil
	}
	return fail(exitUsage, "usage: lazybox actions [ls|validate [FILE]|schema|path|init|run ID]")
}

func (c *cli) printActions(r *pb.UserActions) {
	if !r.Exists {
		c.out("No actions file yet at %s. `lazybox actions init` creates one with examples.\n", r.Path)
		return
	}
	c.out("%s %s\n", c.paint(bold, r.Path), c.paint(dim, fmt.Sprintf("revision %d", r.Revision)))
	if r.Error != "" {
		stale := ""
		if r.Stale {
			stale = " (using the last version that loaded)"
		}
		c.out("%s\n", c.paint(red, "✗ "+r.Error+stale))
	}
	for _, i := range r.Issues {
		colour := yellow
		if i.Severity == "error" {
			colour = red
		}
		c.out("%s\n", c.paint(colour, fmt.Sprintf("%s line %d (%s): %s", i.Severity, i.Line, i.Path, i.Message)))
	}
	if len(r.Actions) > 0 {
		c.out("%s\n", c.paint(bold, "Actions"))
	}
	for _, a := range r.Actions {
		key := "   "
		if a.Key != "" {
			key = "]]" + a.Key
		}
		var steps []string
		for _, s := range a.Steps {
			steps = append(steps, s.Kind)
		}
		c.out("  %s  %-20s %s  %s\n", c.paint(cyan, fmt.Sprintf("%-5s", key)), a.Id, a.Title, c.paint(dim, strings.Join(steps, " → ")))
	}
	if len(r.Hooks) > 0 {
		c.out("%s\n", c.paint(bold, "Hooks"))
	}
	for _, h := range r.Hooks {
		c.printHook(h)
	}
}

func (c *cli) printHook(h *pb.UserHook) {
	target := strings.Join(h.Run, " ")
	switch {
	case h.Webhook != "":
		target = "POST " + h.Webhook
	case h.Shell != "":
		target = h.Shell
	}
	off := ""
	if !h.Enabled {
		off = c.paint(dim, " (off)")
	}
	grants := ""
	if len(h.Grants) > 0 {
		grants = c.paint(yellow, " grants "+strings.Join(h.Grants, ","))
	}
	c.out("  %-20s on %s → %s%s%s\n", h.Id, strings.Join(h.On, ", "), oneLine(target, 60), grants, off)
}

// validateActions checks a file locally, exactly as the Runner parses it.
func (c *cli) validateActions(positional []string) error {
	if len(positional) > 1 {
		return fail(exitUsage, "usage: lazybox actions validate [FILE]")
	}
	path := ""
	if len(positional) == 1 {
		path = positional[0]
	} else if t, err := resolveTarget(c.getenv, c.endpoint, c.runner); err == nil {
		cl := dial(t)
		ctx, cancel := context.WithTimeout(c.ctx, probeTimeout)
		if r, err := cl.GetUserActions(ctx, connect.NewRequest(&pb.GetUserActionsRequest{})); err == nil {
			path = r.Msg.Path
		}
		cancel()
	}
	if path == "" {
		path = c.defaultActionsFile()
	}
	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return fail(exitNotFound, "%s does not exist (lazybox actions init creates it)", path)
	}
	if err != nil {
		return err
	}
	file, issues, parseErr := power.Parse(data)
	errorsFound := parseErr != nil
	for _, i := range issues {
		if i.Severity == "error" {
			errorsFound = true
		}
	}
	if c.json {
		out := map[string]any{"path": path, "valid": !errorsFound, "actions": len(file.Actions), "hooks": len(file.Hooks), "issues": issues}
		if parseErr != nil {
			out["error"] = parseErr.Error()
		}
		if err := c.printJSON(out); err != nil {
			return err
		}
	} else {
		if parseErr != nil {
			c.out("%s: %s\n", path, c.paint(red, parseErr.Error()))
		}
		for _, i := range issues {
			colour := yellow
			if i.Severity == "error" {
				colour = red
			}
			c.out("%s:%d: %s %s (%s)\n", path, i.Line, c.paint(colour, i.Severity+":"), i.Message, i.Path)
		}
		if !errorsFound {
			c.out("%s: %d actions, %d hooks %s\n", path, len(file.Actions), len(file.Hooks), c.paint(green, "✓"))
		}
	}
	if errorsFound {
		return exitError{exitUsage, errors.New("the actions file has errors")}
	}
	return nil
}

func (c *cli) hooks(args []string) error {
	f := c.flags("hooks")
	limit := f.Int("limit", 20, "runs: how many")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	sub := "ls"
	if len(positional) > 0 {
		sub, positional = positional[0], positional[1:]
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	switch sub {
	case "ls":
		r, err := got(cl.GetUserActions(c.ctx, connect.NewRequest(&pb.GetUserActionsRequest{})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printJSON(map[string]any{"path": r.Path, "events": r.Events, "hooks": protoList(r.Hooks)})
		}
		if len(r.Hooks) == 0 {
			c.out("No hooks in %s. Events: %s\n", r.Path, strings.Join(r.Events, ", "))
			return nil
		}
		for _, h := range r.Hooks {
			c.printHook(h)
		}
		return nil
	case "runs":
		r, err := got(cl.ListHookRuns(c.ctx, connect.NewRequest(&pb.ListHookRunsRequest{Limit: uint32(max(*limit, 1))})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(r)
		}
		if len(r.Runs) == 0 {
			c.out("No hook has run since the Runner started.\n")
			return nil
		}
		for _, run := range r.Runs {
			c.printRun(run)
		}
		return nil
	case "test":
		if len(positional) != 1 {
			return fail(exitUsage, "usage: lazybox hooks test ID")
		}
		run, err := got(cl.TestHook(c.ctx, connect.NewRequest(&pb.TestHookRequest{HookId: positional[0]})))
		if err != nil {
			return err
		}
		// Follow it to the end, then show its output.
		deadline := c.now().Add(11 * time.Minute)
		for run.State == power.RunRunning && c.now().Before(deadline) {
			select {
			case <-c.ctx.Done():
				return c.ctx.Err()
			case <-time.After(c.poll):
			}
			if latest := c.findRun(cl, run.Id); latest != nil {
				run = latest
			}
		}
		if c.json {
			if err := c.printProtoJSON(run); err != nil {
				return err
			}
		} else {
			c.printRun(run)
			c.printRunOutput(cl, run)
		}
		if run.State != power.RunSucceeded {
			return exitError{exitFailure, fmt.Errorf("hook %s %s", run.HookId, run.State)}
		}
		return nil
	case "log":
		if len(positional) != 1 {
			return fail(exitUsage, "usage: lazybox hooks log RUN")
		}
		run := c.findRun(cl, positional[0])
		if run == nil {
			return fail(exitNotFound, "no recent hook run %q (lazybox hooks runs)", positional[0])
		}
		if run.CommandId == "" {
			return fail(exitNotFound, "run %s has no output: %s", run.Id, run.Error)
		}
		return c.copyCommand(cl, run.CommandId, c.stdout)
	}
	return fail(exitUsage, "usage: lazybox hooks [ls|runs|test ID|log RUN]")
}

func protoList[T any](items []T) []T {
	if items == nil {
		return []T{}
	}
	return items
}

func (c *cli) findRun(cl *client, id string) *pb.HookRun {
	r, err := cl.ListHookRuns(c.ctx, connect.NewRequest(&pb.ListHookRunsRequest{Limit: 100}))
	if err != nil {
		return nil
	}
	for _, run := range r.Msg.Runs {
		if run.Id == id || strings.HasPrefix(run.Id, id) && len(id) >= 6 {
			return run
		}
	}
	return nil
}

func (c *cli) printRun(run *pb.HookRun) {
	colour := map[string]string{power.RunSucceeded: green, power.RunFailed: red, power.RunRunning: cyan, power.RunSkipped: dim}[run.State]
	when := ""
	if run.StartedAtUnixMillis > 0 {
		when = time.UnixMilli(run.StartedAtUnixMillis).Local().Format("15:04:05")
	}
	detail := run.Error
	if run.HttpStatus > 0 {
		detail = strings.TrimSpace(fmt.Sprintf("HTTP %d %s", run.HttpStatus, detail))
	}
	test := ""
	if run.Test {
		test = c.paint(dim, " (test)")
	}
	c.out("%s  %s  %-16s %-18s %s%s  %s  %s\n", c.paint(dim, when), c.paint(colour, fmt.Sprintf("%-9s", run.State)), run.HookId, run.Event, oneLine(run.Summary, 50), test, c.paint(dim, run.Id), detail)
}

func (c *cli) printRunOutput(cl *client, run *pb.HookRun) {
	if run.CommandId == "" {
		return
	}
	c.out("%s\n", c.paint(dim, "── output ──"))
	_ = c.copyCommand(cl, run.CommandId, c.stdout)
}

// copyCommand writes a command's retained output.
func (c *cli) copyCommand(cl *client, id string, w io.Writer) error {
	offset := uint64(0)
	for {
		r, err := got(cl.ReadCommand(c.ctx, connect.NewRequest(&pb.ReadCommandRequest{CommandId: id, Offset: offset, Limit: 64 << 10})))
		if err != nil {
			return err
		}
		if len(r.Data) == 0 {
			return nil
		}
		if _, err = w.Write(r.Data); err != nil {
			return err
		}
		offset = r.NextOffset
	}
}
