package acp

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

type permissionOption struct {
	ID   string `json:"optionId"`
	Name string `json:"name"`
	Kind string `json:"kind"`
}

type permissionRequest struct {
	SessionID string             `json:"sessionId"`
	ToolCall  json.RawMessage    `json:"toolCall"`
	Options   []permissionOption `json:"options"`
}

// requestID is the journal's id for an agent request.
func requestID(raw json.RawMessage) string {
	var text string
	if json.Unmarshal(raw, &text) == nil {
		return "s:" + text
	}
	return "n:" + string(raw)
}

// AlwaysChoice is an "always allow" answer as clients render it (the request
// data's `always`); ID is the ACP option the person picks.
type AlwaysChoice struct {
	ID     string `json:"id"`
	Scope  string `json:"scope"`
	Label  string `json:"label"`
	Detail string `json:"detail,omitempty"`
}

// readKinds are the tool kinds a read-only conversation lets run: none of
// them changes the workspace.
var readKinds = map[string]bool{"read": true, "search": true, "think": true, "fetch": true}

// permission puts the agent's permission request to the person as an
// approval card, unless the conversation's mode already answers it: YOLO
// allows, read-only allows reads and declines the rest.
func (s *session) permission(m jsonl.Message, emit func(agent.Event)) {
	var req permissionRequest
	if json.Unmarshal(m.Params, &req) != nil || len(req.Options) == 0 {
		_ = s.p.RejectWith(m.ID, invalidParams, "a permission request needs a tool call and options")
		return
	}
	call := s.merge(req.ToolCall)
	if call == nil {
		call = &toolCall{}
	}
	id := requestID(m.ID)
	s.mu.Lock()
	permission, workflow, turn := s.state.Permission, s.state.Workflow, s.state.Turn
	response, answered := s.responded[id]
	s.mu.Unlock()
	if call.ID != "" {
		emit(agent.Event{Type: "tool_activity", Status: "running", TurnID: turn, Data: s.normalized(call)})
	}
	if outcome := autoOutcome(permission, workflow, call.Kind, req.Options); outcome != nil {
		_ = s.p.Reply(m.ID, map[string]any{"outcome": outcome})
		return
	}
	if answered && m.Redelivered && response.Decision != "" {
		// The person answered before the Runner restarted; the agent never
		// got the answer, and has done nothing yet: give it again.
		if outcome, err := permissionOutcome(req.Options, response); err == nil && s.p.Reply(m.ID, map[string]any{"outcome": outcome}) == nil {
			return
		}
	}
	s.mu.Lock()
	s.requests[id] = pending{native: m.ID, params: req}
	s.mu.Unlock()
	emit(agent.Event{Type: "approval", RequestID: id, TurnID: turn, Text: "session/request_permission", Data: s.approvalData(call, req.Options)})
}

// autoOutcome answers a request the conversation's mode decides, or nil.
func autoOutcome(permission, workflow, kind string, options []permissionOption) map[string]string {
	pick := func(kinds ...string) map[string]string {
		for _, k := range kinds {
			for _, o := range options {
				if o.Kind == k {
					return map[string]string{"outcome": "selected", "optionId": o.ID}
				}
			}
		}
		return map[string]string{"outcome": "cancelled"}
	}
	switch {
	case permission == "read-only" || workflow == "plan":
		if readKinds[kind] {
			return pick("allow_once", "allow_always")
		}
		return pick("reject_once", "reject_always")
	case permission == "yolo":
		return pick("allow_once", "allow_always")
	}
	return nil
}

// approvalData is the request as the approval card reads it (request-model.ts):
// the call in the same shape as its tool line, its title, the agent's
// "always allow" options, and the agent's own option list.
func (s *session) approvalData(call *toolCall, options []permissionOption) json.RawMessage {
	name, input := toolShape(call)
	always := []AlwaysChoice{}
	for _, o := range options {
		if o.Kind == "allow_always" {
			always = append(always, AlwaysChoice{ID: o.ID, Scope: "conversation", Label: alwaysLabel(o.Name, name, input), Detail: s.title + " stops asking for this; how long it remembers is up to " + s.title + "."})
		}
	}
	title := call.Title
	data := map[string]any{"tool": name, "title": title, "input": input, "always": always, "options": options, "kind": call.Kind, "toolCallId": call.ID, "agent": s.title}
	if cwd := firstString(input, "cwd"); cwd != "" {
		data["cwd"] = cwd
	}
	if len(call.Content) > 0 {
		if out := call.outputText(); out != "" {
			data["description"] = bound(out)
		}
	}
	raw, _ := json.Marshal(data)
	return raw
}

// alwaysLabel says what stops being asked: the option's own words when they
// say more than "Always allow", else what the call is.
func alwaysLabel(option, tool string, input map[string]any) string {
	plain := strings.ToLower(strings.TrimSpace(option))
	if plain != "" && plain != "always allow" && plain != "allow always" && plain != "always" {
		return option
	}
	switch tool {
	case "Bash":
		if command := firstString(input, "command"); command != "" {
			return "`" + firstWords(command, 3) + "` commands"
		}
		return "commands like this"
	case "Edit", "Write", "File changes":
		return "edits like this"
	case "Read", "Grep":
		return "reads like this"
	case "WebFetch", "WebSearch":
		return "web requests like this"
	}
	return strings.ToLower(tool) + " calls"
}

func firstWords(text string, n int) string {
	fields := strings.Fields(text)
	if len(fields) > n {
		fields = fields[:n]
	}
	return strings.Join(fields, " ")
}

// permissionOutcome is the ACP outcome for the person's answer: the option
// they chose, matched by kind. "Always" names an allow_always option the
// request offered; declining picks the agent's reject-once option.
func permissionOutcome(options []permissionOption, op agent.Operation) (map[string]string, error) {
	find := func(kinds ...string) (string, bool) {
		for _, k := range kinds {
			for _, o := range options {
				if o.Kind == k {
					return o.ID, true
				}
			}
		}
		return "", false
	}
	selected := func(id string) map[string]string { return map[string]string{"outcome": "selected", "optionId": id} }
	switch op.Decision {
	case "accept":
		if op.Always != "" {
			for _, o := range options {
				if o.ID == op.Always && o.Kind == "allow_always" {
					return selected(o.ID), nil
				}
			}
			return nil, fmt.Errorf("%w: this request does not offer %q", agent.ErrUnsupported, op.Always)
		}
		if id, ok := find("allow_once"); ok {
			return selected(id), nil
		}
		return nil, fmt.Errorf("%w: the agent only offers to allow this always; choose that option", agent.ErrUnsupported)
	case "decline":
		if id, ok := find("reject_once", "reject_always"); ok {
			return selected(id), nil
		}
		return map[string]string{"outcome": "cancelled"}, nil
	case "cancel":
		return map[string]string{"outcome": "cancelled"}, nil
	}
	return nil, errors.New("decision must be accept, decline or cancel")
}

// approve answers a permission request with the person's decision. A reason
// given with a decline reaches the agent with the next message.
func (s *session) approve(op agent.Operation) (agent.Receipt, error) {
	r := agent.Receipt{ID: op.ID, Delivery: "submitted"}
	s.mu.Lock()
	p, ok := s.requests[op.RequestID]
	if !ok {
		s.mu.Unlock()
		return r, errors.New("unknown or already answered native request")
	}
	outcome, err := permissionOutcome(p.params.Options, op)
	if err != nil {
		s.mu.Unlock()
		return r, err
	}
	delete(s.requests, op.RequestID)
	if op.Decision == "decline" && strings.TrimSpace(op.Reason) != "" {
		what := "that request"
		var call struct {
			Title string `json:"title"`
		}
		if json.Unmarshal(p.params.ToolCall, &call) == nil && call.Title != "" {
			what = "“" + call.Title + "”"
		}
		s.state.Notes = append(s.state.Notes, "I declined "+what+". "+strings.TrimSpace(op.Reason))
	}
	s.mu.Unlock()
	if err = s.p.Reply(p.native, map[string]any{"outcome": outcome}); err != nil {
		return r, err
	}
	s.emit(agent.Event{Type: "request_resolved", RequestID: op.RequestID, Status: "response_written"})
	return r, nil
}
