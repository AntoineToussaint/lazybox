package acp

import (
	"encoding/json"
	"errors"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

// read turns the agent's lines into journal events. Every event from a line
// carries that line's transport cursor; all but the last are marked partial,
// so the host releases the line only once every event of it is recorded.
func (s *session) read() {
	defer s.cancel()
	var cursor uint64
	var batch []agent.Event
	emit := func(ev agent.Event) {
		ev.Cursor = cursor
		if ev.AgentID == "" {
			ev.AgentID = "root"
		}
		batch = append(batch, ev)
	}
	flush := func() bool {
		for i, ev := range batch {
			ev.Partial = i < len(batch)-1
			if !s.emit(ev) {
				batch = nil
				return false
			}
		}
		batch = batch[:0]
		return true
	}
	phase := ""
	activity := func(status, name string) {
		if key := status + "/" + name; key != phase {
			phase = key
			emit(agent.Event{Type: "activity", Status: status, Name: name})
		}
	}
	for {
		m, err := s.p.Receive(s.ctx)
		cursor = m.Seq
		if err != nil {
			// Only an exit is the process ending: a lost connection to its
			// session host is the Runner's (the stream's end says it), and
			// re-attaching may find the process still running.
			if s.ctx.Err() == nil && errors.Is(err, agent.ErrProcessExited) {
				emit(agent.Event{Type: "failure", Status: agent.ProcessExitFault, Text: err.Error()})
				flush()
			}
			s.end(err)
			return
		}
		switch {
		case m.Method == "":
			if s.answered(m, emit) {
				phase = ""
			}
		case len(m.ID) > 0:
			s.request(m, emit)
		case m.Method == "session/update":
			s.update(m.Params, emit, activity, func() { phase = "responding/" })
		}
		if !flush() {
			s.end(s.ctx.Err())
			return
		}
	}
}

// answered handles a reply that answers no call: the end of a turn, when it
// answers the running session/prompt. It reports whether a turn ended.
func (s *session) answered(m jsonl.Message, emit func(agent.Event)) bool {
	s.mu.Lock()
	if s.loading != nil && string(m.ID) == string(s.loading) {
		// History is replayed up to here; what follows is new.
		s.replaying, s.loading = false, nil
		loaded := s.loaded
		s.mu.Unlock()
		loaded <- m
		return false
	}
	if s.state.Prompt == nil || string(m.ID) != string(s.state.Prompt) {
		s.mu.Unlock()
		return false
	}
	turn := s.state.Turn
	s.state.Prompt, s.state.Turn = nil, ""
	s.mu.Unlock()
	for _, ev := range turnEnd(turn, m.Result, m.Error, s.title) {
		emit(ev)
	}
	return true
}

// turnEnd is the end of a turn as events: why it stopped, and a failure
// first when it did not end on its own or by the person's cancel.
func turnEnd(turn string, result, failure json.RawMessage, title string) []agent.Event {
	end := agent.Event{Type: "turn_ended", TurnID: turn, Status: "completed"}
	if present(failure) {
		// The agent's own words, read the way every adapter call's error is (jsonl.CallError).
		e := jsonl.NewCallError("session/prompt", failure)
		reason := e.Reason()
		said := reason != ""
		fault := agent.Event{Type: "failure", TurnID: turn, Status: "provider_error", Text: title + " ended the turn with an error."}
		if said {
			fault.Text = reason
		}
		if e.Code == authRequired {
			fault.Status = "auth_required"
			fault.Account = &agent.AccountObservation{State: "auth_required", Reason: "acp_auth_required"}
			if !said {
				fault.Text = title + " needs you to sign in again."
			}
		}
		end.Status = "failed"
		end.Data = failure
		return []agent.Event{fault, end}
	}
	var r struct {
		StopReason string `json:"stopReason"`
	}
	_ = json.Unmarshal(result, &r)
	switch r.StopReason {
	case "end_turn", "":
		return []agent.Event{end}
	case "cancelled":
		end.Status = "interrupted"
		return []agent.Event{end}
	}
	fault := agent.Event{Type: "failure", TurnID: turn, Status: r.StopReason}
	switch r.StopReason {
	case "max_tokens":
		fault.Text = title + " stopped: the reply reached its token limit."
	case "max_turn_requests":
		fault.Status = agent.LimitState("error_max_turns")
		fault.Text = title + " stopped: the turn reached its limit of model requests."
	case "refusal":
		fault.Text = title + " declined to continue this request."
	default:
		fault.Text = title + " stopped: " + r.StopReason + "."
	}
	end.Status = "failed"
	return []agent.Event{fault, end}
}

// request serves one of the agent's requests to the client.
func (s *session) request(m jsonl.Message, emit func(agent.Event)) {
	switch m.Method {
	case "session/request_permission":
		s.permission(m, emit)
	case "fs/read_text_file":
		s.serve(m, s.readFile)
	case "fs/write_text_file":
		s.serve(m, s.writeFile)
	case "terminal/create":
		if m.Redelivered {
			// It may already have started under the previous Runner.
			_ = s.p.RejectWith(m.ID, -32603, "The Runner restarted while starting this command; whether it started is unknown. Check the conversation's console before running it again.")
			return
		}
		go s.serve(m, s.createTerminal)
	case "terminal/wait_for_exit":
		go s.serve(m, s.waitTerminal)
	case "terminal/output":
		s.serve(m, s.terminalOutput)
	case "terminal/kill":
		s.serve(m, s.killTerminal)
	case "terminal/release":
		s.serve(m, s.releaseTerminal)
	default:
		_ = s.p.RejectWith(m.ID, -32601, "Lazybox does not offer "+m.Method)
		emit(agent.Event{Type: "unsupported_request", Text: m.Method})
	}
}

// clientError is a JSON-RPC error Lazybox answers a client request with.
type clientError struct {
	code    int
	message string
}

func (e clientError) Error() string { return e.message }

const (
	invalidParams    = -32602
	internalError    = -32603
	resourceNotFound = -32002
)

// serve answers a client request with handle's result or error.
func (s *session) serve(m jsonl.Message, handle func(json.RawMessage) (any, error)) {
	result, err := handle(m.Params)
	if err != nil {
		code := internalError
		var ce clientError
		if errors.As(err, &ce) {
			code = ce.code
		}
		_ = s.p.RejectWith(m.ID, code, err.Error())
		return
	}
	_ = s.p.Reply(m.ID, result)
}
