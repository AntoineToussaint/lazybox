package main

import (
	"errors"
	"fmt"
	"io"
	"strings"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox conversation new` is how an agent starts another conversation
// when the person asks it to (docs/guides/orchestrator.md, "Ask an agent to
// start another conversation"): it asks, and the person approves it in one
// click in the agent's chat, the Inbox or on their phone. A conversation that
// holds an Orchestrator grant in force starts it at once, under the grant.

const conversationUsage = `Usage:
  lazybox conversation new --title "…" (--brief "…" | --brief - | --brief-artifact ID)
                           [--workspace same|new [--start-from BRANCH]] [--provider claude|codex|…]
                           [--profile ID] [--model M] [--reason "why"] [--timeout 90s] [--no-wait]

Asks the person to start a conversation with this brief; they start it in one
click (Start, Edit… or Deny, in this conversation, the Inbox or on their
phone). It is a conversation of its own, in this workspace (or, with
--workspace new, one with new branches from this one's, or from origin's
BRANCH with --start-from), and its first message is the brief. With an Orchestrator grant in force it starts at once,
as an agent of your mission.

Exit codes: 0 started · 4 the person declined · 5 no answer yet (the request
stays open; the answer comes to this conversation) · 2 usage · 6 no Runner.`

func (c *cli) conversation(args []string) error {
	if c.subcommandHelp(args, conversationUsage) {
		return nil
	}
	switch args[0] {
	case "new", "start":
		return c.conversationNew(args[1:])
	}
	fmt.Fprintln(c.stderr, conversationUsage)
	return fail(exitUsage, "unknown conversation command %q", args[0])
}

func (c *cli) conversationNew(args []string) error {
	f := c.flags("conversation")
	session := f.String("session", "", "the asking conversation's session (default: this agent's)")
	title := f.String("title", "", "the new conversation's title, in a line")
	brief := f.String("brief", "", "its first message: what to do, done when, boundaries (- reads stdin; at most 8 KB)")
	artifact := f.String("brief-artifact", "", "an artifact of this workspace that holds the brief (lazybox artifact put … prints its id)")
	workspace := f.String("workspace", "same", "same: this workspace's files; new: a workspace of its own, on new branches from this one's")
	startFrom := f.String("start-from", "", "with --workspace new: the branch of origin its repositories start from (fetched first), instead of this workspace's branches")
	provider := f.String("provider", "", "the agent: claude, codex or an ACP agent (default: yours)")
	profile := f.String("profile", "", "the subscription it runs on (default: yours, or the provider's first)")
	model := f.String("model", "", "the model (default: yours for the same agent, else its default)")
	reason := f.String("reason", "", "why, in a line the person decides from")
	wait := f.Duration("timeout", defaultRequestWait, "how long to wait for the person's answer")
	noWait := f.Bool("no-wait", false, "ask and return at once; the answer comes later as a note in this conversation")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) > 0 {
		return fail(exitUsage, "unexpected %q: quote the brief, --brief \"…\", or pass it on stdin with --brief -", positional[0])
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	if strings.TrimSpace(*title) == "" {
		return fail(exitUsage, "give the conversation a --title (and a --brief, or a --brief-artifact)")
	}
	if *brief == "-" {
		data, err := io.ReadAll(io.LimitReader(c.stdin, 8<<10+1))
		if err != nil {
			return err
		}
		*brief = strings.TrimSpace(string(data))
	}
	if strings.TrimSpace(*brief) == "" && *artifact == "" {
		return fail(exitUsage, "give a --brief (what to do, done when, boundaries), --brief - to read it from stdin, or a --brief-artifact")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	return c.askToStart(cl, &pb.RequestConversationStartRequest{SessionId: me, Title: *title, Brief: *brief, BriefArtifactId: *artifact, Workspace: *workspace, StartFrom: *startFrom, Provider: *provider, ProfileId: *profile, Model: *model, Reason: *reason}, *wait, *noWait)
}

// askToStart asks the person to start a conversation and waits, at most
// wait, for their answer.
func (c *cli) askToStart(cl *client, ask *pb.RequestConversationStartRequest, wait time.Duration, noWait bool) error {
	r, err := got(cl.RequestConversationStart(c.ctx, connect.NewRequest(ask)))
	if err != nil {
		return err
	}
	if r.State != "pending" {
		return c.startOutcome(r)
	}
	if !c.json {
		c.out("Asked the person to start “%s” (request %s): they see it in this conversation, with Start, Edit… and Deny.\n", r.Title, r.Id)
		if r.Workspace == "same" && r.SharedWith > 1 {
			c.out("It would share this workspace's files with %d running conversations; ask with --workspace new for branches of its own.\n", r.SharedWith)
		}
	}
	if noWait {
		if c.json {
			return c.printProtoJSON(r)
		}
		c.out("Not waiting. You will be told the answer in this conversation.\n")
		return nil
	}
	if !c.json {
		c.out("Waiting for their answer (up to %s)…\n", wait)
	}
	deadline := c.now().Add(wait)
	for r.State == "pending" {
		left := deadline.Sub(c.now())
		if left <= 0 {
			break
		}
		seconds := uint32(min(left, 55*time.Second) / time.Second)
		next, err := got(cl.WaitConversationStart(c.ctx, connect.NewRequest(&pb.WaitConversationStartRequest{RequestId: r.Id, TimeoutSeconds: max(seconds, 1)})))
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
	return c.startOutcome(r)
}

// startOutcome prints the answer, with the exit status a script can test:
// 0 started, 4 declined, 5 no answer yet (or expired).
func (c *cli) startOutcome(r *pb.ConversationStartRequest) error {
	if c.json {
		if err := c.printProtoJSON(r); err != nil {
			return err
		}
	}
	switch r.State {
	case "started":
		if c.json {
			return nil
		}
		if r.Child {
			c.out("Started %s (conversation %s) as an agent of your mission “%s”. Follow it with `lazybox agents wait %q` or `lazybox agents watch`.\n", r.StartedTitle, r.StartedConversationId, r.MissionTitle, r.StartedTitle)
			return nil
		}
		where := "in this workspace"
		if r.Workspace == "new" {
			where = "in a new workspace"
		}
		c.out("Started %s (conversation %s), %s. The person approved it; it works on its own and answers to them, so do not do its work here.\n", r.StartedTitle, r.StartedConversationId, where)
		return nil
	case "declined":
		why := ""
		if r.DeclineReason != "" {
			why = " Their reason: " + r.DeclineReason
		}
		return fail(exitDenied, "the person declined starting “%s”.%s\nDo not start it another way; continue here, or ask them how to proceed.", r.Title, why)
	case "expired":
		return fail(exitTimeout, "the request to start “%s” expired without an answer; ask again if it is still needed", r.Title)
	}
	return fail(exitTimeout, "no answer yet. The request stays open in Lazybox; when the person answers you will be told in this conversation. Continue with what you can do without it, or end your turn and say what you are waiting for")
}
