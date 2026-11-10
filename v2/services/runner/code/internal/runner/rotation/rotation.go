// Package rotation supervises native sessions without replacing the Runner's
// logical execution, journal, collection lease, or managed commands.
package rotation

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
)

type Adapter struct {
	Native   agent.Adapter
	Profiles []accounts.Profile
	Eligible []string
	State    *accounts.State
	Resolve  func(accounts.Profile, agent.Open) (agent.Open, error)
	Evidence func() string
}
type session struct {
	attempt       uint64
	mu            sync.Mutex
	adapter       Adapter
	current       agent.Session
	open          agent.Open
	profile       accounts.Profile
	ctx           context.Context
	cancel        context.CancelFunc
	attemptCancel context.CancelFunc
	events        chan agent.Event
	done          chan struct{}
	history       string
	omitted       bool
	caps          agent.Capabilities
	// state mirrors attempt/profile/current for DurableState without s.mu,
	// which is held across provider calls and whole rotations.
	stateMu   sync.Mutex
	state     durable
	stateNow  agent.Session
	detaching bool
}

// durable is this layer's re-attachment state; Native is the current native
// session's own state.
type durable struct {
	Attempt uint64          `json:"attempt"`
	Profile string          `json:"profile"`
	Native  json.RawMessage `json:"native,omitempty"`
}

func (s *session) publish() {
	s.stateMu.Lock()
	s.state = durable{Attempt: s.attempt, Profile: s.profile.ID}
	s.stateNow = s.current
	s.stateMu.Unlock()
}

func (a Adapter) Open(ctx context.Context, o agent.Open) (agent.Session, error) {
	if a.State == nil || a.Resolve == nil {
		return nil, errors.New("rotation requires account state and environment resolver")
	}
	var profile accounts.Profile
	for _, p := range a.Profiles {
		if p.ID == o.Profile {
			profile = p
		}
	}
	if profile.ID == "" {
		return nil, errors.New("rotation profile not found")
	}
	// Validate the whole opt-in pool before starting any native process.
	for _, id := range a.Eligible {
		found := false
		for _, p := range a.Profiles {
			if p.ID == id && p.Provider == profile.Provider && p.Isolation == "environment" {
				found = true
			}
		}
		if !found {
			return nil, errors.New("rotation pool must contain isolated profiles of the selected provider")
		}
	}
	var resumed durable
	var transcript string
	if o.Host != nil && o.Host.Attach != nil {
		transcript = o.Host.Attach.Transcript
		if err := json.Unmarshal(o.Host.Attach.State, &resumed); err != nil || resumed.Profile == "" || len(resumed.Native) == 0 {
			return nil, errors.New("rotation state missing for re-attachment")
		}
		if resumed.Profile != profile.ID {
			// The live native process belongs to a profile rotated to earlier.
			profile = accounts.Profile{}
			for _, p := range a.Profiles {
				if p.ID == resumed.Profile {
					profile = p
				}
			}
			if profile.ID == "" {
				return nil, errors.New("rotated profile no longer configured")
			}
			o.Profile = profile.ID
			var err error
			if o, err = a.Resolve(profile, o); err != nil {
				return nil, err
			}
		}
		inner := *o.Host.Attach
		inner.State = resumed.Native
		// Only this attempt's requests reach the native layer, unprefixed.
		inner.Responded = map[string]bool{}
		prefix := fmt.Sprintf("attempt-%d:", resumed.Attempt)
		for id := range o.Host.Attach.Responded {
			if native, ok := strings.CutPrefix(id, prefix); ok {
				inner.Responded[native] = true
			}
		}
		hosting := *o.Host
		hosting.Attach = &inner
		o.Host = &hosting
	}
	life, cancel := context.WithCancel(ctx)
	attempt, stop := context.WithCancel(life)
	native, err := a.Native.Open(attempt, o)
	if err != nil {
		stop()
		cancel()
		return nil, err
	}
	if o.Host != nil {
		// Later attempts start their own hosts; never re-attach them to this one.
		hosting := *o.Host
		hosting.Attach = nil
		o.Host = &hosting
	}
	if settings, ok := native.(agent.RuntimeSettings); ok {
		o.ProviderSettings = settings.SessionSettings()
	}
	s := &session{attempt: resumed.Attempt, adapter: a, current: native, open: o, profile: profile, ctx: life, cancel: cancel, attemptCancel: stop, events: make(chan agent.Event, 256), done: make(chan struct{}), caps: native.Capabilities()}
	if transcript != "" {
		// A re-attached session rebuilds its handoff excerpt from the journal.
		s.remember(transcript)
	}
	s.publish()
	if agent.IsAssistant(o.Purpose) {
		s.caps.Files = false
		s.caps.Subagents = false
	}
	go s.run()
	return s, nil
}
func (s *session) Reference() string                { s.mu.Lock(); defer s.mu.Unlock(); return s.current.Reference() }
func (s *session) Capabilities() agent.Capabilities { return s.caps }
func (s *session) remember(text string) {
	const limit = 128 << 10
	if len(text) > limit {
		text = text[len(text)-limit:]
		s.omitted = true
	}
	s.history += text
	if len(s.history) > limit {
		s.history = s.history[len(s.history)-limit:]
		s.omitted = true
	}
}
func (s *session) Apply(ctx context.Context, op agent.Operation) (agent.Receipt, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.ctx.Err() != nil {
		return agent.Receipt{}, s.ctx.Err()
	}
	if op.Kind == agent.Answer || op.Kind == agent.Approve {
		prefix := fmt.Sprintf("attempt-%d:", s.attempt)
		native, ok := strings.CutPrefix(op.RequestID, prefix)
		if !ok {
			return agent.Receipt{}, errors.New("request belongs to an earlier subscription attempt")
		}
		op.RequestID = native
	}
	receipt, err := s.current.Apply(ctx, op)
	if err == nil && (op.Kind == agent.Message || op.Kind == agent.Steer) {
		s.remember("\nUSER: " + agent.Prompt(op) + "\n")
	}
	return receipt, err
}
func (s *session) emit(ev agent.Event) bool {
	select {
	case s.events <- ev:
		return true
	case <-s.ctx.Done():
		return false
	}
}
func (s *session) run() {
	defer close(s.done)
	defer close(s.events)
	for {
		s.mu.Lock()
		native := s.current
		s.mu.Unlock()
		ev, err := native.Receive(s.ctx)
		if err != nil {
			s.stateMu.Lock()
			detaching := s.detaching
			s.stateMu.Unlock()
			if s.ctx.Err() == nil && !detaching {
				s.emit(agent.Event{Type: "process_exit", Status: "unknown", Text: err.Error()})
			}
			return
		}
		s.mu.Lock()
		ev.Attempt = s.attempt
		if ev.RequestID != "" {
			ev.RequestID = fmt.Sprintf("attempt-%d:%s", s.attempt, ev.RequestID)
		}
		// Child native IDs can also be recycled after a process restart.
		if ev.AgentID != "" && ev.AgentID != "root" {
			ev.AgentID = fmt.Sprintf("attempt-%d:%s", s.attempt, ev.AgentID)
		}
		if ev.ParentAgentID != "" && ev.ParentAgentID != "root" {
			ev.ParentAgentID = fmt.Sprintf("attempt-%d:%s", s.attempt, ev.ParentAgentID)
		}
		s.mu.Unlock()
		root := ev.AgentID == "" || ev.AgentID == "root"
		if ev.Account != nil {
			s.mu.Lock()
			id := s.profile.ID
			s.mu.Unlock()
			if err = s.adapter.State.Observe(id, *ev.Account); err != nil {
				s.emit(agent.Event{Type: "failure", Status: "account_state_write_failed"})
				return
			}
		}
		if root && ev.Type == "message_delta" {
			s.mu.Lock()
			s.remember(ev.Text)
			s.mu.Unlock()
		}
		if root && ev.Type == "failure" && ev.Account != nil && rotatable(ev.Account.State) && len(s.adapter.Eligible) > 0 {
			observed := ev
			observed.Type = "capacity_observation"
			if !s.emit(observed) {
				return
			}
			if err = s.rotate(ev.Account.State); err == nil {
				continue
			}
			ev.Text = err.Error()
		}
		if !s.emit(ev) {
			return
		}
	}
}
func rotatable(state string) bool {
	return state == "exhausted" || state == "throttled" || state == "auth_required"
}
func (s *session) rotate(reason string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	tried := map[string]bool{s.profile.ID: true}
	candidates := []accounts.Profile{}
	for _, id := range s.adapter.Eligible {
		for _, p := range s.adapter.Profiles {
			if p.ID == id && !tried[id] && s.adapter.State.Eligible(id, time.Now()) {
				tried[id] = true
				candidates = append(candidates, p)
			}
		}
	}
	if len(candidates) == 0 {
		return errors.New("no enabled subscription has available capacity; authenticate or wait for reset")
	}
	if !s.emit(agent.Event{Type: "profile_switching", Status: reason}) {
		return s.ctx.Err()
	}
	// No replacement writer starts until the old native process has exited.
	s.attemptCancel()
	if err := s.current.Close(); err != nil {
		return fmt.Errorf("old provider did not close cleanly: %w", err)
	}
	var failures []error
	for _, p := range candidates {
		if s.ctx.Err() != nil {
			return s.ctx.Err()
		}
		open := s.open
		open.Profile = p.ID
		open.Resume = ""
		var err error
		open, err = s.adapter.Resolve(p, open)
		if err != nil {
			failures = append(failures, fmt.Errorf("%s: credential context unavailable", p.ID))
			continue
		}
		attempt, stop := context.WithCancel(s.ctx)
		native, err := s.adapter.Native.Open(attempt, open)
		if err != nil {
			stop()
			if errors.Is(err, agent.ErrUnsupported) {
				failures = append(failures, fmt.Errorf("%s: provider cannot preserve this permission profile", p.ID))
			} else {
				failures = append(failures, fmt.Errorf("%s: native session startup failed", p.ID))
			}
			continue
		}
		prompt := "Subscription handoff in the SAME Runner execution and working directory. This is a portable conversation checkpoint, not a native session restore. Inspect current files and retained command IDs before continuing. Do not repeat completed changes, commands or external actions. Previous approvals are invalid. Continue the latest user request.\n"
		if s.omitted {
			prompt += "Earlier conversation was omitted to fit the checkpoint. Ask if necessary.\n"
		}
		prompt += s.history
		if s.adapter.Evidence != nil {
			evidence := s.adapter.Evidence()
			if len(evidence) > 128<<10 {
				evidence = evidence[:128<<10] + "\n[EVIDENCE TRUNCATED; inspect retained records]"
			}
			prompt += "\nRUNNER EVIDENCE:\n" + evidence
		}
		s.attempt++
		s.current = native
		s.attemptCancel = stop
		s.profile = p
		s.open = open
		s.publish()
		if !s.emit(agent.Event{Type: "profile_changed", Text: p.ID, SessionID: native.Reference(), Status: "checkpoint_handoff"}) {
			native.Close()
			return s.ctx.Err()
		}
		_, err = native.Apply(s.ctx, agent.Operation{ID: "handoff-" + p.ID + fmt.Sprint(time.Now().UnixNano()), Kind: agent.Message, Text: prompt})
		if err != nil {
			native.Close()
			return fmt.Errorf("checkpoint delivery failed for %s; delivery must be reconciled before retry", p.ID)
		}
		return nil
	}
	return fmt.Errorf("subscription rotation failed: %s", strings.TrimSpace(fmt.Sprint(errors.Join(failures...))))
}
func (s *session) Receive(ctx context.Context) (agent.Event, error) {
	select {
	case ev, ok := <-s.events:
		if !ok {
			return ev, errors.New("provider supervisor ended")
		}
		return ev, nil
	case <-ctx.Done():
		return agent.Event{}, ctx.Err()
	}
}
func (s *session) Close() error {
	s.cancel()
	s.mu.Lock()
	err := s.current.Close()
	s.mu.Unlock()
	<-s.done
	return err
}

func (s *session) DurableState() json.RawMessage {
	s.stateMu.Lock()
	state, native := s.state, s.stateNow
	s.stateMu.Unlock()
	d, ok := native.(agent.Durable)
	if !ok {
		return nil
	}
	if state.Native = d.DurableState(); state.Native == nil {
		return nil
	}
	data, _ := json.Marshal(state)
	return data
}

// Commit forwards only the current attempt's events: cursors of an attempt
// already rotated away belong to a host that no longer exists.
func (s *session) Commit(ev agent.Event) {
	s.stateMu.Lock()
	current, native := s.state.Attempt, s.stateNow
	s.stateMu.Unlock()
	if c, ok := native.(agent.Committer); ok && ev.Attempt == current {
		c.Commit(ev)
	}
}

// Detach leaves the current native process running for a later Runner.
func (s *session) Detach() error {
	s.stateMu.Lock()
	native := s.stateNow
	s.detaching = true
	s.stateMu.Unlock()
	d, ok := native.(agent.Detacher)
	if !ok {
		return errors.New("native session cannot detach")
	}
	if err := d.Detach(); err != nil {
		s.stateMu.Lock()
		s.detaching = false
		s.stateMu.Unlock()
		return err
	}
	s.cancel()
	<-s.done
	return nil
}

func (s *session) SessionSettings() json.RawMessage {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append(json.RawMessage(nil), s.open.ProviderSettings...)
}
