package claude

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/jsonl"
)

type Adapter struct{ Node, Worker, Binary string }
type session struct {
	mu    sync.Mutex
	p     *jsonl.Process
	ref   string
	ctx   context.Context
	tools func(context.Context, string, json.RawMessage) (string, error)
}

// durable is the adapter state a later Runner needs to re-attach.
type durable struct {
	Host string `json:"host"`
}

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
		if h, err = jsonl.Launch(ctx, o.Host.Root); err != nil {
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
	result, err := p.Call(setup, "open", map[string]string{"cwd": o.Cwd, "purpose": o.Purpose, "model": o.Model, "resume": o.Resume, "permission": o.Permission, "workflow": o.Workflow, "binary": a.Binary, "context": o.Context})
	if err != nil {
		p.Close()
		return nil, err
	}
	var response struct {
		Reference string `json:"reference"`
	}
	if err = json.Unmarshal(result, &response); err != nil {
		p.Close()
		return nil, err
	}
	return &session{p: p, ref: response.Reference, ctx: ctx, tools: o.Tools}, nil
}
func attach(ctx context.Context, o agent.Open) (agent.Session, error) {
	var state durable
	if err := json.Unmarshal(o.Host.Attach.State, &state); err != nil || state.Host == "" {
		return nil, errors.New("Claude session has no re-attachable host")
	}
	p, err := jsonl.Attach(ctx, o.Host.Root, state.Host, o.Host.Attach.Cursor)
	if err != nil {
		return nil, err
	}
	if o.Gateway != nil {
		if err = p.SetUpstream(o.Gateway.URL); err != nil {
			_ = p.Detach()
			return nil, err
		}
	}
	return &session{p: p, ref: o.Host.Attach.Reference, ctx: ctx, tools: o.Tools}, nil
}

func (s *session) DurableState() json.RawMessage {
	if s.p.HostID() == "" {
		return nil
	}
	data, _ := json.Marshal(durable{Host: s.p.HostID()})
	return data
}

// Commit acknowledges a native line once its last event is recorded.
func (s *session) Commit(ev agent.Event) {
	if !ev.Partial {
		s.p.Commit(ev.Cursor)
	}
}
func (s *session) Detach() error     { return s.p.Detach() }
func (s *session) Reference() string { s.mu.Lock(); defer s.mu.Unlock(); return s.ref }
func (s *session) Capabilities() agent.Capabilities {
	return agent.Capabilities{Images: true, Files: true, Questions: true, Approvals: true, Resume: true, Subagents: true, ProfileIsolation: "environment credentials; shared keychain isolation unverified"}
}
func (s *session) Apply(ctx context.Context, op agent.Operation) (agent.Receipt, error) {
	r := agent.Receipt{ID: op.ID}
	if op.Kind == agent.Steer {
		return r, agent.ErrUnsupported
	}
	wire := op
	wire.Text = agent.Prompt(op)
	wire.Files = nil
	result, err := s.p.Call(ctx, string(op.Kind), wire)
	if err != nil {
		return r, err
	}
	err = json.Unmarshal(result, &r)
	r.ID = op.ID
	return r, err
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
				return agent.Event{}, err
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
			return ev, err
		}
		ev.Cursor = m.Seq
		if ev.SessionID != "" && (ev.AgentID == "" || ev.AgentID == "root") {
			s.mu.Lock()
			s.ref = ev.SessionID
			s.mu.Unlock()
		}
		return ev, nil
	}
}
func (s *session) Close() error { return s.p.Close() }
