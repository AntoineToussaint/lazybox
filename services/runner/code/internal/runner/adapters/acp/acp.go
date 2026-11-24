package acp

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/version"
)

// ProtocolVersion is the ACP major version this client speaks.
const ProtocolVersion = 1

// dialect is how an ACP agent's stdio is spoken: JSON-RPC 2.0, and the reply
// to a session/prompt read in order with the agent's updates.
var dialect = jsonl.Options{Version: "2.0", Unmatched: true}

// Adapter starts one ACP agent. Binary is the program found for Agent.
type Adapter struct {
	Agent  Agent
	Binary string
}

// durable is what a later Runner needs to supervise a live agent again.
// Pending permission requests are not stored: the host redelivers them.
type durable struct {
	Host string `json:"host"`
	// Prompt is the id of the session/prompt request the running turn waits
	// on, and Turn the Lazybox turn it is; both empty between turns.
	Prompt json.RawMessage `json:"prompt,omitempty"`
	Turn   string          `json:"turn,omitempty"`
	Turns  int             `json:"turns,omitempty"`
	// Next is the id the next session/prompt is written with, reserved and
	// recorded before it is written: the attachment the engine saves before
	// each delivery holds it.
	Next json.RawMessage `json:"next,omitempty"`
	// Orphans are ids a previous Runner reserved and may have written a
	// prompt with before it died, without recording it running (attach moves
	// an unrecorded Next here). New prompts never reuse them; a reply to one
	// still ends the turn it started (answered), where it used to be ignored
	// and the conversation stayed busy for good. Bounded: maxOrphans.
	Orphans []json.RawMessage `json:"orphans,omitempty"`
	// Briefed: the conversation's context reached the agent (in its system
	// prompt, or with the first message).
	Briefed    bool            `json:"briefed,omitempty"`
	Permission string          `json:"permission,omitempty"`
	Workflow   string          `json:"workflow,omitempty"`
	Agent      agentFacts      `json:"agent"`
	Modes      *modeState      `json:"modes,omitempty"`
	Config     json.RawMessage `json:"config,omitempty"`
	Models     *modelState     `json:"models,omitempty"`
	Model      string          `json:"model,omitempty"`
	// Readable are files outside the workspace the agent was given (message
	// attachments, kept by the Runner) and may read.
	Readable []string `json:"readable,omitempty"`
	// Notes are the person's words on declined requests, delivered with the
	// next message: ACP's rejection carries no text.
	Notes []string `json:"notes,omitempty"`
	// Title is the session's title as the agent last reported it.
	Title string `json:"title,omitempty"`
}

// maxOrphans bounds durable.Orphans: each is a prompt a Runner may have
// written just before it died, and a conversation idle across many restarts
// leaves one per restart that was never written.
const maxOrphans = 8

// errNoTitle: the agent has not named the conversation yet; a later turn
// may (session_info_update).
var errNoTitle = errors.New("the agent has not named this conversation yet")

// Title is the conversation's title as the agent named it (agent.Titler).
// ACP has no request for one: an agent names its session when it chooses to.
func (s *session) Title(context.Context, string) (string, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.state.Title == "" {
		return "", errNoTitle
	}
	return s.state.Title, nil
}

// agentFacts are what initialize said about the agent.
type agentFacts struct {
	Title    string `json:"title,omitempty"`
	Image    bool   `json:"image,omitempty"`
	Embedded bool   `json:"embedded,omitempty"`
	Load     bool   `json:"load,omitempty"`
	Resume   bool   `json:"resume,omitempty"`
	System   bool   `json:"system,omitempty"`
}

type modeState struct {
	Current   string `json:"currentModeId"`
	Available []mode `json:"availableModes"`
}
type mode struct {
	ID          string `json:"id"`
	Name        string `json:"name"`
	Description string `json:"description,omitempty"`
}

// modelState is the pre-standard `models` an agent may report with its
// session (session/set_model). Agents on the standard use config options.
type modelState struct {
	Current   string `json:"currentModelId"`
	Available []struct {
		ID          string `json:"modelId"`
		Name        string `json:"name"`
		Description string `json:"description,omitempty"`
	} `json:"availableModels"`
}

type pending struct {
	native json.RawMessage
	params permissionRequest
}

type session struct {
	mu      sync.Mutex
	p       *jsonl.Process
	ref     string
	title   string
	cwd     string
	purpose string
	state   durable
	// requests are the permission requests waiting on the person.
	requests map[string]pending
	// calls is every tool call of the session as reported so far, merged.
	calls map[string]*toolCall
	// replaying: a session/load is replaying history, which the journal
	// already holds; message and tool updates are not recorded again. It
	// ends at the load's reply, read in order with the replayed lines
	// (loading is its request id, loaded where the reply is handed over).
	replaying bool
	loading   json.RawMessage
	loaded    chan jsonl.Message
	events    chan agent.Event
	// ended is closed when read returns (the agent exited, the Runner
	// detached or closed): Receive then ends with why instead of waiting for
	// events nothing will send.
	ended     chan struct{}
	why       error
	ctx       context.Context
	cancel    context.CancelFunc
	tools     func(context.Context, string, json.RawMessage) (string, error)
	responded map[string]agent.Operation
	// terminals are the output limits terminal/create asked for, by command.
	terminals map[string]int
	// openContext is the conversation's context, given with the first message
	// to an agent that takes no system prompt.
	openContext string
}

func (a Adapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	if o.Host != nil && o.Host.Attach != nil {
		return attach(ctx, o)
	}
	binary := a.Binary
	if binary == "" {
		binary = a.Agent.Command
	}
	if binary == "" {
		return nil, errors.New("ACP agent has no command")
	}
	title := a.Agent.Title
	if title == "" {
		title = a.Agent.ID
	}
	env := append(append([]string(nil), o.Env...), a.Agent.LaunchEnv()...)
	life, cancel := context.WithCancel(ctx)
	var p *jsonl.Process
	var err error
	if o.Host != nil {
		var host *jsonl.Host
		if host, err = jsonl.LaunchOn(life, o.Host.Backend, o.Host.Root, o.HostOptions(title)); err == nil {
			p, err = host.StartWith(life, binary, a.Agent.Args, env, o.Cwd, dialect)
		}
	} else {
		p, err = jsonl.StartWith(life, binary, a.Agent.Args, env, o.Cwd, dialect)
	}
	if err != nil {
		cancel()
		return nil, err
	}
	s := newSession(p, life, cancel, o)
	s.title = title
	s.state.Agent.Title = title
	s.state.Permission, s.state.Workflow = o.Permission, o.Workflow
	go s.read()
	fail := func(err error) (agent.Session, error) { cancel(); p.Close(); return nil, err }
	setup, done := context.WithTimeout(life, 60*time.Second)
	defer done()
	result, err := p.Call(setup, "initialize", map[string]any{
		"protocolVersion": ProtocolVersion,
		"clientCapabilities": map[string]any{
			"fs":       map[string]bool{"readTextFile": true, "writeTextFile": true},
			"terminal": o.Tools != nil,
		},
		"clientInfo": map[string]string{"name": "lazybox", "title": "Lazybox", "version": version.Version},
	})
	if err != nil {
		return fail(startupError(title, "initialize", err))
	}
	var init struct {
		ProtocolVersion int `json:"protocolVersion"`
		Capabilities    struct {
			Load   bool `json:"loadSession"`
			Prompt struct {
				Image    bool `json:"image"`
				Embedded bool `json:"embeddedContext"`
			} `json:"promptCapabilities"`
			Session struct {
				Resume json.RawMessage `json:"resume"`
				System json.RawMessage `json:"systemPrompt"`
			} `json:"sessionCapabilities"`
		} `json:"agentCapabilities"`
		AuthMethods []authMethod `json:"authMethods"`
		Info        *struct {
			Title string `json:"title"`
			Name  string `json:"name"`
		} `json:"agentInfo"`
	}
	if err = json.Unmarshal(result, &init); err != nil {
		return fail(fmt.Errorf("%s answered initialize with something that is not ACP: %w", title, err))
	}
	if init.ProtocolVersion != ProtocolVersion {
		return fail(fmt.Errorf("%s speaks ACP version %d; Lazybox speaks version %d", title, init.ProtocolVersion, ProtocolVersion))
	}
	c := init.Capabilities
	s.mu.Lock()
	s.state.Agent = agentFacts{Title: title, Image: c.Prompt.Image, Embedded: c.Prompt.Embedded, Load: c.Load, Resume: present(c.Session.Resume), System: present(c.Session.System)}
	s.mu.Unlock()
	params := map[string]any{"cwd": o.Cwd, "mcpServers": []any{}}
	method := "session/new"
	if o.Resume != "" {
		params["sessionId"] = o.Resume
		switch {
		case present(c.Session.Resume):
			method = "session/resume"
		case c.Load:
			method = "session/load"
		default:
			return fail(fmt.Errorf("%w: %s cannot continue an earlier conversation", agent.ErrUnsupported, title))
		}
	}
	if present(c.Session.System) && o.Context != "" {
		params["systemPrompt"] = map[string]any{"mode": "append", "content": []any{textBlock(o.Context)}}
	}
	if method == "session/load" {
		result, err = s.load(setup, params)
	} else {
		result, err = p.Call(setup, method, params)
	}
	if err != nil {
		return fail(startupError(title, method, err, init.AuthMethods...))
	}
	var opened struct {
		SessionID string          `json:"sessionId"`
		Modes     *modeState      `json:"modes"`
		Config    json.RawMessage `json:"configOptions"`
		Models    *modelState     `json:"models"`
	}
	if err = json.Unmarshal(result, &opened); err != nil {
		return fail(fmt.Errorf("%s answered %s with something that is not ACP: %w", title, method, err))
	}
	ref := opened.SessionID
	if o.Resume != "" {
		ref = o.Resume
	}
	if ref == "" {
		return fail(fmt.Errorf("%s did not name its session", title))
	}
	s.mu.Lock()
	s.ref = ref
	s.state.Next = s.p.NewID()
	s.state.Briefed = o.Context == "" || params["systemPrompt"] != nil || o.Resume != ""
	s.state.Modes, s.state.Models = opened.Modes, opened.Models
	if present(opened.Config) {
		s.state.Config = opened.Config
	}
	s.mu.Unlock()
	// The person's choices: the permission mode and the model.
	if err = s.applyPermission(setup, o.Permission); err != nil {
		return fail(err)
	}
	if o.Model != "" {
		if err = s.setModel(setup, o.Model); err != nil && !errors.Is(err, agent.ErrUnsupported) {
			return fail(fmt.Errorf("%s could not switch to model %s: %w", title, o.Model, err))
		}
	}
	s.emitCatalog()
	return s, nil
}

// load sends session/load and waits for its reply as the read loop reads it:
// every update the agent replays before the reply is history the journal
// already holds, and only the read loop knows where the reply falls among
// them (a call's reply would be handed over ahead of the updates before it).
func (s *session) load(ctx context.Context, params map[string]any) (json.RawMessage, error) {
	id := s.p.NewID()
	loaded := make(chan jsonl.Message, 1)
	s.mu.Lock()
	s.replaying, s.loading, s.loaded = true, id, loaded
	s.mu.Unlock()
	if err := s.p.Request(id, "session/load", params); err != nil {
		return nil, err
	}
	select {
	case m := <-loaded:
		if present(m.Error) {
			return nil, jsonl.NewCallError("session/load", m.Error)
		}
		return m.Result, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

func newSession(p *jsonl.Process, life context.Context, cancel context.CancelFunc, o agent.Open) *session {
	return &session{p: p, cwd: o.Cwd, purpose: o.Purpose, openContext: o.Context, ctx: life, cancel: cancel, tools: o.Tools, requests: map[string]pending{}, calls: map[string]*toolCall{}, events: make(chan agent.Event, 256), ended: make(chan struct{}), terminals: map[string]int{}}
}

func attach(ctx context.Context, o agent.Open) (agent.Session, error) {
	var state durable
	if err := json.Unmarshal(o.Host.Attach.State, &state); err != nil || state.Host == "" {
		return nil, errors.New("ACP session has no re-attachable host")
	}
	life, cancel := context.WithCancel(ctx)
	p, err := jsonl.AttachWith(life, o.Host.Backend, o.Host.Root, state.Host, o.Host.Attach.Cursor, dialect)
	if err != nil {
		cancel()
		return nil, err
	}
	s := newSession(p, life, cancel, o)
	s.ref = o.Host.Attach.Reference
	s.state = state
	if s.state.Prompt == nil && s.state.Next != nil {
		// The previous Runner may have written a prompt with Next and died
		// before recording it: that prompt may be running. Its reply still
		// ends its turn, and the next prompt gets an id of its own.
		s.state.Orphans = append(s.state.Orphans, s.state.Next)
		if n := len(s.state.Orphans); n > maxOrphans {
			s.state.Orphans = s.state.Orphans[n-maxOrphans:]
		}
	}
	s.state.Next = p.NewID()
	s.title = state.Agent.Title
	s.responded = o.Host.Attach.Responded
	go s.read()
	return s, nil
}

func present(raw json.RawMessage) bool { return len(raw) > 0 && string(raw) != "null" }

func textBlock(text string) map[string]any { return map[string]any{"type": "text", "text": text} }

type authMethod struct {
	ID          string   `json:"id"`
	Name        string   `json:"name"`
	Description string   `json:"description"`
	Type        string   `json:"type"`
	Args        []string `json:"args"`
}

// authRequired is ACP's "Authentication required" error code.
const authRequired = -32000

// startupError says in plain words why an agent did not start: most often it
// needs the person to sign in once, the way that agent signs in.
func startupError(title, method string, err error, methods ...authMethod) error {
	var e *jsonl.CallError
	if !errors.As(err, &e) {
		return fmt.Errorf("%s did not start (%s): %w", title, method, err)
	}
	if e.Code == authRequired {
		how := []string{}
		for _, m := range methods {
			if m.Name != "" {
				how = append(how, m.Name)
			}
		}
		hint := "Sign in to " + title + " once in your own terminal, then start the conversation again."
		if len(how) > 0 {
			hint += " It signs in with: " + strings.Join(how, ", ") + "."
		}
		return fmt.Errorf("%s needs you to sign in. %s", title, hint)
	}
	if e.Reason() == "" {
		return fmt.Errorf("%s did not start: %w", title, e) // the error names the step itself
	}
	return fmt.Errorf("%s did not start (%s): %w", title, method, e)
}

func (s *session) DurableState() json.RawMessage {
	if s.p.HostID() == "" {
		return nil
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	state := s.state
	state.Host = s.p.HostID()
	data, _ := json.Marshal(state)
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

func (s *session) Reference() string { s.mu.Lock(); defer s.mu.Unlock(); return s.ref }

func (s *session) Capabilities() agent.Capabilities {
	s.mu.Lock()
	defer s.mu.Unlock()
	return agent.Capabilities{Images: s.state.Agent.Image, Files: true, Approvals: true, Resume: s.state.Agent.Load || s.state.Agent.Resume, LivePermission: true, LiveModel: s.modelChoices() != nil, ProfileIsolation: "inherited: the agent's own sign-in"}
}

// SessionSettings carries the permission mode and model now in force to a
// same-provider handoff.
func (s *session) SessionSettings() json.RawMessage {
	s.mu.Lock()
	defer s.mu.Unlock()
	data, _ := json.Marshal(map[string]string{"permission": s.state.Permission, "model": s.state.Model})
	return data
}

func (s *session) emit(ev agent.Event) bool {
	select {
	case s.events <- ev:
		return true
	case <-s.ctx.Done():
		return false
	}
}

// end records why read stopped and ends Receive once the events read sent
// before it are taken. The events channel is never closed: client calls
// still being served may send to it, and give up on the cancelled context.
func (s *session) end(why error) {
	if why == nil {
		why = errors.New("ACP connection closed")
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
		return agent.Event{}, fmt.Errorf("ACP agent stream ended: %w", s.why)
	}
}

func (s *session) Close() error { err := s.p.Close(); s.cancel(); return err }

// HostID is the session host running the agent, or "" for a local pipe.
func (s *session) HostID() string { return s.p.HostID() }

// prompt is a message as ACP content blocks: the conversation's context the
// first time (when the agent took no system prompt), an instruction update,
// declined-request notes, the text with its attached files, and the images.
func (s *session) prompt(op agent.Operation, context string) []any {
	blocks := []any{}
	if context != "" {
		blocks = append(blocks, textBlock(context))
	}
	if op.Context != "" {
		blocks = append(blocks, textBlock(op.Context))
	}
	for _, note := range s.state.Notes {
		blocks = append(blocks, textBlock(note))
	}
	// The person's text, then each attachment in order: an image block (the
	// Runner sends images natively only to an agent that takes them), a
	// file's text as an embedded resource where the agent takes embedded
	// context and a fenced text block where it does not, and any other file as
	// a resource link, which every ACP agent accepts.
	for _, part := range agent.Parts(op) {
		switch part.Kind {
		case agent.PartText:
			blocks = append(blocks, textBlock(part.Text))
		case agent.PartImage:
			if s.state.Agent.Image {
				blocks = append(blocks, map[string]any{"type": "image", "mimeType": part.Image.MediaType, "data": part.Image.Data})
			} else if part.File != nil {
				blocks = append(blocks, resourceLink(*part.File))
			}
		case agent.PartFileText:
			if s.state.Agent.Embedded {
				blocks = append(blocks, map[string]any{"type": "resource", "resource": map[string]any{"uri": agent.FileURI(part.File.Path), "mimeType": textMediaType(part.File.MediaType), "text": part.Text}})
			} else {
				blocks = append(blocks, textBlock(agent.Inline(*part.File, part.Text)))
			}
		case agent.PartFileReference:
			blocks = append(blocks, resourceLink(*part.File))
		}
	}
	return blocks
}

// resourceLink is an attached file the agent reads itself (the path is also
// readable through fs/read_text_file: Apply adds it to Readable).
func resourceLink(f agent.File) map[string]any {
	link := map[string]any{"type": "resource_link", "uri": agent.FileURI(f.Path), "name": f.Name, "size": f.Size}
	if f.MediaType != "" {
		link["mimeType"] = f.MediaType
	}
	return link
}

// textMediaType is a text resource's type: the file's own when it says text.
func textMediaType(mediaType string) string {
	if strings.HasPrefix(mediaType, "text/") || strings.HasSuffix(mediaType, "json") || strings.HasSuffix(mediaType, "xml") || strings.HasSuffix(mediaType, "yaml") {
		return mediaType
	}
	return "text/plain"
}

func (s *session) Apply(ctx context.Context, op agent.Operation) (agent.Receipt, error) {
	r := agent.Receipt{ID: op.ID, Delivery: "acknowledged"}
	switch op.Kind {
	case agent.Message:
		s.mu.Lock()
		if s.state.Prompt != nil {
			s.mu.Unlock()
			// Nothing sent: the message waits for the running turn's end.
			return r, agent.ErrTurnRunning
		}
		context := ""
		if !s.state.Briefed {
			context = s.openContext
		}
		blocks := s.prompt(op, context)
		if len(blocks) == 0 {
			s.mu.Unlock()
			return r, errors.New("empty message")
		}
		for _, f := range op.Files {
			s.state.Readable = appendUnique(s.state.Readable, f.Path)
		}
		s.state.Turns++
		turn := fmt.Sprintf("turn-%d", s.state.Turns)
		id := s.state.Next
		if id == nil {
			id = s.p.NewID()
		}
		s.state.Prompt, s.state.Turn, s.state.Next = id, turn, s.p.NewID()
		s.state.Briefed, s.state.Notes = true, nil
		ref := s.ref
		s.mu.Unlock()
		// The turn starts before the agent can answer anything about it.
		s.emit(agent.Event{Type: "turn_started", TurnID: turn, SessionID: ref, AgentID: "root"})
		if err := s.p.Request(id, "session/prompt", map[string]any{"sessionId": ref, "prompt": blocks}); err != nil {
			s.mu.Lock()
			s.state.Prompt, s.state.Turn = nil, ""
			s.mu.Unlock()
			s.emit(agent.Event{Type: "turn_ended", TurnID: turn, Status: "failed", AgentID: "root"})
			return r, err
		}
		r.Delivery, r.ProviderID = "submitted", turn
		return r, nil
	case agent.Interrupt:
		s.mu.Lock()
		ref := s.ref
		waiting := s.requests
		s.requests = map[string]pending{}
		s.mu.Unlock()
		// The agent stops its turn; a permission it is still waiting on is
		// answered "cancelled", as the protocol requires.
		for id, req := range waiting {
			_ = s.p.Reply(req.native, map[string]any{"outcome": map[string]string{"outcome": "cancelled"}})
			s.emit(agent.Event{Type: "request_resolved", RequestID: id, Status: "cancelled"})
		}
		if err := s.p.Notify("session/cancel", map[string]string{"sessionId": ref}); err != nil {
			return r, err
		}
		r.Delivery = "submitted"
		return r, nil
	case agent.Approve:
		return s.approve(op)
	case agent.Permission:
		if !agent.PermissionModes[op.Permission] {
			return r, errors.New("permission must be ask, read-only or yolo")
		}
		if err := s.applyPermission(ctx, op.Permission); err != nil {
			return r, err
		}
		if mode := s.reportedMode(); mode != "" {
			s.emit(agent.Event{Type: "agent_permission", Text: mode})
		}
		r.Delivery = "accepted"
		return r, nil
	case agent.Model:
		if err := agent.ValidModel(op.Model); err != nil {
			return r, err
		}
		if err := s.setModel(ctx, op.Model); err != nil {
			return r, err
		}
		s.emitCatalog()
		r.Delivery = "accepted"
		return r, nil
	default:
		return r, agent.ErrUnsupported
	}
}

func appendUnique(list []string, value string) []string {
	for _, v := range list {
		if v == value {
			return list
		}
	}
	return append(list, value)
}
