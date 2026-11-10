package codex

import (
	"context"
	"encoding/json"
	"errors"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/jsonl"
)

type Adapter struct{ Binary string }
type pending struct {
	message  jsonl.Message
	tool     string
	args     json.RawMessage
	answered bool
}
type session struct {
	mu              sync.Mutex
	p               *jsonl.Process
	ref, turn       string
	model, workflow string
	settings        json.RawMessage
	overrides       map[string]json.RawMessage
	requests        map[string]pending
	events          chan agent.Event
	ctx             context.Context
	cancel          context.CancelFunc
	tools           func(context.Context, string, json.RawMessage) (string, error)
	// responded: the user's responses to requests answered before a Runner restart.
	responded map[string]agent.Operation
}

// GatewayArgs deliberately selects an HTTP Responses provider. Setting only
// OPENAI_BASE_URL would let subscription traffic bypass lazybox metering.
func GatewayArgs(g *agent.Gateway) []string {
	if g == nil {
		return nil
	}
	config := []string{`model_provider="lazybox_gateway"`, `model_providers.lazybox_gateway.name="lazybox gateway"`, `model_providers.lazybox_gateway.base_url=` + strconv.Quote(strings.TrimRight(g.URL, "/")), `model_providers.lazybox_gateway.wire_api="responses"`, `model_providers.lazybox_gateway.supports_websockets=false`}
	if g.TokenEnv == "" {
		config = append(config, `model_providers.lazybox_gateway.requires_openai_auth=true`)
	} else {
		config = append(config, `model_providers.lazybox_gateway.requires_openai_auth=false`, `model_providers.lazybox_gateway.env_key=`+strconv.Quote(g.TokenEnv))
	}
	args := []string{}
	for _, c := range config {
		args = append(args, "-c", c)
	}
	return args
}

func (a Adapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	if len(o.ProviderSettings) > 0 {
		var carried map[string]json.RawMessage
		if json.Unmarshal(o.ProviderSettings, &carried) != nil {
			return nil, errors.New("invalid carried provider settings")
		}
		if _, custom := carried["_namedPermissionProfile"]; custom {
			return nil, agent.ErrUnsupported
		}
	}
	binary := a.Binary
	if binary == "" {
		binary = "codex"
	}
	if o.Host != nil && o.Host.Attach != nil {
		return attach(ctx, o)
	}
	gateway := o.Gateway
	life, cancel := context.WithCancel(ctx)
	var host *jsonl.Host
	if o.Host != nil {
		var err error
		if host, err = jsonl.Launch(life, o.Host.Root); err != nil {
			cancel()
			return nil, err
		}
		if gateway != nil {
			// The agent's configured endpoint is the host's session-stable proxy.
			proxied := *gateway
			proxied.URL = host.ProxyURL()
			gateway = &proxied
		}
	}
	args := append([]string{"app-server", "--stdio"}, GatewayArgs(gateway)...)
	if o.Isolated {
		args = append(args, "-c", `cli_auth_credentials_store="file"`)
	}
	var p *jsonl.Process
	var err error
	if host != nil {
		if p, err = host.Start(life, binary, args, o.Env, o.Cwd); err == nil && o.Gateway != nil {
			if err = p.SetUpstream(o.Gateway.URL); err != nil {
				p.Close()
			}
		}
	} else {
		p, err = jsonl.Start(life, binary, args, o.Env, o.Cwd)
	}
	if err != nil {
		cancel()
		return nil, err
	}
	fail := func(err error) (agent.Session, error) { cancel(); p.Close(); return nil, err }
	setup, done := context.WithTimeout(life, 30*time.Second)
	defer done()
	_, err = p.Call(setup, "initialize", map[string]any{"clientInfo": map[string]string{"name": "lazybox_v2_testbench", "title": "Lazybox V2 test bench", "version": "0.1.0"}, "capabilities": map[string]any{"experimentalApi": true}})
	if err != nil {
		return fail(err)
	}
	if err = p.Notify("initialized", map[string]any{}); err != nil {
		return fail(err)
	}
	params := threadParams(o)
	if agent.IsAssistant(o.Purpose) {
		result, err := p.Call(setup, "config/read", map[string]any{"cwd": o.Cwd, "includeLayers": false})
		if err != nil {
			return fail(err)
		}
		var resolved struct {
			Config struct {
				MCP     map[string]json.RawMessage `json:"mcp_servers"`
				Plugins map[string]json.RawMessage `json:"plugins"`
			}
		}
		if err = json.Unmarshal(result, &resolved); err != nil {
			return fail(err)
		}
		config := map[string]any{"features.shell_tool": false, "features.goals": false, "features.view_image": false, "tools.experimental_request_user_input.enabled": false, "features.multi_agent": false, "features.multi_agent_v2": false, "features.apps": false, "web_search": "disabled"}
		for name := range resolved.Config.MCP {
			config["mcp_servers."+strconv.Quote(name)+".enabled"] = false
		}
		for name := range resolved.Config.Plugins {
			config["plugins."+strconv.Quote(name)+".enabled"] = false
		}
		params["config"] = config
	}
	method := "thread/start"
	if o.Resume != "" {
		method = "thread/resume"
		params["threadId"] = o.Resume
	} else if o.Tools != nil {
		params["dynamicTools"] = []any{map[string]any{"type": "function", "name": "lazybox_command", "description": "Start, read, wait for or cancel a runner-owned command. Output remains available to independent human consoles. Start requires human approval. Never repeat a start after receiving its command id.", "inputSchema": json.RawMessage(`{"type":"object","properties":{"action":{"type":"string","enum":["start","read","wait","cancel"]},"argv":{"type":"array","items":{"type":"string"}},"id":{"type":"string"},"offset":{"type":"integer","minimum":0}},"required":["action"],"additionalProperties":false}`)}}
	}
	result, err := p.Call(setup, method, params)
	if err != nil {
		return fail(err)
	}
	var response struct {
		Model             string          `json:"model"`
		Approval          json.RawMessage `json:"approvalPolicy"`
		Reviewer          json.RawMessage `json:"approvalsReviewer"`
		Sandbox           json.RawMessage `json:"sandbox"`
		PermissionProfile *struct {
			ID string `json:"id"`
		} `json:"activePermissionProfile"`
		Thread struct {
			ID string `json:"id"`
		} `json:"thread"`
	}
	if err = json.Unmarshal(result, &response); err != nil || response.Thread.ID == "" {
		return fail(errors.New("Codex omitted native thread id"))
	}
	s := &session{p: p, ref: response.Thread.ID, model: response.Model, workflow: o.Workflow, ctx: life, cancel: cancel, requests: map[string]pending{}, events: make(chan agent.Event, 256), tools: o.Tools}
	settings := map[string]json.RawMessage{}
	for key, value := range map[string]json.RawMessage{"approvalPolicy": response.Approval, "approvalsReviewer": response.Reviewer, "sandboxPolicy": response.Sandbox} {
		if len(value) > 0 && string(value) != "null" {
			settings[key] = value
		}
	}
	if response.Model != "" {
		settings["model"], _ = json.Marshal(response.Model)
	}
	if response.PermissionProfile != nil && response.PermissionProfile.ID != "" && !strings.HasPrefix(response.PermissionProfile.ID, ":") {
		settings["_namedPermissionProfile"], _ = json.Marshal(response.PermissionProfile.ID)
	}
	s.settings, _ = json.Marshal(settings)
	if len(o.ProviderSettings) > 0 {
		if err = json.Unmarshal(o.ProviderSettings, &s.overrides); err != nil {
			return fail(errors.New("invalid carried provider settings"))
		}
		s.settings = append(json.RawMessage(nil), o.ProviderSettings...)
		if model := s.overrides["model"]; len(model) > 0 {
			_ = json.Unmarshal(model, &s.model)
		}
	}
	go s.read()
	return s, nil
}

// durable is everything this adapter needs to resume supervising a live
// app-server. Pending server requests are not stored: the host redelivers them.
type durable struct {
	Host      string                     `json:"host"`
	Turn      string                     `json:"turn,omitempty"`
	Model     string                     `json:"model,omitempty"`
	Settings  json.RawMessage            `json:"settings,omitempty"`
	Overrides map[string]json.RawMessage `json:"overrides,omitempty"`
}

// runTool runs an approved dynamic tool call and answers the app-server.
func (s *session) runTool(native json.RawMessage, request, tool string, args json.RawMessage) {
	if s.tools == nil {
		s.replyTool(native, request, "managed commands unavailable", false)
		return
	}
	text, err := s.tools(s.ctx, tool, args)
	if err != nil {
		text = err.Error()
	}
	s.replyTool(native, request, text, err == nil)
}

func (s *session) replyTool(native json.RawMessage, request, text string, success bool) {
	err := s.p.Reply(native, map[string]any{"success": success, "contentItems": []any{map[string]string{"type": "inputText", "text": text}}})
	if err == nil {
		s.emit(agent.Event{Type: "request_resolved", RequestID: request, Status: "response_written"})
	} else {
		s.emit(agent.Event{Type: "failure", Status: "tool_reply_failed", Text: err.Error()})
	}
}

const toolOutcomeUnknown = "The Runner restarted while this approved lazybox_command was running; its outcome is unknown. Inspect the workspace and command list before retrying."

func attach(ctx context.Context, o agent.Open) (agent.Session, error) {
	var state durable
	if err := json.Unmarshal(o.Host.Attach.State, &state); err != nil || state.Host == "" {
		return nil, errors.New("Codex session has no re-attachable host")
	}
	life, cancel := context.WithCancel(ctx)
	p, err := jsonl.Attach(life, o.Host.Root, state.Host, o.Host.Attach.Cursor)
	if err != nil {
		cancel()
		return nil, err
	}
	if o.Gateway != nil {
		if err = p.SetUpstream(o.Gateway.URL); err != nil {
			cancel()
			_ = p.Detach()
			return nil, err
		}
	}
	s := &session{p: p, ref: o.Host.Attach.Reference, turn: state.Turn, model: state.Model, workflow: o.Workflow, settings: state.Settings, overrides: state.Overrides, ctx: life, cancel: cancel, requests: map[string]pending{}, events: make(chan agent.Event, 256), tools: o.Tools, responded: o.Host.Attach.Responded}
	go s.read()
	return s, nil
}

func (s *session) DurableState() json.RawMessage {
	if s.p.HostID() == "" {
		return nil
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	data, _ := json.Marshal(durable{Host: s.p.HostID(), Turn: s.turn, Model: s.model, Settings: s.settings, Overrides: s.overrides})
	return data
}

// Commit acknowledges a native line once its last event is recorded.
func (s *session) Commit(ev agent.Event) {
	if !ev.Partial {
		s.p.Commit(ev.Cursor)
	}
}
func (s *session) Detach() error {
	err := s.p.Detach()
	if err == nil {
		s.cancel()
	}
	return err
}

func threadParams(o agent.Open) map[string]any {
	params := map[string]any{"cwd": o.Cwd}
	// Omitted policies let app-server resolve the user's saved configuration
	// (and preserve the native policy on resume). Only explicit choices override.
	switch o.Permission {
	case "yolo":
		params["approvalPolicy"], params["sandbox"], params["approvalsReviewer"] = "never", "danger-full-access", "user"
	case "ask":
		params["approvalPolicy"], params["sandbox"], params["approvalsReviewer"] = "untrusted", "workspace-write", "user"
	case "read-only":
		params["approvalPolicy"], params["sandbox"], params["approvalsReviewer"] = "never", "read-only", "user"
	}
	if o.Context != "" {
		params["developerInstructions"] = o.Context
	}
	if o.Model != "" {
		params["model"] = o.Model
	}
	return params
}

func (s *session) SessionSettings() json.RawMessage {
	return append(json.RawMessage(nil), s.settings...)
}
func (s *session) Reference() string { s.mu.Lock(); defer s.mu.Unlock(); return s.ref }
func (s *session) Capabilities() agent.Capabilities {
	return agent.Capabilities{Images: true, Files: true, Questions: s.workflow == "plan", Approvals: true, Steer: true, Resume: true, Subagents: true, ProfileIsolation: "environment; cross-account resume unverified"}
}
func (s *session) emit(ev agent.Event) bool {
	select {
	case s.events <- ev:
		return true
	case <-s.ctx.Done():
		return false
	}
}
func inputs(op agent.Operation) []any {
	items := []any{}
	if text := agent.Prompt(op); text != "" {
		items = append(items, map[string]any{"type": "text", "text": text})
	}
	for _, img := range op.Images {
		items = append(items, map[string]any{"type": "image", "url": "data:" + img.MediaType + ";base64," + img.Data})
	}
	return items
}
func (s *session) Apply(ctx context.Context, op agent.Operation) (agent.Receipt, error) {
	r := agent.Receipt{ID: op.ID, Delivery: "acknowledged"}
	s.mu.Lock()
	ref, turn := s.ref, s.turn
	s.mu.Unlock()
	switch op.Kind {
	case agent.Message, agent.Steer, agent.Interrupt:
		method := "turn/start"
		params := map[string]any{"threadId": ref}
		if op.Kind == agent.Message {
			for key, value := range s.overrides {
				params[key] = value
			}
		}
		if op.Kind == agent.Message && s.workflow == "plan" {
			params["collaborationMode"] = map[string]any{"mode": "plan", "settings": map[string]any{"model": s.model}}
		}
		if op.Kind != agent.Interrupt {
			params["input"] = inputs(op)
		}
		if op.Kind == agent.Steer {
			method = "turn/steer"
			params["expectedTurnId"] = turn
		}
		if op.Kind == agent.Interrupt {
			method = "turn/interrupt"
			params["turnId"] = turn
		}
		result, err := s.p.Call(ctx, method, params)
		if err != nil {
			return r, err
		}
		if op.Kind == agent.Message {
			var response struct {
				Turn struct {
					ID string `json:"id"`
				} `json:"turn"`
			}
			if err = json.Unmarshal(result, &response); err != nil || response.Turn.ID == "" {
				return r, errors.New("Codex omitted native turn id")
			}
			s.mu.Lock()
			s.turn = response.Turn.ID
			s.mu.Unlock()
			r.ProviderID = response.Turn.ID
		}
		return r, nil
	case agent.Answer, agent.Approve:
		s.mu.Lock()
		p, ok := s.requests[op.RequestID]
		if !ok || p.answered {
			s.mu.Unlock()
			return r, errors.New("unknown or already answered native request")
		}
		if p.message.Method == "item/permissions/requestApproval" && op.Decision == "accept" {
			s.mu.Unlock()
			return r, agent.ErrUnsupported
		}
		p.answered = true
		s.requests[op.RequestID] = p
		s.mu.Unlock()
		if p.tool != "" {
			if op.Decision == "accept" {
				go s.runTool(p.message.ID, op.RequestID, p.tool, p.args)
			} else {
				go s.replyTool(p.message.ID, op.RequestID, "Command declined", false)
			}
			r.Delivery = "submitted"
			return r, nil
		}
		response, err := nativeResponse(p.message.Method, op)
		if err != nil {
			return r, err
		}
		err = s.p.Reply(p.message.ID, response)
		r.Delivery = "submitted"
		return r, err
	default:
		return r, agent.ErrUnsupported
	}
}

// nativeResponse is the app-server reply to a native approval or question.
func nativeResponse(method string, op agent.Operation) (any, error) {
	if op.Kind == agent.Answer {
		answers := map[string]any{}
		for k, v := range op.Answers {
			answers[k] = map[string]any{"answers": v}
		}
		return map[string]any{"answers": answers}, nil
	}
	if method == "item/permissions/requestApproval" {
		if op.Decision == "accept" {
			return nil, errors.New("permission-subset grants require a supported explicit grant UI")
		}
		return map[string]any{"permissions": map[string]any{}}, nil
	}
	return map[string]any{"decision": op.Decision}, nil
}

func requestID(raw json.RawMessage) string {
	var text string
	if json.Unmarshal(raw, &text) == nil {
		return "s:" + text
	}
	return "n:" + string(raw)
}
func (s *session) read() {
	defer s.cancel()
	// Every event derived from a native line carries that line's transport
	// cursor. A line's events are delivered together and all but the last are
	// marked partial, so the line is acknowledged only once all are recorded.
	var cursor uint64
	var batch []agent.Event
	emit := func(ev agent.Event) { ev.Cursor = cursor; batch = append(batch, ev) }
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
	phases := map[string]string{}
	activity := func(ev agent.Event) {
		key := ev.Status + "/" + ev.Name
		if phases[ev.AgentID] == key {
			return
		}
		phases[ev.AgentID] = key
		emit(ev)
	}
	for {
		m, err := s.p.Receive(s.ctx)
		cursor = m.Seq
		if err != nil {
			if s.ctx.Err() == nil && !errors.Is(err, jsonl.ErrDetached) {
				emit(agent.Event{Type: "failure", Status: "provider_process_exit", Text: err.Error()})
				flush()
			}
			return
		}
		ok := func() bool {
			var p struct {
				WillRetry bool `json:"willRetry"`
				Error     struct {
					Code json.RawMessage `json:"codexErrorInfo"`
				} `json:"error"`
				ThreadID  string          `json:"threadId"`
				TurnID    string          `json:"turnId"`
				RequestID json.RawMessage `json:"requestId"`
				Delta     string          `json:"delta"`
				Turn      struct {
					ID     string          `json:"id"`
					Status string          `json:"status"`
					Error  json.RawMessage `json:"error"`
				} `json:"turn"`
				Tool      string          `json:"tool"`
				Arguments json.RawMessage `json:"arguments"`
			}
			if err = json.Unmarshal(m.Params, &p); err != nil {
				emit(agent.Event{Type: "failure", Status: "protocol_error", Text: err.Error()})
				return false
			}
			root := s.Reference()
			ev := agent.Event{SessionID: p.ThreadID, TurnID: p.TurnID, AgentID: actorID(p.ThreadID, root)}
			if ev.AgentID != "root" {
				ev.ParentAgentID = "root"
				ev.NativeID = p.ThreadID
			}
			if len(m.ID) > 0 {
				id := requestID(m.ID)
				ev.RequestID = id
				ev.Data = m.Params
				// Redelivered after the user answered: the reply never reached
				// the app-server.
				response, answered := s.responded[id]
				answered = answered && m.Redelivered
				switch m.Method {
				case "item/commandExecution/requestApproval", "item/fileChange/requestApproval", "item/permissions/requestApproval":
					ev.Type = "approval"
					ev.Text = m.Method
				case "item/tool/requestUserInput", "tool/requestUserInput":
					ev.Type = "question"
				case "item/tool/call":
					if p.Tool != "lazybox_command" {
						_ = s.p.Reject(m.ID, "unknown dynamic tool")
						return true
					}
					switch {
					case !answered:
					case response.Decision != "" && response.Decision != "accept":
						// Declined: nothing ran, so the decline is simply repeated.
						go s.replyTool(m.ID, id, "Command declined", false)
						return true
					case agent.ToolReplaySafe(p.Tool, p.Arguments):
						// Only inspects a command that outlived the Runner: run it again.
						go s.runTool(m.ID, id, p.Tool, p.Arguments)
						return true
					default:
						// Approved (or recorded without its decision): the command
						// may already have run.
						_ = s.p.Reply(m.ID, map[string]any{"success": false, "contentItems": []any{map[string]string{"type": "inputText", "text": toolOutcomeUnknown}}})
						emit(agent.Event{Type: "request_resolved", RequestID: id, Status: "outcome_unknown_after_restart"})
						return true
					}
					ev.Type = "approval"
					ev.Text = "Run managed command tool"
				default:
					_ = s.p.Reject(m.ID, "unsupported server request")
					emit(agent.Event{Type: "unsupported_request", Text: m.Method, Data: m.Params})
					return true
				}
				if answered && p.Tool == "" && (response.Kind == agent.Answer || response.Decision != "") {
					// The app-server acts only once it has the reply, so the
					// user's recorded response is given again rather than asked anew.
					if reply, err := nativeResponse(m.Method, response); err == nil && s.p.Reply(m.ID, reply) == nil {
						return true
					}
				}
				s.mu.Lock()
				s.requests[id] = pending{message: m, tool: p.Tool, args: p.Arguments}
				s.mu.Unlock()
				emit(ev)
				return true
			}
			switch m.Method {
			case "turn/started":
				ev.Type = "turn_started"
				ev.TurnID = p.Turn.ID
				s.mu.Lock()
				if ev.AgentID == "root" {
					s.turn = p.Turn.ID
				}
				s.mu.Unlock()
			case "turn/completed":
				ev.Type = "turn_ended"
				ev.TurnID = p.Turn.ID
				ev.Status = p.Turn.Status
				ev.Data = p.Turn.Error
				if p.Turn.Status == "failed" && len(p.Turn.Error) > 0 && string(p.Turn.Error) != "null" {
					failure := ev
					failure.Type = "failure"
					failure.Account = errorObservation(p.Turn.Error)
					failure.Status = failure.Account.State
					emit(failure)
				}
				s.mu.Lock()
				if ev.AgentID == "root" {
					s.turn = ""
				}
				for id, request := range s.requests {
					var scope struct{ ThreadID string }
					_ = json.Unmarshal(request.message.Params, &scope)
					if scope.ThreadID == p.ThreadID {
						delete(s.requests, id)
					}
				}
				s.mu.Unlock()
			case "item/reasoning/summaryTextDelta", "item/reasoning/textDelta":
				ev.Type, ev.Status = "activity", "thinking"
				ev.Data = nil // Activity only; never expose private reasoning text.
			case "item/agentMessage/delta":
				ev.Type = "message_delta"
				ev.Text = p.Delta
			case "serverRequest/resolved":
				ev.Type = "request_resolved"
				ev.RequestID = requestID(p.RequestID)
				s.mu.Lock()
				delete(s.requests, ev.RequestID)
				s.mu.Unlock()
			case "account/rateLimits/updated":
				ev.Type = "capacity_observation"
				ev.Account = rateLimits(m.Params)
				ev.Status = ev.Account.State
			case "thread/tokenUsage/updated":
				ev.Type = "usage"
				ev.Data = m.Params
			case "error":
				ev.Type = "failure"
				ev.Status = "unknown"
				detail, _ := json.Marshal(p.Error)
				ev.Account = errorObservation(detail)
				ev.Status = ev.Account.State
				if p.WillRetry {
					ev.Type = "provider_retry"
				}
				ev.Data = m.Params
			case "thread/status/changed":
				ev.Type, ev.Status = "agent_status", threadStatus(m.Params)
			case "thread/started":
				return true
			case "item/started", "item/completed":
				for _, activity := range collaborationEvents(m.Params, root) {
					emit(activity)
				}
				ev.Type = "tool_activity"
				ev.Status = "running"
				if m.Method == "item/completed" {
					ev.Status = "completed"
				}
				ev.Data, _ = normalizedToolCall(m.Params, m.Method == "item/completed")
				var detail struct {
					Item struct {
						Type string `json:"type"`
						Tool string `json:"tool"`
					} `json:"item"`
				}
				if json.Unmarshal(m.Params, &detail) == nil {
					phase, name := "working", ""
					if m.Method == "item/started" {
						switch detail.Item.Type {
						case "reasoning":
							phase = "thinking"
						case "agentMessage":
							phase = "responding"
						case "commandExecution":
							phase, name = "tool", "command"
						case "mcpToolCall":
							phase, name = "tool", detail.Item.Tool
						}
					}
					activity(agent.Event{Type: "activity", AgentID: ev.AgentID, Status: phase, Name: name})
				}
				if len(ev.Data) == 0 {
					return true
				}
			default:
				return true
			}
			if ev.Type == "activity" {
				activity(ev)
				return true
			}
			if ev.Type == "message_delta" {
				phases[ev.AgentID] = "responding/"
			}
			if ev.Type == "turn_ended" {
				delete(phases, ev.AgentID)
			}
			emit(ev)
			return true
		}()
		if !flush() || !ok {
			return
		}
	}
}
func (s *session) Receive(ctx context.Context) (agent.Event, error) {
	select {
	case e, ok := <-s.events:
		if !ok {
			return e, errors.New("Codex connection closed")
		}
		return e, nil
	case <-ctx.Done():
		return agent.Event{}, ctx.Err()
	}
}
func (s *session) Close() error { err := s.p.Close(); s.cancel(); return err }
