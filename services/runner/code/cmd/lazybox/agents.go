package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"strings"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox agents …` is how an agent the person made an Orchestrator runs a
// mission's agents (audits/2026-11-16/sota-agents.md §6.12), and how those
// agents talk back. The credential is the agent's own: it starts agents only
// under the grant the person gave, and reaches only the agents it started
// (and, from a child, its parent). Nothing here approves, answers for the
// person, passes a gate or accepts a task.

const agentsUsage = `To start another conversation (the person asked you to), use
  lazybox conversation new --title "…" --brief "…"     (the person approves it in one click)

With an Orchestrator grant (the person lets this conversation run a mission's agents):
Usage:
  lazybox agents spawn --title "task" [--brief "objective, done-when, boundaries, output" | --brief -]
                       [--task ID] [--workspace same|new] [--permission read-only|ask|yolo]
                       [--profile ID] [--model M] [--role worker]
  lazybox agents ls
  lazybox agents send <agent|parent> "message" (or - for stdin)
  lazybox agents wait [agent…|parent] [--until settled|idle|needs-input|reported|closed] [--any] [--timeout 10m]
  lazybox agents watch [--timeout 30m]
  lazybox agents stop <agent> [--end] [--reason WHY]
  lazybox agents report "what you did and how you checked it"   (as a child)

Without a grant, spawn asks the person to start the conversation, as conversation new does.`

func (c *cli) agents(args []string) error {
	if c.subcommandHelp(args, agentsUsage) {
		return nil
	}
	switch args[0] {
	case "spawn", "start":
		return c.agentsSpawn(args[1:])
	case "ls", "list":
		return c.agentsList(args[1:])
	case "send":
		return c.agentsSend(args[1:])
	case "wait":
		return c.agentsWait(args[1:])
	case "watch":
		return c.agentsWatch(args[1:])
	case "stop":
		return c.agentsStop(args[1:])
	case "report":
		return c.agentsReport(args[1:])
	}
	fmt.Fprintln(c.stderr, agentsUsage)
	return fail(exitUsage, "unknown agents command %q", args[0])
}

// agentsCaller is the execution the verb runs as: the agent's own, or --session.
func (c *cli) agentsCaller(flagged string) (string, error) {
	if flagged != "" {
		return flagged, nil
	}
	if session := c.getenv("LAZYBOX_V2_SESSION_ID"); session != "" {
		return session, nil
	}
	return "", fail(exitUsage, "this runs inside an agent's conversation (or pass --session SESSION from your own terminal); the Missions panel shows every agent tree")
}

// text reads a message argument, or stdin for "-", bounded like the Runner.
func (c *cli) text(args []string) (string, error) {
	text := strings.Join(args, " ")
	if text == "-" || (text == "" && !c.stdinTTY) {
		data, err := io.ReadAll(io.LimitReader(c.stdin, 4<<10+1))
		if err != nil {
			return "", err
		}
		text = string(data)
	}
	return strings.TrimSpace(text), nil
}

func (c *cli) agentsSpawn(args []string) error {
	f := c.flags("agents")
	session := f.String("session", "", "the orchestrator's session (default: this agent's)")
	title := f.String("title", "", "the task, in a line: it names the agent's conversation and its card on the mission board")
	brief := f.String("brief", "", "the task's brief, sent as its first message (- reads stdin): objective, done when, boundaries, output")
	task := f.String("task", "", "work on an existing mission task instead of creating one")
	workspace := f.String("workspace", "same", "same: share this workspace's files; new: its own workspace, on new branches from this one's")
	permission := f.String("permission", "", "read-only, ask or yolo; at most your grant's ceiling and your own (default: the narrower of the two)")
	profile := f.String("profile", "", "subscription (default: yours); must be one your grant names")
	model := f.String("model", "", "model (default: yours)")
	role := f.String("role", "worker", "a label shown on the board: worker, reviewer, researcher…")
	key := f.String("key", "", "idempotency key: a retried spawn with the same key starts no second agent")
	wait := f.Duration("timeout", defaultRequestWait, "without a grant: how long to wait for the person to start it")
	noWait := f.Bool("no-wait", false, "without a grant: ask and return at once; the answer comes later as a note")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	if *title == "" && *task == "" {
		return fail(exitUsage, "give the agent's task a --title (and a --brief), or name an existing --task")
	}
	if *brief == "-" {
		if *brief, err = c.text([]string{"-"}); err != nil {
			return err
		}
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	// Without a grant in force, starting an agent is the person's to allow:
	// ask them, as `lazybox conversation new` does, instead of failing.
	if *task == "" {
		if lineage, err := got(cl.ListAgentLineage(c.ctx, connect.NewRequest(&pb.AgentLineageRequest{SessionId: me}))); err == nil && (lineage.Grant == nil || !lineage.Grant.Live) {
			if !c.json {
				c.note("No Orchestrator grant here: asking the person to start it as a conversation (lazybox conversation new).\n")
			}
			return c.askToStart(cl, &pb.RequestConversationStartRequest{SessionId: me, Title: *title, Brief: *brief, Workspace: *workspace, ProfileId: *profile, Model: *model}, *wait, *noWait)
		}
	}
	operation := *key
	if operation == "" {
		operation = fmt.Sprintf("spawn-%d", c.now().UnixNano())
	}
	out, err := got(cl.SpawnAgent(c.ctx, connect.NewRequest(&pb.SpawnAgentRequest{SessionId: me, OperationId: operation, Title: *title, Brief: *brief, TaskId: *task, Workspace: *workspace, Permission: *permission, ProfileId: *profile, Model: *model, Role: *role})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	s := out.Spawned
	if s == nil {
		return errors.New("the Runner started no agent")
	}
	where := "your workspace"
	if s.WorkspaceName != "" {
		where = "workspace " + s.WorkspaceName
	}
	c.out("Started %s (%s · %s · %s · %s).\nIt is task %s on “%s”. Follow it with `lazybox agents wait %q` or `lazybox agents watch`.\n", s.Title, s.Role, s.ProfileId, s.Permission, where, s.TaskId, out.MissionTitle, s.Title)
	if out.Grant != nil && out.Grant.Ceiling != nil {
		c.out("%d of %d agents running.\n", out.Grant.LiveChildren, out.Grant.Ceiling.MaxChildren)
	}
	return nil
}

func (c *cli) agentsList(args []string) error {
	f := c.flags("agents")
	session := f.String("session", "", "whose tree (default: this agent's)")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	out, err := got(cl.ListAgentLineage(c.ctx, connect.NewRequest(&pb.AgentLineageRequest{SessionId: me})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	c.printLineage(out)
	return nil
}

func (c *cli) printLineage(out *pb.AgentLineage) {
	if out.MissionId == "" {
		c.out("You run no agents and no agent started you. The person can let this conversation run agents for a mission.\n")
		return
	}
	if g := out.Grant; g != nil {
		state := "in force"
		if !g.Live {
			state = "revoked"
		}
		ceiling := g.GetCeiling()
		c.out("Orchestrating “%s” (grant %s, %s): %d of %d agents running, %s or narrower, on %s.\n", out.MissionTitle, g.Id, state, g.LiveChildren, ceiling.GetMaxChildren(), ceiling.GetPermission(), strings.Join(ceiling.GetProfileIds(), ", "))
	}
	if p := out.Parent; p != nil {
		c.out("Started by “%s” (your orchestrator) for “%s” · it is %s.\n", p.Title, out.MissionTitle, liveWord(p))
	}
	if g := out.Grant; g != nil && g.Ceiling != nil && g.Ceiling.BudgetUsd > 0 {
		state := ""
		if g.BudgetState == "exhausted" {
			state = " · new agents paused until the person raises it"
		}
		c.out("Budget: $%.2f spent, $%.2f reserved of $%.2f%s.\n", out.SpentUsd, out.ReservedUsd, g.Ceiling.BudgetUsd, state)
	}
	if p := out.Plan; p != nil {
		c.out("Plan %s (%s, est. $%.2f):\n", p.Id, p.State, p.TotalUsd)
		started := map[string]string{}
		for _, ch := range out.Children {
			started[ch.TaskId] = liveWord(ch)
		}
		for _, t := range p.Tasks {
			id, state := t.TaskId, "waiting for the person"
			if id == "" {
				id = "-"
			} else if s, ok := started[t.TaskId]; ok {
				state = s
			} else {
				state = "ready to start: lazybox agents spawn --task " + t.TaskId
			}
			if p.State != "approved" {
				state = p.State
			}
			c.out("  %-12s %-14s %s · %s\n", t.Key, id, truncate(t.Title, 32), state)
		}
	}
	if len(out.Children) == 0 && out.Grant != nil && out.Plan == nil {
		c.out("No agents yet: lazybox agents spawn --title … --brief …, or propose a plan: lazybox mission plan -\n")
	}
	for _, ch := range out.Children {
		line := fmt.Sprintf("  %-28s %-14s %-9s %-10s %s", truncate(ch.Title, 28), liveWord(ch), ch.Role, ch.Permission, ch.TaskStatus)
		c.out("%s\n", strings.TrimRight(line, " "))
		if ch.Reported {
			c.out("    reported: %s\n", oneLine(ch.Report, 160))
		} else if ch.LastMessage != "" {
			c.out("    last: %s\n", oneLine(ch.LastMessage, 160))
		}
	}
}

func liveWord(ch *pb.AgentChild) string {
	switch {
	case ch.Ended:
		return "ended"
	case ch.Live == "waiting":
		return "needs the person"
	case ch.Reported && ch.Live != "running":
		return "reported"
	case ch.Live == "":
		return "unknown"
	}
	return ch.Live
}

func truncate(s string, n int) string {
	r := []rune(s)
	if len(r) > n {
		return string(r[:n-1]) + "…"
	}
	return s
}

func (c *cli) agentsSend(args []string) error {
	f := c.flags("agents")
	session := f.String("session", "", "who sends (default: this agent)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	if len(positional) < 1 {
		return fail(exitUsage, "usage: lazybox agents send <agent|parent> \"message\"")
	}
	text, err := c.text(positional[1:])
	if err != nil {
		return err
	}
	if text == "" {
		return fail(exitUsage, "say what to send (or - to read it from stdin)")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	out, err := got(cl.SendAgentMessage(c.ctx, connect.NewRequest(&pb.SendAgentMessageRequest{SessionId: me, To: positional[0], Text: text, OperationId: fmt.Sprintf("send-%d", c.now().UnixNano())})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	if out.Delivery == "queued" {
		c.out("Sent to “%s”; it reads it when its current turn ends.\n", out.ToLabel)
	} else {
		c.out("Sent to “%s”.\n", out.ToLabel)
	}
	return nil
}

func (c *cli) agentsWait(args []string) error {
	f := c.flags("agents")
	session := f.String("session", "", "who waits (default: this agent)")
	until := f.String("until", "settled", "settled (not running), idle, needs-input, reported or closed")
	any := f.Bool("any", false, "return when any one target gets there (default: all of them)")
	timeout := f.Duration("timeout", 10*time.Minute, "give up after this long (exit 5)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	deadline := c.now().Add(*timeout)
	for {
		left := deadline.Sub(c.now())
		if left <= 0 {
			break
		}
		out, err := got(cl.WaitAgents(c.ctx, connect.NewRequest(&pb.WaitAgentsRequest{SessionId: me, Targets: positional, Until: *until, Any: *any, TimeoutSeconds: uint32(max(min(left, 55*time.Second)/time.Second, 1))})))
		if err != nil {
			var ce *connect.Error
			if errors.As(err, &ce) && ce.Code() == connect.CodeUnavailable && c.ctx.Err() == nil {
				// The Runner is restarting; the agents are not.
				time.Sleep(min(time.Second, max(left, 0)))
				continue
			}
			return err
		}
		if out.Matched {
			if c.json {
				return c.printProtoJSON(out)
			}
			c.out("%s.\n", capitalize(out.Reason))
			c.printLineage(out)
			return nil
		}
		if c.json && c.now().After(deadline) {
			_ = c.printProtoJSON(out)
		}
	}
	return fail(exitTimeout, "still waiting after %s; the agents keep running. Wait again, or end your turn and say what you are waiting for", *timeout)
}

func capitalize(s string) string {
	if s == "" {
		return s
	}
	r := []rune(s)
	return strings.ToUpper(string(r[0])) + string(r[1:])
}

// agentsWatch follows the lineage live: new events, and each agent's state
// as it changes.
func (c *cli) agentsWatch(args []string) error {
	f := c.flags("agents")
	session := f.String("session", "", "whose tree (default: this agent's)")
	timeout := f.Duration("timeout", 30*time.Minute, "stop watching after this long")
	if _, err := c.parse(f, args); err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(c.ctx, *timeout)
	defer cancel()
	first, err := got(cl.ListAgentLineage(ctx, connect.NewRequest(&pb.AgentLineageRequest{SessionId: me})))
	if err != nil {
		return err
	}
	if !c.json {
		c.printLineage(first)
	}
	after, states := first.Seq, map[string]string{}
	for _, ch := range first.Children {
		states[ch.ConversationId] = liveWord(ch)
	}
	for ctx.Err() == nil {
		out, err := got(cl.WaitAgents(ctx, connect.NewRequest(&pb.WaitAgentsRequest{SessionId: me, Until: "event", TimeoutSeconds: 5, AfterSeq: after})))
		if err != nil {
			var ce *connect.Error
			if ctx.Err() != nil {
				break
			}
			if errors.As(err, &ce) && ce.Code() == connect.CodeUnavailable {
				time.Sleep(time.Second)
				continue
			}
			return err
		}
		titles := map[string]string{}
		for _, ch := range out.Children {
			titles[ch.ConversationId] = ch.Title
		}
		for _, e := range out.Events {
			if c.json {
				_ = c.printProtoJSON(e)
			} else {
				c.out("%s  %-24s %s\n", time.UnixMilli(e.AtUnixMillis).Format("15:04:05"), truncate(firstOf(titles[e.ConversationId], "—"), 24), e.Summary)
			}
			after = max(after, e.Seq)
		}
		for _, ch := range out.Children {
			if now := liveWord(ch); states[ch.ConversationId] != now {
				states[ch.ConversationId] = now
				if !c.json {
					c.out("%s  %-24s is %s\n", c.now().Format("15:04:05"), truncate(ch.Title, 24), now)
				}
			}
		}
		if len(out.Events) == 0 {
			after = max(after, out.Seq)
		}
	}
	return nil
}

func firstOf(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}

func (c *cli) agentsStop(args []string) error {
	f := c.flags("agents")
	session := f.String("session", "", "who stops it (default: this agent)")
	end := f.Bool("end", false, "end its conversation and its attempt, not only its current turn")
	reason := f.String("reason", "", "why, shown to the person")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox agents stop <agent> [--end]")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	out, err := got(cl.StopChildAgent(c.ctx, connect.NewRequest(&pb.StopChildAgentRequest{SessionId: me, Target: positional[0], End: *end, Reason: *reason})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	if *end {
		c.out("Ended %s.\n", positional[0])
	} else {
		c.out("Interrupted %s's turn; it waits for a message.\n", positional[0])
	}
	return nil
}

func (c *cli) agentsReport(args []string) error {
	f := c.flags("agents")
	session := f.String("session", "", "who reports (default: this agent)")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	me, err := c.agentsCaller(*session)
	if err != nil {
		return err
	}
	summary, err := c.text(positional)
	if err != nil {
		return err
	}
	if summary == "" {
		return fail(exitUsage, "say what you did and how you checked it")
	}
	cl, err := c.dial()
	if err != nil {
		return err
	}
	out, err := got(cl.ReportAgentTask(c.ctx, connect.NewRequest(&pb.ReportAgentTaskRequest{SessionId: me, Summary: summary})))
	if err != nil {
		return err
	}
	if c.json {
		return c.printProtoJSON(out)
	}
	c.out("Reported. The person reviews and accepts the task; your orchestrator was told.\n")
	return nil
}
