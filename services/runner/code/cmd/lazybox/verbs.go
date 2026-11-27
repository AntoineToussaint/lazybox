package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"sort"
	"strings"
	"time"

	"connectrpc.com/connect"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// Stable JSON shapes of the read verbs.

type jsonAgent struct {
	ID       string `json:"id"`
	ParentID string `json:"parentId,omitempty"`
	Name     string `json:"name,omitempty"`
	Status   string `json:"status"`
	Phase    string `json:"phase,omitempty"`
	Tool     string `json:"tool,omitempty"`
}

type jsonRequest struct {
	ID   string `json:"id"`
	Kind string `json:"kind"`
	Text string `json:"text,omitempty"`
}

type jsonConversation struct {
	ID            string        `json:"id"`
	Session       string        `json:"session"`
	Title         string        `json:"title"`
	WorkspaceID   string        `json:"workspaceId,omitempty"`
	WorkspaceName string        `json:"workspaceName,omitempty"`
	State         string        `json:"state"`
	Reason        string        `json:"reason,omitempty"`
	Provider      string        `json:"provider,omitempty"`
	Model         string        `json:"model,omitempty"`
	Permission    string        `json:"permission,omitempty"`
	Busy          bool          `json:"busy"`
	Queued        uint32        `json:"queued,omitempty"`
	Fault         string        `json:"fault,omitempty"`
	Pending       []jsonRequest `json:"pending,omitempty"`
	Agents        []jsonAgent   `json:"agents,omitempty"`
	LastMessage   string        `json:"lastMessage,omitempty"`
	UpdatedAt     string        `json:"updatedAt,omitempty"`
	Runner        string        `json:"runner,omitempty"`
}

type jsonWorkspace struct {
	ID            string             `json:"id"`
	Name          string             `json:"name"`
	State         string             `json:"state"`
	Root          string             `json:"root,omitempty"`
	Repositories  []string           `json:"repositories"`
	Conversations []jsonConversation `json:"conversations"`
}

type jsonListing struct {
	Runner        string             `json:"runner,omitempty"`
	Workspaces    []jsonWorkspace    `json:"workspaces"`
	Conversations []jsonConversation `json:"unassigned"`
	Error         string             `json:"error,omitempty"`
}

func (c *cli) describe(v conversation, runner string) jsonConversation {
	s := v.Head
	st, reason := state(s, c.now())
	out := jsonConversation{ID: v.Key(), Session: s.Id, Title: v.Title(), WorkspaceID: s.CollectionId, WorkspaceName: v.WorkspaceName(), State: st, Reason: reason,
		Provider: s.Provider, Model: s.Model, Permission: s.Permission, Busy: s.Busy, Queued: s.Queued, Fault: s.Fault, LastMessage: oneLine(s.LastMessage, 240), Runner: runner}
	if at := activity(s); !at.IsZero() {
		out.UpdatedAt = at.UTC().Format(time.RFC3339)
	}
	for _, p := range s.Pending {
		out.Pending = append(out.Pending, jsonRequest{ID: p.RequestId, Kind: p.Type, Text: p.Text})
	}
	for _, a := range s.Agents {
		out.Agents = append(out.Agents, jsonAgent{ID: a.Id, ParentID: a.ParentId, Name: a.Name, Status: a.Status, Phase: a.Phase, Tool: a.Tool})
	}
	return out
}

func repositories(w *pb.Collection) []string {
	names := []string{}
	for _, m := range w.Members {
		if m.Spec != nil {
			names = append(names, m.Spec.Name)
		}
	}
	return names
}

func (c *cli) listing(k catalog, runner string) jsonListing {
	out := jsonListing{Runner: runner, Workspaces: []jsonWorkspace{}, Conversations: []jsonConversation{}}
	index := map[string]int{}
	for _, w := range k.Workspaces {
		index[w.Id] = len(out.Workspaces)
		out.Workspaces = append(out.Workspaces, jsonWorkspace{ID: w.Id, Name: w.Name, State: w.State, Root: w.Root, Repositories: repositories(w), Conversations: []jsonConversation{}})
	}
	for _, v := range k.Conversations {
		d := c.describe(v, "")
		if i, ok := index[v.Head.CollectionId]; ok {
			out.Workspaces[i].Conversations = append(out.Workspaces[i].Conversations, d)
		} else {
			out.Conversations = append(out.Conversations, d)
		}
	}
	return out
}

// each runs fn against the selected Runner, or every registered one with --all.
func (c *cli) each(all bool, fn func(*client) error) error {
	if !all {
		cl, err := c.dial()
		if err != nil {
			return err
		}
		return fn(cl)
	}
	locators, err := readLocators(c.getenv)
	if err != nil {
		return fail(exitUnavailable, "cannot read the registered Runners: %v", err)
	}
	if len(locators) == 0 {
		return fail(exitUnavailable, "no registered Runners (they register when started with make dev or working)")
	}
	var failures int
	for _, l := range locators {
		t, err := loadTarget(l.Name, l.Endpoint, false)
		if err == nil {
			err = fn(dial(t))
		}
		if err != nil {
			failures++
			c.note("%s: %v\n", l.Name, err)
		}
	}
	if failures == len(locators) {
		return fail(exitUnavailable, "no registered Runner answered")
	}
	return nil
}

func (c *cli) ls(args []string) error {
	f := c.flags("ls")
	all := f.Bool("all", false, "every registered Runner, grouped by Runner")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	if !*all {
		// An agent sees its own commands, not the person's conversations.
		if cl, err := c.dial(); err == nil {
			if session := c.agentSession(cl); session != "" {
				return c.agentLs(cl, session)
			}
		}
	}
	var listings []jsonListing
	err := c.each(*all, func(cl *client) error {
		k, err := loadCatalog(c.ctx, cl)
		if err != nil {
			return err
		}
		listings = append(listings, c.listing(k, cl.target.Name))
		return nil
	})
	if err != nil && len(listings) == 0 {
		return err
	}
	if c.json {
		if *all {
			return c.printJSON(map[string]any{"runners": listings})
		}
		return c.printJSON(listings[0])
	}
	for i, l := range listings {
		if *all {
			if i > 0 {
				c.out("\n")
			}
			c.out("%s\n", c.paint(bold, "▌ "+l.Runner))
		}
		c.printListing(l)
	}
	return nil
}

func (c *cli) printListing(l jsonListing) {
	if len(l.Workspaces) == 0 && len(l.Conversations) == 0 {
		c.out("No workspaces or conversations yet. Create one: lazybox new NAME --repo REPO\n")
		return
	}
	for _, w := range l.Workspaces {
		repos := strings.Join(w.Repositories, ", ")
		if repos == "" {
			repos = "no repositories"
		}
		state := ""
		if w.State != "" && w.State != "ready" {
			state = " · " + w.State
		}
		c.out("%s %s %s\n", c.paint(bold, w.Name), c.paint(dim, w.ID), c.paint(dim, "· "+repos+state))
		if len(w.Conversations) == 0 {
			c.out("  %s\n", c.paint(dim, "no conversations"))
		}
		for _, v := range w.Conversations {
			c.printConversationRow(v, "  ")
		}
	}
	if len(l.Conversations) > 0 {
		c.out("%s\n", c.paint(bold, "Outside a workspace"))
		for _, v := range l.Conversations {
			c.printConversationRow(v, "  ")
		}
	}
}

func (c *cli) printConversationRow(v jsonConversation, indent string) {
	who := v.Provider
	if v.Model != "" {
		who = strings.TrimPrefix(who+" · "+v.Model, " · ")
	}
	line := fmt.Sprintf("%s%s %-11s %-36s %s", indent, c.paint(stateColour(v.State), stateIcon(v.State)), c.paint(stateColour(v.State), stateLabel(v.State)), oneLine(v.Title, 36), c.paint(dim, v.Session))
	if who != "" {
		line += "  " + c.paint(dim, who)
	}
	if v.Reason != "" {
		line += "  " + v.Reason
	}
	c.out("%s\n", line)
	for _, a := range v.Agents {
		if a.ParentID == "" || a.Status == "done" || a.Status == "stopped" || a.Status == "interrupted" {
			continue
		}
		name := a.Name
		if name == "" {
			name = a.ID
		}
		detail := a.Status
		if a.Phase != "" {
			detail += " · " + a.Phase
		}
		c.out("%s    ↳ %s %s\n", indent, oneLine(name, 40), c.paint(dim, detail))
	}
}

func (c *cli) inbox(args []string) error {
	f := c.flags("inbox")
	all := f.Bool("all", false, "every registered Runner")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	items := []jsonConversation{}
	err := c.each(*all, func(cl *client) error {
		k, err := loadCatalog(c.ctx, cl)
		if err != nil {
			return err
		}
		for _, v := range k.Conversations {
			d := c.describe(v, cl.target.Name)
			if d.State == stateIdle || d.State == stateEnded {
				continue
			}
			items = append(items, d)
		}
		return nil
	})
	if err != nil && len(items) == 0 {
		return err
	}
	sort.SliceStable(items, func(i, j int) bool { return stateOrder[items[i].State] < stateOrder[items[j].State] })
	if c.json {
		return c.printJSON(map[string]any{"items": items})
	}
	if len(items) == 0 {
		c.out("Nothing needs you. %s\n", c.paint(dim, "(lazybox ls shows everything)"))
		return nil
	}
	current := ""
	for _, v := range items {
		if v.State != current {
			current = v.State
			c.out("%s\n", c.paint(stateColour(current)+";"+bold, strings.ToUpper(stateLabel(current))))
		}
		where := v.WorkspaceName
		if where != "" {
			where += " / "
		}
		c.out("  %s%s  %s\n", where, oneLine(v.Title, 48), c.paint(dim, v.Session))
		if v.Reason != "" {
			c.out("    %s\n", v.Reason)
		}
		for _, p := range v.Pending {
			c.out("    %s\n", c.paint(dim, fmt.Sprintf("lazybox approve %s  ·  lazybox deny %s", p.ID, p.ID)))
		}
	}
	return nil
}

type jsonRunner struct {
	Name     string `json:"name"`
	Product  string `json:"product,omitempty"`
	Endpoint string `json:"endpoint"`
	URL      string `json:"url,omitempty"`
	Live     bool   `json:"live"`
	Default  bool   `json:"default"`
	Problem  string `json:"problem,omitempty"`
}

func (c *cli) runners(args []string) error {
	f := c.flags("runners")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	locators, err := readLocators(c.getenv)
	if err != nil {
		return fail(exitUnavailable, "cannot read the registered Runners: %v", err)
	}
	chosen := ""
	if t, err := resolveTarget(c.getenv, "", c.runner); err == nil {
		chosen = t.Name
	}
	out := []jsonRunner{}
	for _, l := range locators {
		r := jsonRunner{Name: l.Name, Product: l.Product, Endpoint: l.Endpoint, Default: l.Name == chosen}
		t, err := loadTarget(l.Name, l.Endpoint, false)
		if err == nil {
			r.URL = t.URL
			ctx, cancel := context.WithTimeout(c.ctx, probeTimeout)
			_, err = dial(t).Version(ctx, connect.NewRequest(&pb.VersionRequest{}))
			cancel()
			err = dial(t).explain(err)
		}
		r.Live = err == nil
		if err != nil {
			r.Problem = err.Error()
		}
		out = append(out, r)
	}
	if c.json {
		return c.printJSON(map[string]any{"runners": out})
	}
	if len(out) == 0 {
		c.out("No registered Runners. `make dev` or `make working` registers one in %s/runners.\n", configDir(c.getenv))
		return nil
	}
	for _, r := range out {
		mark := "  "
		if r.Default {
			mark = c.paint(bold, "→ ")
		}
		status := c.paint(green, "running ") + c.paint(dim, r.URL)
		if !r.Live {
			status = c.paint(dim, "not running · "+r.Problem)
		}
		c.out("%s%-8s %s\n", mark, r.Name, status)
	}
	return nil
}

// readMessage is the text to send: the argument, or stdin for "-" or a pipe.
func (c *cli) readMessage(args []string) (string, error) {
	text := strings.Join(args, " ")
	if text == "-" || (text == "" && !c.stdinTTY) {
		data, err := io.ReadAll(io.LimitReader(c.stdin, 1<<20+1))
		if err != nil {
			return "", err
		}
		if len(data) > 1<<20 {
			return "", fail(exitUsage, "the message is larger than 1 MiB")
		}
		text = string(data)
	}
	if strings.TrimSpace(text) == "" {
		return "", fail(exitUsage, "nothing to send: give the text, or pipe it in (lazybox send CONVERSATION -)")
	}
	return text, nil
}

// live returns the conversation's execution that can take a message,
// resuming an ended one.
func (c *cli) live(cl *client, v conversation) (*pb.Session, error) {
	if !v.Head.Closed && !v.Head.Historical {
		return v.Head, nil
	}
	r, err := got(cl.ResumeSession(c.ctx, connect.NewRequest(&pb.ResumeSessionRequest{SessionId: v.Head.Id})))
	if err != nil {
		return nil, err
	}
	c.note("Resumed “%s”.\n", v.Title())
	return r.Session, nil
}

func (c *cli) send(args []string) error {
	f := c.flags("send")
	steer := f.Bool("steer", false, "steer the running turn instead of queueing after it")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) == 0 {
		return fail(exitUsage, "usage: lazybox send <conversation> [text|-]")
	}
	text, err := c.readMessage(positional[1:])
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	if session := c.getenv("LAZYBOX_V2_SESSION_ID"); cl.target.Scoped && session != "" {
		if *steer {
			return fail(exitUsage, "agent messages queue behind a running turn; send without --steer")
		}
		return c.sendLineageMessage(cl, session, positional[0], text)
	}
	k, err := loadCatalog(c.ctx, cl)
	if err != nil {
		return err
	}
	v, err := k.findConversation(positional[0])
	if err != nil {
		return err
	}
	s, err := c.live(cl, v)
	if err != nil {
		return err
	}
	apply := &pb.ApplyRequest{SessionId: s.Id, OperationId: commands.ID(), Operation: &pb.ApplyRequest_Message{Message: &pb.Message{Text: text}}}
	if *steer {
		apply.Operation = &pb.ApplyRequest_Steer{Steer: &pb.Message{Text: text}}
	}
	receipt, err := got(cl.Apply(c.ctx, connect.NewRequest(apply)))
	if err != nil {
		return err
	}
	if c.json {
		return c.printJSON(map[string]any{"conversation": v.Key(), "session": s.Id, "operationId": receipt.OperationId, "delivery": receipt.Delivery})
	}
	queued := ""
	if s.Busy {
		queued = " It runs after the current turn."
	}
	c.out("Sent to “%s” (%s).%s\n", v.Title(), s.Id, queued)
	return nil
}

func (c *cli) wait(args []string) error {
	f := c.flags("wait")
	until := f.String("until", "idle", "idle or needs-input")
	timeout := f.Duration("timeout", 30*time.Minute, "give up after this long (exit 5)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox wait <conversation> --until idle|needs-input [--timeout 30m]")
	}
	if *until != "idle" && *until != "needs-input" {
		return fail(exitUsage, "--until is idle or needs-input")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	k, err := loadCatalog(c.ctx, cl)
	if err != nil {
		return err
	}
	v, err := k.findConversation(positional[0])
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(c.ctx, *timeout)
	defer cancel()
	id, wasBusy := v.Head.Id, false
	for {
		r, err := cl.GetSession(ctx, connect.NewRequest(&pb.SessionRequest{SessionId: id}))
		if err != nil {
			if ctx.Err() != nil {
				return fail(exitTimeout, "“%s” is still not %s after %s", v.Title(), *until, *timeout)
			}
			return cl.explain(err)
		}
		s := r.Msg.Session
		if s.SuccessorExecutionId != "" {
			id = s.SuccessorExecutionId
			continue
		}
		st, reason := state(s, c.now())
		busy := s.Busy || s.Queued > 0
		wasBusy = wasBusy || busy
		result := func(code int, message string) error {
			if c.json {
				if err := c.printJSON(map[string]any{"conversation": v.Key(), "session": s.Id, "state": st, "reason": reason, "reached": code == exitOK}); err != nil {
					return err
				}
				if code == exitOK {
					return nil
				}
				return exitError{code, errors.New(message)}
			}
			if code == exitOK {
				c.out("%s\n", message)
				return nil
			}
			return exitError{code, errors.New(message)}
		}
		switch {
		case *until == "needs-input" && st == stateNeedsInput:
			return result(exitOK, fmt.Sprintf("“%s” needs you: %s", v.Title(), reason))
		case st == stateFailed && !busy:
			return result(exitFailure, fmt.Sprintf("“%s” failed: %s", v.Title(), reason))
		case *until == "idle" && !busy && len(s.Pending) == 0:
			return result(exitOK, fmt.Sprintf("“%s” is idle.", v.Title()))
		case *until == "needs-input" && wasBusy && !busy && len(s.Pending) == 0:
			return result(exitFailure, fmt.Sprintf("“%s” finished its turn without asking for input.", v.Title()))
		}
		select {
		case <-ctx.Done():
			return fail(exitTimeout, "“%s” is still not %s after %s", v.Title(), *until, *timeout)
		case <-time.After(c.poll):
		}
	}
}

// pendingRequest is one approval or question waiting somewhere.
type pendingRequest struct {
	conversation conversation
	request      *pb.AgentEvent
}

func (c *cli) decide(args []string, accept bool) error {
	name := map[bool]string{true: "approve", false: "deny"}[accept]
	f := c.flags(name)
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox %s <request|conversation>", name)
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
	var found []pendingRequest
	for _, v := range k.Conversations {
		for _, p := range v.Head.Pending {
			if p.RequestId == ref || strings.HasSuffix(p.RequestId, ":"+ref) {
				found = append(found, pendingRequest{v, p})
			}
		}
	}
	if len(found) == 0 {
		v, err := k.findConversation(ref)
		if err != nil {
			var coded exitError
			if errors.As(err, &coded) && coded.code == exitNotFound {
				return fail(exitNotFound, "no pending request or conversation %q (lazybox inbox lists what needs you)", ref)
			}
			return err
		}
		for _, p := range v.Head.Pending {
			if p.Type == "approval" || p.Type == "conversation_request" {
				found = append(found, pendingRequest{v, p})
			}
		}
		switch len(found) {
		case 0:
			if len(v.Head.Pending) > 0 {
				return fail(exitUsage, "“%s” asks a question, not for approval; answer it in the app (lazybox open %s)", v.Title(), v.Head.Id)
			}
			return fail(exitNotFound, "“%s” has no pending approval", v.Title())
		case 1:
		default:
			return fail(exitUsage, "“%s” has %d pending approvals; name one by its request id (lazybox inbox)", v.Title(), len(found))
		}
	}
	p := found[0]
	if p.request.Type == "question" {
		return fail(exitUsage, "request %s is a question; answer it in the app (lazybox open %s)", ref, p.conversation.Head.Id)
	}
	if p.request.Type == "repository_request" {
		// Adding prepares worktrees with progress: the app's card does it.
		if accept {
			return fail(exitUsage, "request %s asks to add a repository; add it from its card in the app, which shows the progress (lazybox open %s)", ref, p.conversation.Head.Id)
		}
		if _, err = got(cl.DeclineRepositoryRequest(c.ctx, connect.NewRequest(&pb.DeclineRepositoryRequestRequest{RequestId: p.request.RequestId}))); err != nil {
			return err
		}
		if c.json {
			return c.printJSON(map[string]any{"conversation": p.conversation.Key(), "session": p.conversation.Head.Id, "request": p.request.RequestId, "decision": "declined"})
		}
		c.out("Declined in “%s”: %s\n", p.conversation.Title(), requestLine(p.request))
		return nil
	}
	if p.request.Type == "conversation_request" {
		// An agent asks to start a conversation: approve starts it as asked.
		decision := map[bool]string{true: "start", false: "decline"}[accept]
		r, err := got(cl.AnswerConversationStart(c.ctx, connect.NewRequest(&pb.AnswerConversationStartRequest{RequestId: p.request.RequestId, Decision: decision})))
		if err != nil {
			return err
		}
		if c.json {
			return c.printProtoJSON(r)
		}
		if accept {
			c.out("Started “%s” (conversation %s), as “%s” asked.\n", r.StartedTitle, r.StartedConversationId, p.conversation.Title())
		} else {
			c.out("Declined in “%s”: %s\n", p.conversation.Title(), requestLine(p.request))
		}
		return nil
	}
	decision := pb.Decision_DECISION_DECLINE
	if accept {
		decision = pb.Decision_DECISION_ACCEPT
	}
	apply := &pb.ApplyRequest{SessionId: p.conversation.Head.Id, OperationId: commands.ID(), Operation: &pb.ApplyRequest_Approval{Approval: &pb.Approval{RequestId: p.request.RequestId, Decision: decision}}}
	if _, err = got(cl.Apply(c.ctx, connect.NewRequest(apply))); err != nil {
		return err
	}
	verbPast := map[bool]string{true: "Approved", false: "Declined"}[accept]
	if c.json {
		return c.printJSON(map[string]any{"conversation": p.conversation.Key(), "session": p.conversation.Head.Id, "request": p.request.RequestId, "decision": strings.ToLower(verbPast)})
	}
	c.out("%s in “%s”: %s\n", verbPast, p.conversation.Title(), requestLine(p.request))
	return nil
}

func (c *cli) stop(args []string) error {
	f := c.flags("stop")
	agentID := f.String("agent", "", "stop this sub-agent; the main agent's turn continues")
	end := f.Bool("end", false, "end the conversation (it can be resumed)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox stop <conversation|command> [--agent ID] [--end]")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	if session := c.agentSession(cl); session != "" && *agentID == "" && !*end {
		// An agent stops its own commands (`lazybox ls` lists them).
		return c.agentStop(cl, positional[0])
	}
	k, err := loadCatalog(c.ctx, cl)
	if err != nil {
		return err
	}
	v, convErr := k.findConversation(positional[0])
	if convErr != nil {
		var coded exitError
		if !errors.As(convErr, &coded) || coded.code != exitNotFound || *agentID != "" || *end {
			return convErr
		}
		// Not a conversation: a command.
		status, err := cl.CancelCommand(c.ctx, connect.NewRequest(&pb.CommandRequest{CommandId: positional[0]}))
		if err != nil {
			if connect.CodeOf(err) == connect.CodeNotFound {
				return fail(exitNotFound, "no conversation or command %q", positional[0])
			}
			return cl.explain(err)
		}
		if c.json {
			return c.printJSON(map[string]any{"command": status.Msg.Id, "stopped": true})
		}
		c.out("Stopped command %s (%s).\n", status.Msg.Id, status.Msg.Title)
		return nil
	}
	s := v.Head
	switch {
	case *end:
		if _, err = got(cl.CloseSession(c.ctx, connect.NewRequest(&pb.SessionRequest{SessionId: s.Id}))); err != nil {
			return err
		}
		c.report(map[string]any{"conversation": v.Key(), "session": s.Id, "ended": true}, "Ended “%s”. lazybox send resumes it.\n", v.Title())
	case *agentID != "":
		apply := &pb.ApplyRequest{SessionId: s.Id, OperationId: commands.ID(), Operation: &pb.ApplyRequest_StopAgent{StopAgent: &pb.StopAgent{AgentId: *agentID}}}
		if _, err = got(cl.Apply(c.ctx, connect.NewRequest(apply))); err != nil {
			return err
		}
		c.report(map[string]any{"conversation": v.Key(), "session": s.Id, "agent": *agentID, "stopped": true}, "Stopped sub-agent %s in “%s”.\n", *agentID, v.Title())
	default:
		if !s.Busy {
			return fail(exitFailure, "“%s” is not running a turn", v.Title())
		}
		apply := &pb.ApplyRequest{SessionId: s.Id, OperationId: commands.ID(), Operation: &pb.ApplyRequest_Interrupt{Interrupt: &pb.Interrupt{}}}
		if _, err = got(cl.Apply(c.ctx, connect.NewRequest(apply))); err != nil {
			return err
		}
		c.report(map[string]any{"conversation": v.Key(), "session": s.Id, "interrupted": true}, "Interrupted “%s”.\n", v.Title())
	}
	return nil
}

// report prints value as JSON with --json, otherwise the sentence.
func (c *cli) report(value any, format string, args ...any) {
	if c.json {
		_ = c.printJSON(value)
		return
	}
	c.out(format, args...)
}
