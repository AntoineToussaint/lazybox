package claude

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

type Adapter struct{ Node, Worker, Binary string }
type session struct {
	mu    sync.Mutex
	p     *jsonl.Process
	ref   string
	ctx   context.Context
	tools func(context.Context, string, json.RawMessage) (string, error)
	// features are the worker operations beyond the original protocol. A worker
	// keeps running across Runner restarts, so this is what the worker that is
	// actually running reported, not what this Runner's worker build supports.
	features []string
	// noted: an unreadable worker line was already recorded once.
	noted bool
}

// durable is the adapter state a later Runner needs to re-attach.
type durable struct {
	Host     string   `json:"host"`
	Features []string `json:"features,omitempty"`
}

func (s *session) has(feature string) bool { return slices.Contains(s.features, feature) }

// toolOutcomeUnknown answers a managed-command call that was in flight when
// the previous Runner stopped: it may or may not have run.
const toolOutcomeUnknown = "The Runner restarted while this lazybox_command call was in flight; its outcome is unknown. Inspect the workspace and command list before retrying."

func GatewayEnv(g *agent.Gateway, env []string) []string {
	out := append([]string{}, env...)
	if g == nil {
		return out
	}
	filtered := out[:0]
	for _, entry := range out {
		if !strings.HasPrefix(entry, "ANTHROPIC_BASE_URL=") {
			filtered = append(filtered, entry)
		}
	}
	out = append(filtered, "ANTHROPIC_BASE_URL="+strings.TrimRight(g.URL, "/"))
	if g.TokenEnv != "" {
		for _, entry := range env {
			if value, ok := strings.CutPrefix(entry, g.TokenEnv+"="); ok {
				filtered = out[:0]
				for _, v := range out {
					if !strings.HasPrefix(v, "ANTHROPIC_AUTH_TOKEN=") && !strings.HasPrefix(v, "ANTHROPIC_API_KEY=") {
						filtered = append(filtered, v)
					}
				}
				out = append(filtered, "ANTHROPIC_AUTH_TOKEN="+value)
				break
			}
		}
	}
	return out
}
func (a Adapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	node := a.Node
	if node == "" {
		node = "node"
	}
	if a.Worker == "" {
		return nil, errors.New("Claude SDK worker path required; run make build-worker")
	}
	if o.Host != nil && o.Host.Attach != nil {
		return attach(ctx, o)
	}
	var p *jsonl.Process
	var err error
	if o.Host != nil {
		// The agent talks to the host's session-stable proxy; the Runner points
		// the proxy at whichever gateway it currently runs.
		var h *jsonl.Host
		if h, err = jsonl.LaunchOn(ctx, o.Host.Backend, o.Host.Root, o.HostOptions("claude")); err != nil {
			return nil, err
		}
		env := GatewayEnv(o.Gateway, o.Env)
		if o.Gateway != nil {
			proxied := *o.Gateway
			proxied.URL = h.ProxyURL()
			env = GatewayEnv(&proxied, o.Env)
		}
		if p, err = h.Start(ctx, node, []string{a.Worker}, env, o.Cwd); err != nil {
			return nil, err
		}
		if o.Gateway != nil {
			if err = p.SetUpstream(o.Gateway.URL); err != nil {
				p.Close()
				return nil, err
			}
		}
	} else if p, err = jsonl.Start(ctx, node, []string{a.Worker}, GatewayEnv(o.Gateway, o.Env), o.Cwd); err != nil {
		return nil, err
	}
	setup, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	// The conversation's "always allow" choices so far: the new worker holds
	// them (a worker started before grants ignores the field).
	result, err := p.Call(setup, "open", map[string]any{"cwd": o.Cwd, "purpose": o.Purpose, "model": o.Model, "effort": o.Effort, "resume": o.Resume, "forkThread": o.ForkThread, "forkAfterTurn": o.ForkAfterTurn, "permission": o.Permission, "workflow": o.Workflow, "binary": a.Binary, "context": o.Context, "browser": o.Browser, "claudeAiConnectors": o.ClaudeAIConnectors, "grants": agent.GrantSpecs(o.Grants)})
	if err != nil {
		p.Close()
		return nil, err
	}
	var response struct {
		Reference string   `json:"reference"`
		Features  []string `json:"features"`
	}
	if err = json.Unmarshal(result, &response); err != nil {
		p.Close()
		return nil, err
	}
	if o.ForkThread != "" && (response.Reference == "" || response.Reference == o.ForkThread) {
		p.Close()
		return nil, errors.New("Claude worker did not create a separate conversation")
	}
	return &session{p: p, ref: response.Reference, ctx: ctx, tools: o.Tools, features: response.Features}, nil
}
func attach(ctx context.Context, o agent.Open) (agent.Session, error) {
	var state durable
	if err := json.Unmarshal(o.Host.Attach.State, &state); err != nil || state.Host == "" {
		return nil, errors.New("Claude session has no re-attachable host")
	}
	p, err := jsonl.AttachOn(ctx, o.Host.Backend, o.Host.Root, state.Host, o.Host.Attach.Cursor)
	if err != nil {
		return nil, err
	}
	if o.Gateway != nil {
		if err = p.SetUpstream(o.Gateway.URL); err != nil {
			_ = p.Detach()
			return nil, err
		}
	}
	return &session{p: p, ref: o.Host.Attach.Reference, ctx: ctx, tools: o.Tools, features: state.Features}, nil
}

func (s *session) DurableState() json.RawMessage {
	if s.p.HostID() == "" {
		return nil
	}
	data, _ := json.Marshal(durable{Host: s.p.HostID(), Features: s.features})
	return data
}

// Commit acknowledges a native line once its last event is recorded.
func (s *session) Commit(ev agent.Event) {
	if !ev.Partial {
		s.p.Commit(ev.Cursor)
	}
}

// unreadable skips a worker line the Runner cannot read. The first one of the
// session becomes a record (agent.UnreadableEvent) the engine journals and
// commits; later ones are committed here, so a worker that keeps sending them
// cannot fill the journal. Every line before it was already recorded: Receive
// is only called again once the previous event is.
func (s *session) unreadable(m jsonl.Message, err error) (agent.Event, bool) {
	s.mu.Lock()
	first := !s.noted
	s.noted = true
	s.mu.Unlock()
	if !first {
		s.p.Commit(m.Seq)
		return agent.Event{}, false
	}
	return agent.Event{Type: agent.UnreadableEvent, Text: fmt.Sprintf("Skipped a Claude worker %s line Lazybox could not read: %v", m.Method, err), Cursor: m.Seq}, true
}

func (s *session) Detach() error     { return s.p.Detach() }
func (s *session) Reference() string { s.mu.Lock(); defer s.mu.Unlock(); return s.ref }
func (s *session) Capabilities() agent.Capabilities {
	return agent.Capabilities{Images: true, Files: true, Questions: true, Approvals: true, Resume: true, Subagents: true, Steer: s.has("steer"), LivePermission: s.has("permission"), StopAgent: s.has("stop_agent"), LiveModel: s.has("model"), ProfileIsolation: "environment credentials; shared keychain isolation unverified"}
}
func (s *session) Apply(ctx context.Context, op agent.Operation) (agent.Receipt, error) {
	r := agent.Receipt{ID: op.ID}
	// Steer hands the message to the running turn: Claude Code folds it in
	// between two steps (the worker's steer.ts). A worker started before
	// steering existed cannot; the message then waits for the turn to end.
	if op.Kind == agent.Model && op.Effort != "" && !s.has("model_effort") {
		return agent.Receipt{}, errors.New("this Claude worker cannot change reasoning effort while running; resume the conversation with the current worker")
	}
	if op.Kind == agent.Steer && !s.has("steer") {
		return r, agent.ErrUnsupported
	}
	if op.Kind == agent.RevokeGrant {
		// Only a worker that reports its grants holds any to take back.
		if !s.has("grants") {
			return r, agent.ErrUnsupported
		}
		result, err := s.p.Call(ctx, "revoke", map[string]string{"id": op.GrantID})
		if err != nil {
			return r, err
		}
		err = json.Unmarshal(result, &r)
		r.ID = op.ID
		return r, err
	}
	if op.Kind == agent.StopAgent {
		// A worker started before single sub-agent stops cannot do one.
		if !s.has("stop_agent") || op.NativeID == "" {
			return r, agent.ErrUnsupported
		}
		result, err := s.p.Call(ctx, string(op.Kind), map[string]string{"nativeId": op.NativeID})
		if err != nil {
			return r, err
		}
		err = json.Unmarshal(result, &r)
		r.ID = op.ID
		return r, err
	}
	wire := message(op, s.has("parts"))
	if op.Context != "" && !s.has("context") {
		// A worker started before instruction updates existed ignores the
		// field; put the update ahead of the text so it still reaches the agent.
		if wire.Parts != nil {
			wire.Parts = append([]wirePart{{Type: "text", Text: op.Context}}, wire.Parts...)
		} else {
			wire.Text = op.Context + "\n\n" + wire.Text
		}
		wire.Context = ""
	}
	if err := fitsWorker(wire); err != nil {
		return r, err
	}
	result, err := s.p.Call(ctx, string(op.Kind), wire)
	if err != nil {
		return r, err
	}
	err = json.Unmarshal(result, &r)
	if err == nil && op.Kind == agent.Message && r.Delivery == agent.ModelRequiredFault {
		return r, agent.ErrModelRequired
	}
	r.ID = op.ID
	return r, err
}

// WorkerLineBytes is the longest line the Claude worker reads (LINE_LIMIT in
// workers/claude-sdk/src/wire.ts; the two are kept equal). Every message the
// Runner accepts (attachments.MaxPayloadBytes, base64 included) fits with
// room for the text around it; a message that would not, such as text that
// JSON escaping grows past it, is refused here with the reason instead of
// being written.
const WorkerLineBytes = 64 << 20

// lineEnvelope is room for the request around a message's params.
const lineEnvelope = 1 << 10

func fitsWorker(wire wireMessage) error {
	data, err := json.Marshal(wire)
	if err != nil {
		return err
	}
	if len(data)+lineEnvelope > WorkerLineBytes {
		return fmt.Errorf("the message is too large for Claude: %d MiB encoded, at most %d MiB", (len(data)+(1<<20)-1)>>20, WorkerLineBytes>>20)
	}
	return nil
}

// Title names the conversation with Claude Code's own session-title
// generator (the one its /rename and session list use: a small, tool-free
// request on the conversation's subscription). A worker started before it
// existed cannot.
func (s *session) Title(ctx context.Context, description string) (string, error) {
	if !s.has("title") {
		return "", agent.ErrUnsupported
	}
	result, err := s.p.Call(ctx, "title", map[string]string{"description": description})
	if err != nil {
		return "", err
	}
	var response struct {
		Title string `json:"title"`
	}
	err = json.Unmarshal(result, &response)
	return response.Title, err
}
func (s *session) Receive(ctx context.Context) (agent.Event, error) {
	for {
		m, err := s.p.Receive(ctx)
		if err != nil {
			return agent.Event{}, err
		}
		if m.Method == "tool" && len(m.ID) > 0 {
			var p struct {
				Name      string          `json:"name"`
				Arguments json.RawMessage `json:"arguments"`
			}
			if err = json.Unmarshal(m.Params, &p); err != nil {
				// The worker waits for an answer: say no, and go on.
				_ = s.p.Reject(m.ID, "unreadable tool call")
				if ev, ok := s.unreadable(m, err); ok {
					return ev, nil
				}
				continue
			}
			if m.Redelivered && !agent.ToolReplaySafe(p.Name, p.Arguments) {
				// Never re-run a call whose first execution may have had effects;
				// say so, to the agent and in the conversation.
				_ = s.p.Reject(m.ID, toolOutcomeUnknown)
				return agent.Event{Type: "request_resolved", RequestID: "tool:" + string(m.ID), Name: p.Name, Status: "outcome_unknown_after_restart", Cursor: m.Seq}, nil
			}
			go func() {
				if s.tools == nil {
					_ = s.p.Reject(m.ID, "managed commands unavailable")
					return
				}
				result, err := s.tools(s.ctx, p.Name, p.Arguments)
				if err != nil {
					_ = s.p.Reject(m.ID, err.Error())
				} else {
					_ = s.p.Reply(m.ID, result)
				}
			}()
			continue
		}
		if m.Method != "event" {
			continue
		}
		var ev agent.Event
		if err = json.Unmarshal(m.Params, &ev); err != nil {
			// One line the Runner cannot read must not end supervision of a
			// worker that keeps running: skip it.
			if ev, ok := s.unreadable(m, err); ok {
				return ev, nil
			}
			continue
		}
		// One event per worker line: the line completes with it, whatever the
		// worker's JSON said.
		ev.Cursor, ev.Partial = m.Seq, false
		if ev.SessionID != "" && (ev.AgentID == "" || ev.AgentID == "root") {
			s.mu.Lock()
			s.ref = ev.SessionID
			s.mu.Unlock()
		}
		return ev, nil
	}
}
func (s *session) Close() error { return s.p.Close() }

// HostID is the session host running the worker, or "" for a local pipe.
func (s *session) HostID() string { return s.p.HostID() }

// wireMessage is a message as the worker takes it. Parts is its content in
// order (the worker makes one content block of each); a worker started
// before parts existed reads Text and Images instead.
type wireMessage struct {
	agent.Operation
	Parts []wirePart `json:"parts,omitempty"`
}

// wirePart is one Claude content block: text, or a base64 image.
type wirePart struct {
	Type      string `json:"type"`
	Text      string `json:"text,omitempty"`
	MediaType string `json:"mediaType,omitempty"`
	Data      string `json:"data,omitempty"`
}

// message is op for the worker. Claude takes text and image blocks in a user
// message: the person's text, then each attachment in order, an image as an
// image block, a file's text as a fenced text block, and any other file as a
// text block naming the retained path, which Claude opens with Read (it reads
// PDFs page by page). Files never travel as paths of their own.
func message(op agent.Operation, parts bool) wireMessage {
	all := agent.Parts(op)
	wire := wireMessage{Operation: op}
	wire.Files = nil
	if !parts {
		wire.Text = agent.Flatten(all)
		return wire
	}
	wire.Text, wire.Images = "", nil
	for _, part := range all {
		switch part.Kind {
		case agent.PartText:
			wire.Parts = append(wire.Parts, wirePart{Type: "text", Text: part.Text})
		case agent.PartImage:
			wire.Parts = append(wire.Parts, wirePart{Type: "image", MediaType: part.Image.MediaType, Data: part.Image.Data})
		case agent.PartFileText:
			wire.Parts = append(wire.Parts, wirePart{Type: "text", Text: agent.Inline(*part.File, part.Text)})
		case agent.PartFileReference:
			wire.Parts = append(wire.Parts, wirePart{Type: "text", Text: agent.Reference(*part.File)})
		}
	}
	return wire
}
