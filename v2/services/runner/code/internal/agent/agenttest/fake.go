// Package agenttest supplies an explicitly fake provider for deterministic
// lifecycle, race, recovery, and transport tests. It is never registered by the Runner.
package agenttest

import (
	"context"
	"encoding/json"
	"errors"
	"sync"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
)

type Adapter struct {
	Session *Session
	Steer   bool
	// Durable, when set, makes sessions re-attachable with this state.
	Durable json.RawMessage
}

func (a *Adapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	a.Session = &Session{Events: make(chan agent.Event, 128), Applied: make(chan agent.Operation, 128), Ref: o.Resume, Open: o, Steer: a.Steer, detached: make(chan struct{})}
	if a.Durable != nil {
		a.Session.State = a.Durable
	}
	if o.Host != nil && o.Host.Attach != nil {
		a.Session.Ref = o.Host.Attach.Reference
	}
	if a.Session.Ref == "" {
		a.Session.Ref = "native-reference"
	}
	return a.Session, nil
}

type Session struct {
	mu      sync.Mutex
	Events  chan agent.Event
	Applied chan agent.Operation
	Ref     string
	Open    agent.Open
	Steer   bool
	Closed  bool
	// State is the fake's DurableState; nil means it cannot outlive a Runner.
	State     json.RawMessage
	Detached  bool
	Committed []uint64
	detached  chan struct{}
}

func (s *Session) DurableState() json.RawMessage { s.mu.Lock(); defer s.mu.Unlock(); return s.State }

// Commit acknowledges a line with its last event, as the real adapters do.
func (s *Session) Commit(ev agent.Event) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if !ev.Partial {
		s.Committed = append(s.Committed, ev.Cursor)
	}
}
func (s *Session) Detach() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.State == nil {
		return errors.New("fake session is not durable")
	}
	if !s.Detached {
		s.Detached = true
		close(s.detached)
	}
	return nil
}
func (s *Session) Commits() []uint64 {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]uint64(nil), s.Committed...)
}

func (s *Session) Reference() string { return s.Ref }
func (s *Session) Capabilities() agent.Capabilities {
	return agent.Capabilities{Images: true, Files: true, Questions: true, Approvals: true, Resume: true, Steer: s.Steer}
}
func (s *Session) Apply(ctx context.Context, o agent.Operation) (agent.Receipt, error) {
	s.Applied <- o
	return agent.Receipt{ID: o.ID, Delivery: "acknowledged", ProviderID: "native-turn"}, nil
}
func (s *Session) Receive(ctx context.Context) (agent.Event, error) {
	select {
	case e := <-s.Events:
		return e, nil
	case <-s.detached:
		return agent.Event{}, errors.New("detached")
	case <-ctx.Done():
		return agent.Event{}, ctx.Err()
	}
}
func (s *Session) Close() error { s.mu.Lock(); defer s.mu.Unlock(); s.Closed = true; return nil }
