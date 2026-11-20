// Package rotation supervises native sessions without replacing the Runner's
// logical execution, journal, collection lease, or managed commands.
package rotation

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"maps"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

type Adapter struct {
	Native   agent.Adapter
	Profiles []accounts.Profile
	// Current lists the subscriptions signed in now, so a person can switch a
	// running conversation to one added after it started; nil uses Profiles.
	Current  func() []accounts.Profile
	Eligible []string
	State    *accounts.State
	Resolve  func(accounts.Profile, agent.Open) (agent.Open, error)
	Evidence func() string
	// Switching reports whether an account policy lets the conversation move
	// to another account by itself at a limit now; nil never switches by
	// itself. Without it a limit stops the conversation and the person
	// switches (SwitchProfile).
	Switching func() bool
}

// profiles are the subscriptions signed in now when Current is set, so a
// subscription added after the conversation started can be switched to, by
// the person or automatically; otherwise the list taken at start.
func (a Adapter) profiles() []accounts.Profile {
	if a.Current != nil {
		return a.Current()
	}
	return a.Profiles
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
	attemptCtx    context.Context
	attemptCancel context.CancelFunc
	// checkpoint: the conversation moved to this attempt between requests, so
	// the handoff checkpoint travels with the next message instead of a turn.
	checkpoint bool
	events     chan agent.Event
	done       chan struct{}
	history    string
	omitted    bool
	// replayed counts, by agent.ReplayKey, root reply text already in the
	// excerpt from the journal: the host replays its line whole after a
	// re-attachment, and that text must not be remembered twice. It covers
	// only that line: replayedAfter is the last line recorded whole.
	replayed      map[string]int
	replayedAfter uint64
	caps          agent.Capabilities
	// state mirrors attempt/profile/current for DurableState without s.mu,
	// which is held across provider calls and whole rotations.
	stateMu   sync.Mutex
	state     durable
	stateNow  agent.Session
	detaching bool
	// observeFailing: saving the account state failed and was reported;
	// only run reads or writes it.
	observeFailing bool
}

// durable is this layer's re-attachment state; Native is the current native
// session's own state.
type durable struct {
	Attempt    uint64          `json:"attempt"`
	Profile    string          `json:"profile"`
	Checkpoint bool            `json:"checkpoint,omitempty"`
	Native     json.RawMessage `json:"native,omitempty"`
}

func (s *session) publish() {
	s.stateMu.Lock()
	s.state = durable{Attempt: s.attempt, Profile: s.profile.ID, Checkpoint: s.checkpoint}
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
	if err := validPool(a.Profiles, profile.Provider, a.Eligible); err != nil {
		return nil, err
	}
	var resumed durable
	var transcript string
	var replayed map[string]int
	var replayedAfter uint64
	if o.Host != nil && o.Host.Attach != nil {
		transcript = o.Host.Attach.Transcript
		replayed, replayedAfter = maps.Clone(o.Host.Attach.Replayed), o.Host.Attach.Cursor
		if err := json.Unmarshal(o.Host.Attach.State, &resumed); err != nil || resumed.Profile == "" || len(resumed.Native) == 0 {
			return nil, errors.New("rotation state missing for re-attachment")
		}
		if resumed.Profile != profile.ID {
			// The live native process belongs to a profile rotated to earlier.
			profile = accounts.Profile{}
			for _, p := range a.profiles() {
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
		inner.Responded = map[string]agent.Operation{}
		prefix := fmt.Sprintf("attempt-%d:", resumed.Attempt)
		for id, response := range o.Host.Attach.Responded {
			if native, ok := strings.CutPrefix(id, prefix); ok {
				response.RequestID = native
				inner.Responded[native] = response
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
	// Historical forking applies only when this conversation first opens.
	o.ForkThread, o.ForkAfterTurn = "", ""
	if o.Host != nil {
		// Later attempts start their own hosts; never re-attach them to this one.
		hosting := *o.Host
		hosting.Attach = nil
		o.Host = &hosting
	}
	if settings, ok := native.(agent.RuntimeSettings); ok {
		o.ProviderSettings = settings.SessionSettings()
	}
	s := &session{attempt: resumed.Attempt, adapter: a, current: native, open: o, profile: profile, ctx: life, cancel: cancel, attemptCtx: attempt, attemptCancel: stop, checkpoint: resumed.Checkpoint, events: make(chan agent.Event, 256), done: make(chan struct{}), caps: native.Capabilities(), replayed: replayed, replayedAfter: replayedAfter}
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

// validPool admits only isolated profiles of the provider: switching to one
// never touches the credentials a sibling conversation uses.
func validPool(profiles []accounts.Profile, provider string, ids []string) error {
	for _, id := range ids {
		found := false
		for _, p := range profiles {
			if p.ID == id && p.Provider == provider && p.Isolation == "environment" {
				found = true
			}
		}
		if !found {
			return errors.New("rotation pool must contain isolated profiles of the selected provider")
		}
	}
	return nil
}
func (s *session) Reference() string { s.mu.Lock(); defer s.mu.Unlock(); return s.current.Reference() }

// HostID is the current profile's native host (agent.Hosted).
func (s *session) HostID() string                   { s.mu.Lock(); defer s.mu.Unlock(); return agent.HostOf(s.current) }
func (s *session) Capabilities() agent.Capabilities { return s.caps }

// Title asks the current profile's session (agent.Titler).
func (s *session) Title(ctx context.Context, description string) (string, error) {
	s.mu.Lock()
	current := s.current
	s.mu.Unlock()
	return agent.TitleOf(ctx, current, description)
}

// replayedEvent reports, and consumes, an event the journal recorded before
// the restart (so the excerpt holds its text). The replay ends with the first
// line completed after the last one recorded whole.
func (s *session) replayedEvent(ev agent.Event) bool {
	if s.replayed == nil {
		return false
	}
	seen := false
	if key := agent.ReplayKey(ev); s.replayed[key] > 0 {
		s.replayed[key]--
		seen = true
	}
	if !ev.Partial && ev.Cursor > s.replayedAfter {
		s.replayed = nil
	}
	return seen
}

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
	if op.Kind == agent.StopAgent {
		// Sub-agent ids carry their attempt; one of an earlier subscription
		// attempt ended with that attempt's native session.
		native, ok := strings.CutPrefix(op.AgentID, fmt.Sprintf("attempt-%d:", s.attempt))
		if !ok {
			return agent.Receipt{}, errors.New("sub-agent belongs to an earlier subscription attempt")
		}
		op.AgentID = native
	}
	carried := false
	if s.checkpoint && op.Kind == agent.Message {
		// The first request on a new subscription carries the conversation so far.
		op.Context = strings.TrimSpace(s.handoffPrompt(false) + "\n" + op.Context)
		carried = true
	}
	receipt, err := s.current.Apply(ctx, op)
	if err == nil && carried {
		s.checkpoint = false
		s.publish()
	}
	if err == nil && op.Kind == agent.Permission {
		// A later subscription attempt starts in the mode now in force.
		s.open.Permission = op.Permission
	}
	if err == nil && op.Kind == agent.Model {
		// A later subscription attempt runs the model now in force, including
		// when the provider's carried settings name the model they started on.
		s.open.Model = op.Model
		if op.Effort != "" {
			s.open.Effort = op.Effort
		}
		s.open.ProviderSettings = withModel(s.open.ProviderSettings, op.Model, op.Effort)
	}
	if err == nil && (op.Kind == agent.Message || op.Kind == agent.Steer) {
		s.remember("\nUSER: " + agent.Prompt(op) + "\n")
	}
	if err == nil && op.Kind == agent.RevokeGrant {
		// A later subscription attempt starts without it.
		s.open.Grants = agent.WithoutGrant(s.open.Grants, op.GrantID)
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
		native, attempt := s.current, s.attemptCtx
		s.mu.Unlock()
		ev, err := native.Receive(attempt)
		if err != nil {
			// A switch the person asked for closed this attempt: read the next.
			s.mu.Lock()
			switched := s.current != native
			s.mu.Unlock()
			if switched && s.ctx.Err() == nil {
				continue
			}
			s.stateMu.Lock()
			detaching := s.detaching
			s.stateMu.Unlock()
			if s.ctx.Err() == nil && !detaching {
				// "exited" when the native process's exit was observed: the
				// engine then faults the conversation as ended (agent.ProcessExitFault),
				// not as a lost stream a re-attachment settles.
				status := "unknown"
				if errors.Is(err, agent.ErrProcessExited) {
					status = "exited"
				}
				s.emit(agent.Event{Type: "process_exit", Status: status, Text: err.Error()})
			}
			return
		}
		s.mu.Lock()
		if s.current != native {
			// Read from an attempt that was switched away meanwhile.
			s.mu.Unlock()
			continue
		}
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
			// Saving what the provider said about the subscription is the
			// Runner's bookkeeping: a write that fails (a full disk) is
			// reported once and never stops reading the agent.
			if err = s.adapter.State.Observe(id, *ev.Account); err != nil && !s.observeFailing {
				log.Printf("subscription %s: account state not saved: %v", id, err)
				s.observeFailing = true
				if !s.emit(agent.StorageError("the subscription status", err)) {
					return
				}
			} else if err == nil {
				s.observeFailing = false
			}
		}
		s.mu.Lock()
		if replayed := s.replayedEvent(ev); root && ev.Type == "message_delta" && !replayed {
			s.remember(ev.Text)
		}
		if root && ev.Type == agent.GrantEvent {
			// A later subscription attempt starts holding it too.
			if g, err := agent.GrantOf(ev, time.Now()); err == nil {
				s.open.Grants = agent.WithGrant(s.open.Grants, g)
			}
		}
		s.mu.Unlock()
		s.mu.Lock()
		pooled := len(s.adapter.Eligible) > 0 && s.adapter.Switching != nil
		s.mu.Unlock()
		pooled = pooled && s.adapter.Switching()
		if root && ev.Type == "failure" && ev.Account != nil && rotatable(ev.Account.State) && pooled {
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
	current := s.adapter.profiles()
	for _, id := range s.adapter.Eligible {
		for _, p := range current {
			if p.ID == id && !tried[id] && s.adapter.State.Eligible(id, time.Now()) {
				tried[id] = true
				candidates = append(candidates, p)
			}
		}
	}
	if len(candidates) == 0 {
		return errors.New("no enabled subscription has available capacity; authenticate or wait for reset")
	}
	return s.switchLocked(reason, candidates, true, false)
}

// SwitchProfile moves the conversation to profile id because the person asked
// (agent.ProfileSwitcher). Unlike automatic rotation it takes the chosen
// profile even when its last observation says it is unavailable: the person
// may know better, and a provider refusal comes back as a limit again.
func (s *session) SwitchProfile(id string, continueTurn bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.ctx.Err() != nil {
		return s.ctx.Err()
	}
	if id == s.profile.ID {
		if !continueTurn {
			return errors.New("the conversation already uses this subscription")
		}
		return s.retryLocked()
	}
	for _, p := range s.adapter.profiles() {
		if p.ID == id {
			if p.Provider != s.profile.Provider {
				return errors.New("a conversation can only switch to a subscription of its own provider")
			}
			return s.switchLocked("requested", []accounts.Profile{p}, continueTurn, true)
		}
	}
	return errors.New("subscription not found")
}

// SetRotation replaces the automatic-switching pool (agent.ProfileSwitcher).
func (s *session) SetRotation(ids []string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := validPool(s.adapter.profiles(), s.profile.Provider, ids); err != nil {
		return err
	}
	s.adapter.Eligible = append([]string(nil), ids...)
	return nil
}

// retryLocked continues the interrupted request on the same subscription and
// native conversation, as after its limit reset. No operation is replayed:
// the agent is asked to continue what the conversation already holds.
func (s *session) retryLocked() error {
	if !s.emit(agent.Event{Type: "profile_changed", Text: s.profile.ID, SessionID: s.current.Reference(), Status: "retry_same_profile", Data: s.switchData(s.profile.ID, "requested", true)}) {
		return s.ctx.Err()
	}
	prompt := "The previous turn stopped because this subscription reached a usage limit. Continue the latest user request from where it stopped. Inspect current files and retained command IDs first; do not repeat completed changes, commands or external actions."
	if _, err := s.current.Apply(s.ctx, agent.Operation{ID: fmt.Sprintf("retry-%s-%d", s.profile.ID, time.Now().UnixNano()), Kind: agent.Message, Text: prompt}); err != nil {
		return fmt.Errorf("continuing on %s failed; delivery must be reconciled before retry: %w", s.profile.ID, err)
	}
	return nil
}

// switchData describes a switch for the conversation: where it came from and
// what that subscription last reported, so a client can say when it resets.
func (s *session) switchData(from, reason string, requested bool) json.RawMessage {
	observed := s.adapter.State.Observation(from)
	data, _ := json.Marshal(map[string]any{"from": from, "reason": reason, "requested": requested, "fromState": observed.State, "resetAt": observed.ResetAt})
	return data
}

// handoffPrompt is the portable checkpoint a new native conversation starts
// from. continueTurn asks it to finish the latest request; otherwise the
// person's next message follows it.
func (s *session) handoffPrompt(continueTurn bool) string {
	prompt := "Subscription handoff in the SAME Runner execution and working directory. This is a portable conversation checkpoint, not a native session restore. Inspect current files and retained command IDs before continuing. Do not repeat completed changes, commands or external actions. Previous approvals are invalid. "
	if continueTurn {
		prompt += "Continue the latest user request.\n"
	} else {
		prompt += "The latest user request was finished; the user's next message follows this checkpoint.\n"
	}
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
	return prompt
}

// switchLocked replaces the native attempt with the first candidate that
// starts. The old process is closed before any replacement starts, so there is
// never a second writer. continueTurn delivers the checkpoint now as a turn;
// otherwise it waits for the next message. Holds s.mu.
func (s *session) switchLocked(reason string, candidates []accounts.Profile, continueTurn, requested bool) error {
	// Resolve credentials first: a candidate that cannot start must not cost
	// the running agent.
	type prepared struct {
		profile accounts.Profile
		open    agent.Open
	}
	var ready []prepared
	var failures []error
	for _, p := range candidates {
		open := s.open
		open.Profile = p.ID
		open.Resume = ""
		var err error
		if open, err = s.adapter.Resolve(p, open); err != nil {
			failures = append(failures, fmt.Errorf("%s: credential context unavailable", p.ID))
			continue
		}
		ready = append(ready, prepared{p, open})
	}
	if len(ready) == 0 {
		return fmt.Errorf("subscription rotation failed: %s", strings.TrimSpace(fmt.Sprint(errors.Join(failures...))))
	}
	data := s.switchData(s.profile.ID, reason, requested)
	if !s.emit(agent.Event{Type: "profile_switching", Status: reason}) {
		return s.ctx.Err()
	}
	// No replacement writer starts until the old native process has exited.
	s.attemptCancel()
	if err := s.current.Close(); err != nil {
		return fmt.Errorf("old provider did not close cleanly: %w", err)
	}
	for _, candidate := range ready {
		p, open := candidate.profile, candidate.open
		if s.ctx.Err() != nil {
			return s.ctx.Err()
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
		prompt := s.handoffPrompt(true)
		s.attempt++
		s.current = native
		s.replayed = nil // A new native session: its lines are not the replayed one.
		s.attemptCtx, s.attemptCancel = attempt, stop
		s.profile = p
		s.open = open
		s.checkpoint = !continueTurn
		s.publish()
		status := "checkpoint_handoff"
		if !continueTurn {
			status = "checkpoint_next_message"
		}
		if !s.emit(agent.Event{Type: "profile_changed", Text: p.ID, SessionID: native.Reference(), Status: status, Data: data}) {
			native.Close()
			return s.ctx.Err()
		}
		if !continueTurn {
			return nil
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

// withModel replaces the model a provider's carried settings name, if they
// name one; other settings, and settings that name none, are kept as they are.
func withModel(settings json.RawMessage, model, effort string) json.RawMessage {
	var fields map[string]json.RawMessage
	if len(settings) == 0 || json.Unmarshal(settings, &fields) != nil || fields["model"] == nil {
		return settings
	}
	fields["model"], _ = json.Marshal(model)
	if effort != "" {
		fields["effort"], _ = json.Marshal(effort)
	}
	data, err := json.Marshal(fields)
	if err != nil {
		return settings
	}
	return data
}
