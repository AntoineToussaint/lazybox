package codex

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

type Adapter struct{ Binary string }
type pending struct {
	message  jsonl.Message
	tool     string
	args     json.RawMessage
	answered bool
}
type session struct {
	cwd             string
	mu              sync.Mutex
	p               *jsonl.Process
	ref, turn       string
	model, workflow string
	turnModel       string
	permission      string
	settings        json.RawMessage
	overrides       map[string]json.RawMessage
	requests        map[string]pending
	events          chan agent.Event
	// ended is closed when read returns (the app-server exited, the Runner
	// detached or closed): Receive then ends with why instead of waiting
	// for events nothing will send.
	ended   chan struct{}
	why     error
	ctx     context.Context
	cancel  context.CancelFunc
	tools   func(context.Context, string, json.RawMessage) (string, error)
	purpose string
	// responded: the user's responses to requests answered before a Runner restart.
	responded map[string]agent.Operation
	// titler names the conversation (title.go).
	titler *titler
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
	if (o.ForkThread == "") != (o.ForkAfterTurn == "") || (o.ForkThread != "" && o.Resume != "") {
		return nil, errors.New("a Codex fork requires a source thread and a completed turn")
	}
	if len(o.ProviderSettings) > 0 {
		var carried map[string]json.RawMessage
		if json.Unmarshal(o.ProviderSettings, &carried) != nil {
			return nil, errors.New("invalid carried provider settings")
		}
		if _, custom := carried["_namedPermissionProfile"]; custom {
			return nil, agent.ErrUnsupported
		}
	}
	binary := codexProgram(a.Binary)
	if o.Host != nil && o.Host.Attach != nil {
		return attach(ctx, o, binary)
	}
	gateway := o.Gateway
	life, cancel := context.WithCancel(ctx)
	var host *jsonl.Host
	if o.Host != nil {
		var err error
		if host, err = jsonl.LaunchOn(life, o.Host.Backend, o.Host.Root, o.HostOptions("codex")); err != nil {
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
	initialized, err := p.Call(setup, "initialize", map[string]any{"clientInfo": map[string]string{"name": "lazybox_v2_testbench", "title": "Lazybox V2 test bench", "version": "0.1.0"}, "capabilities": map[string]any{"experimentalApi": true}})
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
		if o.Effort != "" {
			config["model_reasoning_effort"] = o.Effort
		}
		for name := range resolved.Config.MCP {
			config["mcp_servers."+strconv.Quote(name)+".enabled"] = false
		}
		for name := range resolved.Config.Plugins {
			config["plugins."+strconv.Quote(name)+".enabled"] = false
		}
		params["config"] = config
	}
	method := "thread/start"
	if o.ForkThread != "" {
		method = "thread/fork"
		params["threadId"] = o.ForkThread
		params["lastTurnId"] = o.ForkAfterTurn
		params["excludeTurns"] = true
	} else if o.Resume != "" {
		method = "thread/resume"
		params["threadId"] = o.Resume
	} else if tools := dynamicTools(o.Purpose, o.Browser); o.Tools != nil && tools != nil {
		params["dynamicTools"] = tools
	}
	result, err := p.Call(setup, method, params)
	if err != nil {
		return fail(err)
	}
	var response struct {
		Model             string          `json:"model"`
		Cwd               string          `json:"cwd"`
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
	if o.ForkThread != "" {
		if response.Thread.ID == o.ForkThread {
			return fail(errors.New("Codex returned the source thread for a fork"))
		}
		if err := verifyFork(setup, p, response.Thread.ID, o.ForkAfterTurn, o.Cwd, response.Cwd); err != nil {
			_, archiveErr := p.Call(setup, "thread/archive", map[string]any{"threadId": response.Thread.ID})
			return fail(errors.Join(err, archiveErr))
		}
	}
	s := &session{p: p, cwd: o.Cwd, ref: response.Thread.ID, model: response.Model, workflow: o.Workflow, permission: o.Permission, ctx: life, cancel: cancel, requests: map[string]pending{}, events: make(chan agent.Event, 256), ended: make(chan struct{}), tools: o.Tools, purpose: o.Purpose}
	s.titler = newTitler(binary, o, s.currentModel)
	settings := map[string]json.RawMessage{}
	for key, value := range map[string]json.RawMessage{"approvalPolicy": response.Approval, "approvalsReviewer": response.Reviewer, "sandboxPolicy": response.Sandbox} {
		if len(value) > 0 && string(value) != "null" {
			settings[key] = value
		}
	}
	if o.Effort != "" {
		settings["effort"], _ = json.Marshal(o.Effort)
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
		// Carried provider settings describe the previous attempt. The
		// current launch permission owns the policy for the next turn.
		if policy := turnPolicies[o.Permission]; policy != nil {
			if s.overrides == nil {
				s.overrides = make(map[string]json.RawMessage)
			}
			maps.Copy(s.overrides, policy)
		}
		s.settings, _ = json.Marshal(s.overrides)
		if model := s.overrides["model"]; len(model) > 0 {
			_ = json.Unmarshal(model, &s.model)
		}
	}
	// A Codex outside the tested range may break in ways no error reports
	// (version.go): the conversation says so first, in one line.
	if notice, untested := versionNotice(initialized); untested {
		s.events <- notice
	}
	// The model Codex resolved for this thread, its own default included: the
	// picker marks it from the start instead of none (a default has no id).
	if ev, ok := readyEvent(s.model); ok {
		s.events <- ev
	}
	if mode := reportedPermission(response.Approval, response.Sandbox); mode != "" {
		s.events <- agent.Event{Type: "agent_permission", Text: mode}
	}
	go s.read()
	// The models this subscription can switch to, for /model and the picker.
	go s.reportModels()
	return s, nil
}

// durable is everything this adapter needs to resume supervising a live
// app-server. Pending server requests are not stored: the host redelivers them.
type durable struct {
	Host      string                     `json:"host"`
	Turn      string                     `json:"turn,omitempty"`
	TurnModel string                     `json:"turnModel,omitempty"`
	Model     string                     `json:"model,omitempty"`
	Settings  json.RawMessage            `json:"settings,omitempty"`
	Overrides map[string]json.RawMessage `json:"overrides,omitempty"`
}

// runTool runs an approved dynamic tool call and answers the app-server. An
// empty request is a call that needed no approval, so nothing is resolved.
func (s *session) runTool(native json.RawMessage, request, tool string, args json.RawMessage) {
	if s.tools == nil {
		s.replyTool(native, request, "managed commands unavailable", false)
		return
	}
	text, err := s.tools(s.ctx, tool, args)
	if err != nil {
		s.replyTool(native, request, err.Error(), false)
		return
	}
	if agent.BrowserTool(tool) {
		s.replyItems(native, request, browserContent(text), true)
		return
	}
	s.replyTool(native, request, text, true)
}

func (s *session) replyTool(native json.RawMessage, request, text string, success bool) {
	s.replyItems(native, request, []any{map[string]string{"type": "inputText", "text": text}}, success)
}

func (s *session) replyItems(native json.RawMessage, request string, items []any, success bool) {
	err := s.p.Reply(native, map[string]any{"success": success, "contentItems": items})
	switch {
	case err != nil:
		s.emit(agent.Event{Type: "failure", Status: "tool_reply_failed", Text: err.Error()})
	case request != "":
		s.emit(agent.Event{Type: "request_resolved", RequestID: request, Status: "response_written"})
	}
}

const toolOutcomeUnknown = "The Runner restarted while this lazybox_command was running; its outcome is unknown. Inspect the workspace and command list before retrying."

func attach(ctx context.Context, o agent.Open, binary string) (agent.Session, error) {
	var state durable
	if err := json.Unmarshal(o.Host.Attach.State, &state); err != nil || state.Host == "" {
		return nil, errors.New("Codex session has no re-attachable host")
	}
	life, cancel := context.WithCancel(ctx)
	p, err := jsonl.AttachOn(life, o.Host.Backend, o.Host.Root, state.Host, o.Host.Attach.Cursor)
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
	s := &session{p: p, cwd: o.Cwd, ref: o.Host.Attach.Reference, turn: state.Turn, turnModel: state.TurnModel, model: state.Model, workflow: o.Workflow, permission: o.Permission, settings: state.Settings, overrides: state.Overrides, ctx: life, cancel: cancel, requests: map[string]pending{}, events: make(chan agent.Event, 256), ended: make(chan struct{}), tools: o.Tools, purpose: o.Purpose, responded: o.Host.Attach.Responded}
	s.titler = newTitler(binary, o, s.currentModel)
	go s.read()
	return s, nil
}

func (s *session) DurableState() json.RawMessage {
	if s.p.HostID() == "" {
		return nil
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	data, _ := json.Marshal(durable{Host: s.p.HostID(), Turn: s.turn, TurnModel: s.turnModel, Model: s.model, Settings: s.settings, Overrides: s.overrides})
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

func reportedPermission(approval, sandbox json.RawMessage) string {
	var policy string
	var box struct {
		Type string `json:"type"`
	}
	if json.Unmarshal(approval, &policy) != nil || json.Unmarshal(sandbox, &box) != nil || box.Type == "" || policy == "" {
		return ""
	}
	if box.Type == "dangerFullAccess" && policy == "never" {
		return "YOLO"
	}
	return box.Type + " · " + policy
}

// turnPolicies are the turn/start overrides for each permission mode, matching
// the thread-level policies threadParams uses at start.
var turnPolicies = map[string]map[string]json.RawMessage{
	"yolo":      {"approvalPolicy": json.RawMessage(`"never"`), "sandboxPolicy": json.RawMessage(`{"type":"dangerFullAccess"}`), "approvalsReviewer": json.RawMessage(`"user"`)},
	"ask":       {"approvalPolicy": json.RawMessage(`"untrusted"`), "sandboxPolicy": json.RawMessage(`{"type":"workspaceWrite"}`), "approvalsReviewer": json.RawMessage(`"user"`)},
	"read-only": {"approvalPolicy": json.RawMessage(`"never"`), "sandboxPolicy": json.RawMessage(`{"type":"readOnly"}`), "approvalsReviewer": json.RawMessage(`"user"`)},
}

// Verify the cutoff and cwd before admitting the child. An older server could
// accept a fork request while silently ignoring fields it does not recognize.
func verifyFork(ctx context.Context, p *jsonl.Process, thread, turn, expectedCwd, cwd string) error {
	if cwd != expectedCwd {
		expected, wantErr := os.Stat(expectedCwd)
		actual, gotErr := os.Stat(cwd)
		if wantErr != nil || gotErr != nil || !os.SameFile(expected, actual) {
			return errors.New("Codex fork did not open in the target workspace")
		}
	}
	result, err := p.Call(ctx, "thread/turns/list", map[string]any{
		"threadId": thread, "limit": 1, "sortDirection": "desc", "itemsView": "notLoaded",
	})
	if err != nil {
		return fmt.Errorf("verify Codex fork boundary: %w", err)
	}
	var page struct {
		Data []struct {
			ID string `json:"id"`
		} `json:"data"`
	}
	if err := json.Unmarshal(result, &page); err != nil || len(page.Data) != 1 || page.Data[0].ID != turn {
		return errors.New("Codex fork did not end at the selected turn")
	}
	return nil
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
	if o.Effort != "" {
		params["config"] = map[string]any{"model_reasoning_effort": o.Effort}
	}
	return params
}

func (s *session) SessionSettings() json.RawMessage {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append(json.RawMessage(nil), s.settings...)
}
func (s *session) Reference() string { s.mu.Lock(); defer s.mu.Unlock(); return s.ref }

// currentModel is the model of the conversation's next turn.
func (s *session) currentModel() string { s.mu.Lock(); defer s.mu.Unlock(); return s.model }
func (s *session) Capabilities() agent.Capabilities {
	return agent.Capabilities{Images: true, Files: true, Questions: s.workflow == "plan", Approvals: true, Steer: true, Resume: true, Subagents: true, LivePermission: true, LiveModel: true, ProfileIsolation: "environment; cross-account resume unverified"}
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
	// turn/start has no out-of-band context field, so an instruction update is
	// its own input item ahead of the user's text.
	if op.Context != "" {
		items = append(items, map[string]any{"type": "text", "text": op.Context})
	}
	// The person's text, then each attachment in order. turn/start takes text
	// and images: a file's text is a fenced text item, any other file a text
	// item naming the retained path, which Codex reads in its sandbox.
	for _, part := range agent.Parts(op) {
		switch part.Kind {
		case agent.PartText:
			items = append(items, map[string]any{"type": "text", "text": part.Text})
		case agent.PartImage:
			items = append(items, map[string]any{"type": "image", "url": "data:" + part.Image.MediaType + ";base64," + part.Image.Data})
		case agent.PartFileText:
			items = append(items, map[string]any{"type": "text", "text": agent.Inline(*part.File, part.Text)})
		case agent.PartFileReference:
			items = append(items, map[string]any{"type": "text", "text": agent.Reference(*part.File)})
		}
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
			s.mu.Lock()
			for key, value := range s.overrides {
				params[key] = value
			}
			if s.workflow == "plan" {
				plan := map[string]any{"model": s.model}
				var settings map[string]json.RawMessage
				_ = json.Unmarshal(s.settings, &settings)
				if effort := settings["effort"]; len(effort) > 0 {
					plan["reasoning_effort"] = effort
				}
				params["collaborationMode"] = map[string]any{"mode": "plan", "settings": plan}
			}
			s.mu.Unlock()
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
			if s.turn != response.Turn.ID {
				s.turnModel = s.model
			}
			s.turn = response.Turn.ID
			s.mu.Unlock()
			r.ProviderID = response.Turn.ID
			approval, _ := params["approvalPolicy"].(json.RawMessage)
			sandbox, _ := params["sandboxPolicy"].(json.RawMessage)
			if mode := reportedPermission(approval, sandbox); mode != "" {
				s.emit(agent.Event{Type: "agent_permission", Text: mode})
			}
		}
		return r, nil
	case agent.Permission:
		policy, ok := turnPolicies[op.Permission]
		if !ok {
			return r, errors.New("permission must be ask, read-only or yolo")
		}
		// turn/start applies these "to this turn and subsequent turns"; keeping
		// them in the overrides also carries them across a Runner restart.
		s.mu.Lock()
		if s.overrides == nil {
			s.overrides = map[string]json.RawMessage{}
		}
		maps.Copy(s.overrides, policy)
		s.permission = op.Permission
		s.mu.Unlock()
		r.Delivery = "accepted"
		return r, nil
	case agent.ListProviderCatalog:
		ctx, cancel := context.WithTimeout(ctx, 8*time.Second)
		defer cancel()
		result, err := listProviderCatalog(ctx, s.p, ref, s.cwd, op.Section)
		if err != nil {
			return r, err
		}
		s.emit(agent.Event{Type: "provider_catalog", Text: result, OperationID: op.ID})
		return r, nil
	case agent.CompactContext:
		_, err := s.p.Call(ctx, "thread/compact/start", map[string]string{"threadId": ref})
		return r, err
	case agent.ListMcpServers:
		ctx, cancel := context.WithTimeout(ctx, 8*time.Second)
		defer cancel()
		text, err := listMcpServers(ctx, s.p, ref)
		if err != nil {
			return r, err
		}
		s.emit(agent.Event{Type: "mcp_servers", Text: text, OperationID: op.ID})
		return r, nil
	case agent.Model:
		model, err := json.Marshal(op.Model)
		if err != nil || op.Model == "" {
			return r, errors.New("model required")
		}
		// turn/start takes the model "for this turn and subsequent turns"; the
		// overrides carry it to the next one and across a Runner restart.
		s.mu.Lock()
		if s.overrides == nil {
			s.overrides = map[string]json.RawMessage{}
		}
		s.overrides["model"] = model
		if op.Effort != "" {
			s.overrides["effort"], _ = json.Marshal(op.Effort)
		}
		s.model = op.Model
		// A same-provider handoff carries the model now in force.
		var settings map[string]json.RawMessage
		if json.Unmarshal(s.settings, &settings) == nil && settings != nil {
			settings["model"] = model
			if op.Effort != "" {
				settings["effort"] = s.overrides["effort"]
			}
			s.settings, _ = json.Marshal(settings)
		}
		s.mu.Unlock()
		r.Delivery = "accepted"
		return r, nil
	case agent.Answer, agent.Approve:
		s.mu.Lock()
		p, ok := s.requests[op.RequestID]
		if !ok || p.answered {
			s.mu.Unlock()
			return r, errors.New("unknown or already answered native request")
		}
		p.answered = true
		s.requests[op.RequestID] = p
		s.mu.Unlock()
		if p.tool != "" {
			if op.Decision == "accept" {
				go s.runTool(p.message.ID, op.RequestID, p.tool, p.args)
			} else {
				declined := toolDeclined(p.tool)
				if op.Reason != "" {
					declined += " The user said: " + op.Reason
				}
				go s.replyTool(p.message.ID, op.RequestID, declined, false)
			}
			r.Delivery = "submitted"
			return r, nil
		}
		response, err := nativeResponse(p.message.Method, p.message.Params, op)
		if err != nil {
			s.mu.Lock()
			p.answered = false
			s.requests[op.RequestID] = p
			s.mu.Unlock()
			return r, err
		}
		if err = s.p.Reply(p.message.ID, response); err != nil {
			return r, err
		}
		r.Delivery = "submitted"
		if op.Kind == agent.Approve && op.Decision == "decline" && op.Reason != "" {
			// The native decline has no text: the reason reaches the agent in
			// the same turn, as the user's own words.
			var scope struct {
				ThreadID string `json:"threadId"`
				TurnID   string `json:"turnId"`
			}
			_ = json.Unmarshal(p.message.Params, &scope)
			steer := map[string]any{"threadId": scope.ThreadID, "expectedTurnId": scope.TurnID, "input": []any{map[string]any{"type": "text", "text": declineNote(p.message.Method, p.message.Params, op.Reason)}}}
			if _, err = s.p.Call(ctx, "turn/steer", steer); err != nil {
				r.Error = "declined; the reason could not be delivered: " + err.Error()
			}
		}
		return r, nil
	default:
		return r, agent.ErrUnsupported
	}
}

// nativeResponse is the app-server reply to a native approval or question.
func nativeResponse(method string, params json.RawMessage, op agent.Operation) (any, error) {
	if op.Kind == agent.Answer {
		answers := map[string]any{}
		for k, v := range op.Answers {
			answers[k] = map[string]any{"answers": v}
		}
		return map[string]any{"answers": answers}, nil
	}
	if method == permissionsMethod {
		// PermissionsRequestApprovalResponse: the access granted, for this
		// turn or (always) the rest of the conversation. A decline grants
		// nothing. Accepting grants exactly what was asked, never more.
		if op.Decision != "accept" {
			return map[string]any{"permissions": map[string]any{}, "scope": "turn"}, nil
		}
		granted, _ := requestedAccess(params)
		if granted == nil {
			return nil, fmt.Errorf("%w: this request asks for no access Lazybox can grant", agent.ErrUnsupported)
		}
		scope := "turn"
		if op.Always != "" {
			if op.Always != "session" {
				return nil, fmt.Errorf("%w: this request does not offer %q", agent.ErrUnsupported, op.Always)
			}
			scope = "session"
		}
		return map[string]any{"permissions": granted, "scope": scope}, nil
	}
	decision, err := approvalDecision(method, params, op)
	if err != nil {
		return nil, err
	}
	return map[string]any{"decision": decision}, nil
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
	// noted: methods whose unreadable params were already recorded once.
	noted := map[string]bool{}
	// The main thread's context in use, from its latest token usage, and the
	// turns whose compaction was already said (compaction.go).
	compactions := compactionState{}
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
		// fields reads what one method promises into v. The app-server has
		// many notifications this adapter ignores, and some reuse a field
		// name with another type (account/login/completed's error is a
		// string): params are read per method, and a line whose fields are
		// not what its method promises is skipped, never the end of
		// supervision. Its first occurrence per method is recorded.
		fields := func(v any) bool {
			err := json.Unmarshal(m.Params, v)
			if err == nil {
				return true
			}
			if !noted[m.Method] {
				noted[m.Method] = true
				emit(agent.Event{Type: agent.UnreadableEvent, Text: "Skipped a Codex " + m.Method + " message Lazybox could not read: " + err.Error()})
			}
			return false
		}
		func() {
			var p struct {
				ThreadID string `json:"threadId"`
				TurnID   string `json:"turnId"`
			}
			if !fields(&p) {
				if len(m.ID) > 0 {
					// The app-server waits for an answer to a request.
					_ = s.p.Reject(m.ID, "unreadable request params")
				}
				return
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
				// call is a dynamic tool call's own fields; empty for other requests.
				var call struct {
					Tool      string          `json:"tool"`
					Arguments json.RawMessage `json:"arguments"`
				}
				switch m.Method {
				case "item/commandExecution/requestApproval", "item/fileChange/requestApproval", permissionsMethod:
					ev.Type = "approval"
					ev.Text = m.Method
					ev.Data = withAlways(m.Method, m.Params)
				case "item/tool/requestUserInput", "tool/requestUserInput":
					ev.Type = "question"
				case "item/tool/call":
					if !fields(&call) {
						_ = s.p.Reject(m.ID, "unreadable dynamic tool call")
						return
					}
					if !toolOffered(s.purpose, call.Tool) {
						_ = s.p.Reject(m.ID, "unknown dynamic tool")
						return
					}
					if agent.IsInteractiveCommand(call.Tool, call.Arguments) {
						if m.Redelivered {
							_ = s.p.Reject(m.ID, toolOutcomeUnknown)
							return
						}
						// The Runner presents the command in its own handoff card.
						go s.runTool(m.ID, "", call.Tool, call.Arguments)
						return
					}
					title := toolApproval(call.Tool, call.Arguments)
					s.mu.Lock()
					autoRun := call.Tool == agent.CommandTool && s.permission == "yolo" && s.workflow != "plan" && !agent.IsAssistant(s.purpose)
					s.mu.Unlock()
					switch {
					case title == "":
						// Only reads: there is nothing to approve.
						go s.runTool(m.ID, "", call.Tool, call.Arguments)
						return
					case !answered && autoRun:
						if m.Redelivered && !agent.ToolReplaySafe(call.Tool, call.Arguments) {
							s.replyTool(m.ID, id, toolOutcomeUnknown, false)
						} else {
							go s.runTool(m.ID, "", call.Tool, call.Arguments)
						}
						return
					case !answered:
					case response.Decision != "" && response.Decision != "accept":
						// Declined: nothing ran, so the decline is simply repeated.
						go s.replyTool(m.ID, id, toolDeclined(call.Tool), false)
						return
					case agent.ToolReplaySafe(call.Tool, call.Arguments):
						// Running it again cannot repeat an effect: run it again.
						go s.runTool(m.ID, id, call.Tool, call.Arguments)
						return
					default:
						// Approved (or recorded without its decision): the command
						// may already have run.
						_ = s.p.Reply(m.ID, map[string]any{"success": false, "contentItems": []any{map[string]string{"type": "inputText", "text": toolOutcomeUnknown}}})
						emit(agent.Event{Type: "request_resolved", RequestID: id, Status: "outcome_unknown_after_restart"})
						return
					}
					ev.Type = "approval"
					ev.Text = title
				default:
					_ = s.p.Reject(m.ID, "unsupported server request")
					emit(agent.Event{Type: "unsupported_request", Text: m.Method, Data: m.Params})
					return
				}
				if answered && call.Tool == "" && (response.Kind == agent.Answer || response.Decision != "") {
					// The app-server acts only once it has the reply, so the
					// user's recorded response is given again rather than asked anew.
					if reply, err := nativeResponse(m.Method, m.Params, response); err == nil && s.p.Reply(m.ID, reply) == nil {
						return
					}
				}
				s.mu.Lock()
				s.requests[id] = pending{message: m, tool: call.Tool, args: call.Arguments}
				s.mu.Unlock()
				emit(ev)
				return
			}
			switch m.Method {
			case "turn/started":
				var turn struct {
					Turn struct {
						ID string `json:"id"`
					} `json:"turn"`
				}
				if !fields(&turn) {
					return
				}
				ev.Type = "turn_started"
				ev.TurnID = turn.Turn.ID
				s.mu.Lock()
				if ev.AgentID == "root" {
					if s.turn != turn.Turn.ID {
						s.turnModel = s.model
					}
					s.turn = turn.Turn.ID
				}
				s.mu.Unlock()
			case "turn/completed":
				var turn struct {
					Turn struct {
						ID     string          `json:"id"`
						Status string          `json:"status"`
						Error  json.RawMessage `json:"error"`
					} `json:"turn"`
				}
				if !fields(&turn) {
					return
				}
				ev.Type = "turn_ended"
				ev.TurnID = turn.Turn.ID
				ev.Status = turn.Turn.Status
				ev.Data = turn.Turn.Error
				if turn.Turn.Status == "failed" && len(turn.Turn.Error) > 0 && string(turn.Turn.Error) != "null" {
					failure := ev
					failure.Type = "failure"
					failure.Account = errorObservation(turn.Turn.Error)
					failure.Status = failure.Account.State
					emit(failure)
				}
				var over []json.RawMessage
				s.mu.Lock()
				if ev.AgentID == "root" {
					s.turn = ""
				}
				for id, request := range s.requests {
					var scope struct{ ThreadID string }
					_ = json.Unmarshal(request.message.Params, &scope)
					if scope.ThreadID == p.ThreadID {
						delete(s.requests, id)
						over = append(over, request.message.ID)
					}
				}
				s.mu.Unlock()
				// The turn ended without these being answered: the app-server
				// no longer waits on them, so the host must not redeliver them
				// to a later Runner as if it did.
				for _, id := range over {
					_ = s.p.Resolve(id)
				}
			case "item/reasoning/summaryTextDelta", "item/reasoning/textDelta":
				ev.Type, ev.Status = "activity", "thinking"
				ev.Data = nil // Activity only; never expose private reasoning text.
			case "item/agentMessage/delta":
				var delta struct {
					Delta  string `json:"delta"`
					ItemID string `json:"itemId"`
				}
				if !fields(&delta) {
					return
				}
				ev.Type = "message_delta"
				ev.Text = delta.Delta
				if ev.AgentID != "root" && delta.ItemID != "" {
					ev.Data, _ = json.Marshal(map[string]string{"itemId": delta.ItemID})
				}
			case "serverRequest/resolved":
				var resolved struct {
					RequestID json.RawMessage `json:"requestId"`
				}
				if !fields(&resolved) {
					return
				}
				ev.Type = "request_resolved"
				ev.RequestID = requestID(resolved.RequestID)
				s.mu.Lock()
				native := resolved.RequestID
				if request, ok := s.requests[ev.RequestID]; ok {
					native = request.message.ID
				}
				delete(s.requests, ev.RequestID)
				s.mu.Unlock()
				// The app-server resolved it itself (another client answered,
				// or it was cancelled): the host forgets it, so a later Runner
				// is not shown an approval nobody waits on.
				_ = s.p.Resolve(native)
			case "account/rateLimits/updated":
				ev.Type = "capacity_observation"
				ev.Account = rateLimits(m.Params)
				ev.Status = ev.Account.State
			case "thread/tokenUsage/updated":
				ev.Type = "usage"
				ev.Data = m.Params
				if ev.AgentID == "root" {
					compactions.usage(m.Params)
					// Codex's usage names no model: the one in force does, so the
					// session reports what it runs on (metering's LatestModel).
					s.mu.Lock()
					model := s.turnModel
					if ev.TurnID == "" || ev.TurnID != s.turn {
						model = ""
					}
					s.mu.Unlock()
					ev.Data = withModel(m.Params, model)
				}
			case "thread/compacted":
				// Deprecated for the contextCompaction item; said once either way.
				if ev.AgentID != "root" || !compactions.first(p.TurnID) {
					return
				}
				ev.Type, ev.Data = "compacted", compactions.data()
			case "error":
				var failure struct {
					WillRetry bool `json:"willRetry"`
					Error     struct {
						Code json.RawMessage `json:"codexErrorInfo"`
					} `json:"error"`
				}
				if !fields(&failure) {
					return
				}
				ev.Type = "failure"
				ev.Status = "unknown"
				detail, _ := json.Marshal(failure.Error)
				ev.Account = errorObservation(detail)
				ev.Status = ev.Account.State
				if failure.WillRetry {
					ev.Type = "provider_retry"
				}
				ev.Data = m.Params
			case "thread/status/changed":
				ev.Type, ev.Status = "agent_status", threadStatus(m.Params)
			case "thread/started":
				return
			case "turn/plan/updated":
				// The agent's plan, as a tool call of its turn: each update
				// replaces the one checklist the conversation shows.
				ev.Type = "tool_activity"
				if ev.Data = planToolCall(m.Params); len(ev.Data) == 0 {
					return
				}
			case "item/started", "item/completed":
				for _, activity := range collaborationEvents(m.Params, root) {
					emit(activity)
				}
				if compactionItem(m.Params) {
					// The context compaction Claude's conversations show too:
					// a phase while it runs, then one line.
					if m.Method == "item/started" {
						activity(agent.Event{Type: "activity", AgentID: ev.AgentID, Status: "compacting"})
						return
					}
					if ev.AgentID != "root" || !compactions.first(p.TurnID) {
						return
					}
					ev.Type, ev.Data = "compacted", compactions.data()
					break
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
					return
				}
			default:
				return
			}
			if ev.Type == "activity" {
				activity(ev)
				return
			}
			if ev.Type == "message_delta" {
				phases[ev.AgentID] = "responding/"
			}
			if ev.Type == "turn_ended" {
				delete(phases, ev.AgentID)
			}
			emit(ev)
			return
		}()
		if !flush() {
			s.end(s.ctx.Err())
			return
		}
	}
}

// end records why read stopped and ends Receive once the events read sent
// before it are taken. The events channel is never closed: tool replies
// still running may send to it, and give up on the cancelled context.
func (s *session) end(why error) {
	if why == nil {
		why = errors.New("Codex connection closed")
	}
	s.why = why
	close(s.ended)
}

func (s *session) Receive(ctx context.Context) (agent.Event, error) {
	select {
	case e := <-s.events:
		return e, nil
	case <-ctx.Done():
		return agent.Event{}, ctx.Err()
	case <-s.ended:
		// What read sent before it stopped (its failure, say) comes first.
		select {
		case e := <-s.events:
			return e, nil
		default:
		}
		return agent.Event{}, fmt.Errorf("Codex app-server stream ended: %w", s.why)
	}
}
func (s *session) Close() error { err := s.p.Close(); s.cancel(); return err }

// HostID is the session host running the app-server, or "" for a local pipe.
func (s *session) HostID() string { return s.p.HostID() }
