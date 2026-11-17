package telemetry

import (
	"context"
	"encoding/json"
	"errors"
	"log"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// Adapter goes inside rotation: every native Open receives an immutable scope.
type Adapter struct {
	Native  agent.Adapter
	Store   Store
	Prices  Pricer
	Gateway *Gateway
	Scope   Scope
}
type session struct {
	agent.Session
	adapter          Adapter
	scope            Scope
	release          func()
	metered, resumed bool
	// failing: a metering write failed and was reported; the next notice
	// waits for a write that succeeds. notice is the report, delivered as the
	// next event. Receive is called by one goroutine, so neither is locked.
	failing bool
	notice  *agent.Event
}

// durable keeps the metering attempt identity, so usage after a Runner restart
// is attributed to the same attempt rather than a new one.
type durable struct {
	Attempt string          `json:"attempt"`
	Native  json.RawMessage `json:"native"`
}

func (a Adapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	scope := a.Scope
	scope.Profile = o.Profile
	scope.Attempt = identifier()
	if o.Host != nil && o.Host.Attach != nil {
		var state durable
		if err := json.Unmarshal(o.Host.Attach.State, &state); err != nil || state.Attempt == "" {
			return nil, errors.New("metering state missing for re-attachment")
		}
		scope.Attempt = state.Attempt
		inner := *o.Host.Attach
		inner.State = state.Native
		hosting := *o.Host
		hosting.Attach = &inner
		o.Host = &hosting
	}
	release := func() {}
	metered := o.Gateway != nil && o.Gateway.ID == "metered"
	if metered {
		// Never silently redirect configured cloud providers or custom auth endpoints.
		for _, env := range o.Env {
			for _, prefix := range []string{"ANTHROPIC_BASE_URL=", "CLAUDE_CODE_USE_BEDROCK=", "CLAUDE_CODE_USE_VERTEX=", "CLAUDE_CODE_USE_FOUNDRY="} {
				if strings.HasPrefix(env, prefix) && strings.TrimPrefix(env, prefix) != "" && strings.TrimPrefix(env, prefix) != "0" {
					return nil, errors.New("metering requires the standard provider route; custom provider environment retained")
				}
			}
		}
		url, done, err := a.Gateway.Register(scope)
		if err != nil {
			return nil, err
		}
		release = done
		o.Gateway = &agent.Gateway{ID: "metered", URL: url}
	}
	native, err := a.Native.Open(ctx, o)
	if err != nil {
		release()
		return nil, err
	}
	return &session{Session: native, adapter: a, scope: scope, release: release, metered: metered, resumed: o.Resume != "" || (o.Host != nil && o.Host.Attach != nil)}, nil
}

func (s *session) DurableState() json.RawMessage {
	native, ok := s.Session.(agent.Durable)
	if !ok {
		return nil
	}
	inner := native.DurableState()
	if inner == nil {
		return nil
	}
	data, _ := json.Marshal(durable{Attempt: s.scope.Attempt, Native: inner})
	return data
}
func (s *session) Commit(ev agent.Event) {
	if native, ok := s.Session.(agent.Committer); ok {
		native.Commit(ev)
	}
}

// Detach leaves the native process running and releases only this Runner's
// gateway route; the next Runner registers its own for the same attempt.
func (s *session) Detach() error {
	native, ok := s.Session.(agent.Detacher)
	if !ok {
		return errors.New("native session cannot detach")
	}
	if err := native.Detach(); err != nil {
		return err
	}
	s.release()
	return nil
}
func (s *session) Close() error { err := s.Session.Close(); s.release(); return err }

// Title forwards to the native session (agent.Titler).
func (s *session) Title(ctx context.Context, description string) (string, error) {
	return agent.TitleOf(ctx, s.Session, description)
}
func (s *session) SessionSettings() json.RawMessage {
	if native, ok := s.Session.(agent.RuntimeSettings); ok {
		return native.SessionSettings()
	}
	return nil
}

// Receive passes the agent's events on, metering usage and context as they
// go. Metering is the Runner's own bookkeeping: a write that fails (a full
// disk: SQLite's "database or disk is full") is reported once, as a notice in
// the conversation and in the log, and never ends the stream. Ending it here
// is what once made the supervisor stop reading a live agent and the
// conversation fail ("provider supervisor ended").
func (s *session) Receive(ctx context.Context) (agent.Event, error) {
	if s.notice != nil {
		notice := *s.notice
		s.notice = nil
		return notice, nil
	}
	ev, err := s.Session.Receive(ctx)
	if err != nil {
		return ev, err
	}
	wrote, err := s.record(ctx, ev)
	switch {
	case err != nil && ctx.Err() == nil && !s.failing:
		log.Printf("metering %s/%s: usage not saved: %v", s.scope.Conversation, s.scope.Execution, err)
		s.failing = true
		notice := agent.StorageError("this conversation's usage", err)
		s.notice = &notice
	case err == nil && wrote:
		s.failing = false
	}
	return ev, nil
}

// record meters what ev says about usage or context, reporting whether it
// wrote anything.
func (s *session) record(ctx context.Context, ev agent.Event) (bool, error) {
	if ev.Type != "usage" && ev.Type != "context" {
		return false, nil
	}
	var body map[string]any
	if json.Unmarshal(ev.Data, &body) != nil {
		return false, nil
	}
	actor := ev.AgentID
	if actor == "" {
		actor = "root"
	}
	if ev.Type == "context" {
		var c ContextSnapshot
		if json.Unmarshal(ev.Data, &c) != nil {
			return false, nil
		}
		c.Scope = s.scope
		c.Agent = actor
		if c.Source == "" {
			c.Source = "sdk"
		}
		return true, s.adapter.Store.PutContext(ctx, c)
	}
	if strings.HasPrefix(s.scope.Provider, "acp:") {
		return true, s.reportedUsage(ctx, actor, body)
	}
	if s.scope.Provider == "codex" {
		usage := object(body["tokenUsage"])
		last := object(usage["last"])
		total := object(usage["total"])
		c := ContextSnapshot{Scope: s.scope, Agent: actor, Source: "sdk", Used: count(last["totalTokens"]), Capacity: count(usage["modelContextWindow"])}
		if err := s.adapter.Store.PutContext(ctx, c); err != nil {
			return true, err
		}
		if !s.metered && len(total) > 0 {
			input, cached := count(total["inputTokens"]), count(total["cachedInputTokens"])
			if cached == nil {
				cached = number(0)
			}
			if input != nil && *input >= *cached {
				r := Record{Scope: s.scope, Agent: actor, Parent: ev.ParentAgentID, Source: "sdk", ResponseID: str(body["threadId"]), Status: "complete", Tokens: Tokens{Input: number(*input - *cached), Output: count(total["outputTokens"]), CacheRead: cached, CacheWrite5m: number(0), CacheWrite1h: number(0)}}
				return true, s.adapter.Store.PutCumulative(ctx, r, s.resumed)
			}
		}
		return true, nil
	}
	if s.metered || str(body["requestId"]) == "" {
		return false, nil
	}
	parser := NewParser("claude", "application/json")
	raw, _ := json.Marshal(map[string]any{"id": body["requestId"], "model": body["model"], "usage": body["usage"]})
	parser.Push(raw)
	r := parser.Finish(true)
	r.Scope = s.scope
	r.Source = "sdk"
	r.Agent = actor
	r.Parent = ev.ParentAgentID
	r.ID = "sdk-message:" + s.scope.Conversation + "/" + s.scope.Profile + "/" + r.ResponseID
	return true, s.adapter.Store.Put(ctx, s.adapter.Prices.Price(r))
}

// HostID forwards the native session's host (agent.Hosted).
func (s *session) HostID() string { return agent.HostOf(s.Session) }
