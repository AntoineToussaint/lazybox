package main

import (
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
	"sync"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
)

// `lazybox watch` follows a conversation (or every live conversation of a
// workspace) as a readable timeline, or as NDJSON for scripts and agents.

// record is one journal record as `watch --json` prints it.
type record struct {
	Cursor       uint64          `json:"cursor"`
	At           string          `json:"at"`
	Conversation string          `json:"conversation,omitempty"`
	Session      string          `json:"session"`
	Title        string          `json:"title,omitempty"`
	Type         string          `json:"type"`
	Agent        string          `json:"agent,omitempty"`
	AgentName    string          `json:"agentName,omitempty"`
	Parent       string          `json:"parent,omitempty"`
	Status       string          `json:"status,omitempty"`
	Text         string          `json:"text,omitempty"`
	RequestID    string          `json:"requestId,omitempty"`
	OperationID  string          `json:"operationId,omitempty"`
	Receipt      *jsonReceipt    `json:"receipt,omitempty"`
	Data         json.RawMessage `json:"data,omitempty"`
	// Replay: the record existed before the watch began.
	Replay bool `json:"replay,omitempty"`
}

type jsonReceipt struct {
	OperationID string `json:"operationId"`
	Delivery    string `json:"delivery"`
	Error       string `json:"error,omitempty"`
}

func toRecord(j *pb.JournalEvent, v conversation, session string) record {
	r := record{Cursor: j.Cursor, At: j.CapturedAt, Conversation: v.Key(), Session: session, Title: v.Title()}
	if e := j.Event; e != nil {
		r.Type, r.Agent, r.AgentName, r.Parent, r.Status, r.Text, r.RequestID, r.OperationID = e.Type, e.AgentId, e.Name, e.ParentAgentId, e.Status, e.Text, e.RequestId, e.OperationId
		if len(e.ProviderDataJson) > 0 && json.Valid(e.ProviderDataJson) {
			r.Data = e.ProviderDataJson
		}
	}
	if j.Receipt != nil {
		r.Receipt = &jsonReceipt{OperationID: j.Receipt.OperationId, Delivery: j.Receipt.Delivery, Error: j.Receipt.Error}
		if r.Type == "" {
			r.Type = "receipt"
		}
	}
	return r
}

func (r record) time() time.Time {
	t, _ := time.Parse(time.RFC3339Nano, r.At)
	return t
}

// agentLabel names who an event is about: "main", or a sub-agent's name.
func (r record) agentLabel() string {
	switch {
	case r.Agent == "" || r.Agent == "root":
		return "main"
	case r.AgentName != "":
		return oneLine(r.AgentName, 14)
	}
	return oneLine(r.Agent, 14)
}

// operationKind infers what an accepted operation was. The journal records
// its kind, but the wire event does not carry it: a message has text, a
// response names a request, anything else is a control (interrupt,
// permission, stop).
func (r record) operationKind() string {
	switch {
	case r.RequestID != "":
		return "response"
	case r.Text != "":
		return "message"
	}
	return "control"
}

var typeColour = map[string]string{
	"turn_started": cyan, "turn_ended": cyan, "approval": yellow, "question": yellow, "request_resolved": green,
	"failure": red, "session_closed": dim, "runner_reattached": magenta, "operation_accepted": blue, "receipt": dim,
	"agent_status": dim, "message": "", "tool_call": dim, "activity": dim, "profile_changed": magenta, "process_exit": red,
}

// describeRecord is the timeline's text for one record.
func describeRecord(r record) string {
	switch r.Type {
	case "operation_accepted":
		return strings.TrimSpace(r.operationKind() + " " + quoteLine(r.Text))
	case "receipt":
		if r.Receipt != nil {
			text := r.Receipt.Delivery
			if r.Receipt.Error != "" {
				text += ": " + r.Receipt.Error
			}
			return text
		}
	case "turn_ended", "failure", "agent_status", "process_exit", "request_resolved":
		return strings.TrimSpace(r.Status + " " + oneLine(r.Text, 100))
	case "approval", "question":
		return strings.TrimSpace(oneLine(r.Text, 100) + " " + r.RequestID)
	}
	return oneLine(r.Text, 100)
}

func quoteLine(text string) string {
	if text == "" {
		return ""
	}
	return "“" + oneLine(text, 80) + "”"
}

// timeline prints records, coalescing a message's streamed deltas into one line.
type timeline struct {
	c        *cli
	mu       sync.Mutex
	labelled bool
	// deltas holds a message being streamed, per session and agent.
	deltas map[string]*delta
	emit   func(string)
}

type delta struct {
	r    record
	text strings.Builder
	last time.Time
}

// newTimeline prints through emit; one timeline per followed conversation.
func newTimeline(c *cli, labelled bool, emit func(string)) *timeline {
	return &timeline{c: c, labelled: labelled, deltas: map[string]*delta{}, emit: emit}
}

func (t *timeline) line(r record, kind, text string) string {
	at := r.time().Local().Format("15:04:05")
	label := ""
	if t.labelled {
		label = t.c.paint(bold, fmt.Sprintf("%-18s ", oneLine(r.Title, 18)))
	}
	return fmt.Sprintf("%s %s%s %s %s", t.c.paint(dim, at), label, t.c.paint(magenta, fmt.Sprintf("%-14s", r.agentLabel())), t.c.paint(typeColour[kind], fmt.Sprintf("%-18s", kind)), text)
}

func (t *timeline) add(r record) {
	t.mu.Lock()
	defer t.mu.Unlock()
	key := r.Session + "\x00" + r.agentLabel()
	if r.Type == "message_delta" {
		d := t.deltas[key]
		if d == nil {
			d = &delta{r: r}
			t.deltas[key] = d
		}
		d.text.WriteString(r.Text)
		d.last = time.Now()
		return
	}
	t.flushSessionLocked(r.Session)
	if r.Type == "" {
		return
	}
	t.emit(t.line(r, r.Type, describeRecord(r)))
}

func (t *timeline) flushSessionLocked(session string) {
	for key, d := range t.deltas {
		if d.r.Session == session {
			t.emit(t.line(d.r, "message", quoteLine(lastParagraph(d.text.String()))))
			delete(t.deltas, key)
		}
	}
}

// flushIdle prints messages that stopped streaming a moment ago.
func (t *timeline) flushIdle(quiet time.Duration) {
	t.mu.Lock()
	defer t.mu.Unlock()
	for key, d := range t.deltas {
		if time.Since(d.last) >= quiet {
			t.emit(t.line(d.r, "message", quoteLine(lastParagraph(d.text.String()))))
			delete(t.deltas, key)
		}
	}
}

func lastParagraph(text string) string {
	parts := strings.Split(strings.TrimSpace(text), "\n\n")
	return parts[len(parts)-1]
}

func (c *cli) watch(args []string) error {
	f := c.flags("watch")
	since := f.String("since", "", "replay from a cursor (a number) or a duration ago (10m) instead of the last few events")
	tail := f.Int("tail", 20, "events of history to show before following live")
	positional, err := c.parse(f, args)
	if err != nil {
		return err
	}
	if len(positional) != 1 {
		return fail(exitUsage, "usage: lazybox watch <conversation|workspace> [--json] [--since CURSOR|DURATION]")
	}
	from, after := time.Time{}, uint64(0)
	if *since != "" {
		if n, err := strconv.ParseUint(*since, 10, 64); err == nil {
			after = n
		} else if d, err := time.ParseDuration(*since); err == nil && d > 0 {
			from = c.now().Add(-d)
		} else {
			return fail(exitUsage, "--since is a cursor (a number) or a duration such as 10m")
		}
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
	w, wsErr := k.findWorkspace(ref)
	if wsErr == nil && !strings.Contains(ref, "/") {
		if v, err := k.findConversation(ref); err == nil && strings.EqualFold(v.Title(), ref) && !strings.EqualFold(w.Name, ref) {
			return c.follow(cl, []conversation{v}, false, after, from, *since != "", *tail)
		}
		return c.followWorkspace(cl, w, after, from, *since != "", *tail)
	}
	v, err := k.findConversation(ref)
	if err != nil {
		return err
	}
	return c.follow(cl, []conversation{v}, false, after, from, *since != "", *tail)
}
