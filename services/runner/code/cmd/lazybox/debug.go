package main

import (
	"context"
	"fmt"
	"strings"
	"time"

	"connectrpc.com/connect"
	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox debug` explains a conversation's state from its journal and the
// Runner's view of it: what happened last, what is pending, and in plain
// words what is wrong, if anything.

// lifecycle are the record types a diagnosis shows; deltas, tool chatter and
// activity are left out.
var lifecycle = map[string]bool{
	"session_opened": true, "turn_started": true, "turn_ended": true, "operation_accepted": true, "receipt": true, "failure": true,
	"runner_reattached": true, "agent_status": true, "approval": true, "question": true, "request_resolved": true,
	"session_closed": true, "profile_changed": true, "profile_switching": true, "process_exit": true, "continuation_requested": true,
	"code_reverted": true, "unsupported_request": true, "unreadable_event": true, "model_changed": true, "permission_changed": true, "compacted": true,
}

type diagnosis struct {
	Conversation string        `json:"conversation"`
	Session      string        `json:"session"`
	Title        string        `json:"title"`
	Workspace    string        `json:"workspace,omitempty"`
	State        string        `json:"state"`
	Busy         bool          `json:"busy"`
	Closed       bool          `json:"closed"`
	Fault        string        `json:"fault,omitempty"`
	Queued       uint32        `json:"queued"`
	Status       string        `json:"status,omitempty"`
	Permission   string        `json:"permission,omitempty"`
	Pending      []jsonRequest `json:"pending"`
	Agents       []jsonAgent   `json:"agents"`
	Events       []record      `json:"events"`
	Findings     []string      `json:"findings"`
	// Journal: how many records were read, and the last cursor.
	Records uint64 `json:"records"`
	Cursor  uint64 `json:"cursor"`
}

func (c *cli) debug(args []string) error {
	f := c.flags("debug")
	n := f.Int("events", 25, "how many lifecycle events to show")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox debug <conversation> [--events N] [--json]")
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
	got, err := got(cl.GetSession(c.ctx, connect.NewRequest(&pb.SessionRequest{SessionId: v.Head.Id})))
	if err != nil {
		return err
	}
	s := got.Session
	records, err := c.journal(cl, v, s.Id)
	if err != nil {
		return err
	}
	d := diagnose(v, s, records, c.now())
	if *n >= 0 && len(d.Events) > *n {
		d.Events = d.Events[len(d.Events)-*n:]
	}
	if c.json {
		return c.printJSON(d)
	}
	c.printDiagnosis(d)
	return nil
}

// journal reads the conversation's records, across its earlier executions,
// until it has caught up with what existed when it asked.
func (c *cli) journal(cl *client, v conversation, session string) ([]record, error) {
	ctx, cancel := context.WithCancel(c.ctx)
	defer cancel()
	stream, err := cl.WatchEvents(ctx, connect.NewRequest(&pb.WatchEventsRequest{SessionId: session, IncludePrevious: true}))
	if err != nil {
		return nil, cl.explain(err)
	}
	defer stream.Close()
	// A journal with nothing new for a moment is complete: the stream would
	// otherwise wait for live records.
	idle := time.AfterFunc(1500*time.Millisecond, cancel)
	defer idle.Stop()
	var out []record
	for stream.Receive() {
		idle.Reset(1500 * time.Millisecond)
		j := stream.Msg()
		out = append(out, toRecord(j, v, session))
		if j.Retained != 0 && j.Cursor >= j.Retained {
			break
		}
	}
	if err := stream.Err(); err != nil && ctx.Err() == nil {
		return nil, cl.explain(err)
	}
	return out, nil
}

func diagnose(v conversation, s *pb.Session, records []record, now time.Time) diagnosis {
	st, _ := state(s, now)
	d := diagnosis{Conversation: v.Key(), Session: s.Id, Title: v.Title(), Workspace: v.WorkspaceName(), State: st, Busy: s.Busy, Closed: s.Closed || s.Historical,
		Fault: s.Fault, Queued: s.Queued, Status: s.Status, Permission: s.Permission, Pending: []jsonRequest{}, Agents: []jsonAgent{}, Events: []record{}, Findings: []string{}, Records: uint64(len(records))}
	for _, p := range s.Pending {
		d.Pending = append(d.Pending, jsonRequest{ID: p.RequestId, Kind: p.Type, Text: p.Text})
	}
	for _, a := range s.Agents {
		d.Agents = append(d.Agents, jsonAgent{ID: a.Id, ParentID: a.ParentId, Name: a.Name, Status: a.Status, Phase: a.Phase, Tool: a.Tool})
	}
	if len(records) > 0 {
		d.Cursor = records[len(records)-1].Cursor
	}
	for _, r := range records {
		if lifecycle[r.Type] {
			d.Events = append(d.Events, r)
		}
	}
	clock := func(r record) string { return r.time().Local().Format("15:04") }
	find := func(text string, args ...any) { d.Findings = append(d.Findings, fmt.Sprintf(text, args...)) }

	// The last turn, and what ended it.
	var lastStart, lastEnd, lastControl, lastFailure, reattached *record
	var acceptedMessages []record
	for i := range records {
		r := &records[i]
		if r.Agent != "" && r.Agent != "root" && (r.Type == "turn_started" || r.Type == "turn_ended") {
			continue
		}
		switch r.Type {
		case "turn_started":
			lastStart = r
			acceptedMessages = nil
		case "turn_ended":
			lastEnd = r
		case "operation_accepted":
			if r.operationKind() == "control" {
				lastControl = r
			}
			if r.operationKind() == "message" {
				acceptedMessages = append(acceptedMessages, *r)
			}
		case "failure":
			lastFailure = r
		case "runner_reattached":
			reattached = r
		}
		if r.Receipt != nil && (r.Receipt.Delivery == "failed" || r.Receipt.Error != "") {
			find("delivery of operation %s failed at %s: %s", r.Receipt.OperationID, clock(*r), oneLine(r.Receipt.Error, 160))
		}
	}
	if s.Fault != "" {
		find("fault “%s” blocks messages: the agent takes no new message until it is cleared (sending one clears a turn fault; otherwise resume or start a new conversation)", oneLine(s.Fault, 160))
	}
	for _, p := range s.Pending {
		if strings.Contains(p.Type, "question") {
			find("the agent is blocked on your answer: %s — answer it in the app (lazybox open %s)", oneLine(p.Text, 100), s.Id)
			continue
		}
		find("the agent is blocked on your approval: %s — lazybox approve %s (or deny)", oneLine(p.Text, 100), p.RequestId)
	}
	if lastEnd != nil && (lastStart == nil || lastEnd.Cursor > lastStart.Cursor) && lastEnd.Status == "interrupted" {
		if lastControl != nil && lastControl.Cursor < lastEnd.Cursor {
			find("the last turn ended by an interrupt you sent at %s", clock(*lastControl))
		} else {
			find("the last turn was interrupted at %s", clock(*lastEnd))
		}
	}
	if lastEnd != nil && (lastStart == nil || lastEnd.Cursor > lastStart.Cursor) && lastEnd.Status == "failed" {
		reason := ""
		if lastFailure != nil {
			reason = ": " + strings.TrimSpace(lastFailure.Status+" "+oneLine(lastFailure.Text, 120))
		}
		find("the last turn failed at %s%s", clock(*lastEnd), reason)
	} else if lastFailure != nil && (lastEnd == nil || lastFailure.Cursor > lastEnd.Cursor) {
		find("the provider reported a failure at %s: %s", clock(*lastFailure), strings.TrimSpace(lastFailure.Status+" "+oneLine(lastFailure.Text, 120)))
	}
	if s.Busy && s.Queued > 0 && len(acceptedMessages) > 0 {
		find("a message has been queued behind the running turn for %s; it is sent when the turn ends (lazybox stop interrupts it)", roundAge(now.Sub(acceptedMessages[0].time())))
	} else if s.Queued > 0 && !s.Busy {
		find("%d message(s) queued but no turn is running; the Runner should send them shortly", s.Queued)
	}
	if s.Busy && len(records) > 0 {
		if quiet := now.Sub(records[len(records)-1].time()); quiet > 10*time.Minute {
			find("no event for %s while a turn runs; the agent may be stuck (lazybox stop %s interrupts it)", roundAge(quiet), s.Id)
		}
	}
	if reattached != nil {
		find("the Runner restarted at %s and re-attached this agent", clock(*reattached))
	}
	if s.Closed || s.Historical {
		find("the conversation has ended; lazybox send resumes it")
	}
	if s.RecoveryRequired {
		find("this conversation needs recovery after an unclean Runner stop; resume it from the app")
	}
	if s.ResumeBlocked != "" {
		find("it cannot be resumed: %s", s.ResumeBlocked)
	}
	if len(d.Findings) == 0 {
		switch {
		case s.Busy:
			find("nothing looks wrong: a turn is running")
		default:
			find("nothing looks wrong: idle, nothing pending")
		}
	}
	return d
}

func roundAge(d time.Duration) string {
	switch {
	case d < time.Minute:
		return fmt.Sprintf("%ds", int(d.Seconds()))
	case d < time.Hour:
		return fmt.Sprintf("%dm", int(d.Minutes()))
	}
	return fmt.Sprintf("%dh%02dm", int(d.Hours()), int(d.Minutes())%60)
}

func (c *cli) printDiagnosis(d diagnosis) {
	where := ""
	if d.Workspace != "" {
		where = d.Workspace + " / "
	}
	c.out("%s%s  %s\n", where, c.paint(bold, d.Title), c.paint(dim, d.Session))
	c.out("State     %s", c.paint(stateColour(d.State), stateLabel(d.State)))
	c.out("  busy=%v queued=%d", d.Busy, d.Queued)
	if d.Status != "" {
		c.out(" status=%s", d.Status)
	}
	if d.Permission != "" {
		c.out(" permission=%s", d.Permission)
	}
	c.out("\n")
	if d.Fault != "" {
		c.out("Fault     %s\n", c.paint(red, d.Fault))
	}
	for _, p := range d.Pending {
		c.out("Pending   %s %s %s\n", p.Kind, c.paint(dim, p.ID), oneLine(p.Text, 100))
	}
	for _, a := range d.Agents {
		if a.ParentID != "" {
			c.out("Sub-agent %s %s\n", oneLine(a.Name, 40), c.paint(dim, a.Status))
		}
	}
	c.out("\n%s %s\n", c.paint(bold, "Recent lifecycle"), c.paint(dim, fmt.Sprintf("(%d records, cursor %d)", d.Records, d.Cursor)))
	if len(d.Events) == 0 {
		c.out("  %s\n", c.paint(dim, "no events yet"))
	}
	for _, r := range d.Events {
		kind, text := r.Type, describeRecord(r)
		if kind == "operation_accepted" {
			kind, text = "accepted "+r.operationKind(), quoteLine(r.Text)
		}
		c.out("  %s %s %-14s %-20s %s\n", c.paint(dim, r.time().Local().Format("15:04:05")), c.paint(dim, fmt.Sprintf("#%-5d", r.Cursor)), r.agentLabel(), c.paint(typeColour[r.Type], kind), text)
	}
	c.out("\n%s\n", c.paint(bold, "Findings"))
	for _, finding := range d.Findings {
		c.out("  • %s\n", finding)
	}
}
