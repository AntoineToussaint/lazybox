package engine

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
)

type accepted struct {
	fingerprint [32]byte
	receipt     agent.Receipt
	// adopted: accepted by an earlier Runner, whose fingerprint was not kept.
	adopted bool
}
type Snapshot struct {
	Title             string                    `json:"title,omitempty"`
	Purpose           string                    `json:"purpose"`
	ID                string                    `json:"id"`
	Profile           string                    `json:"profile"`
	Reference         string                    `json:"reference"`
	GatewayID         string                    `json:"gatewayId,omitempty"`
	Capabilities      agent.Capabilities        `json:"capabilities"`
	Busy              bool                      `json:"busy"`
	Closed            bool                      `json:"closed"`
	Fault             string                    `json:"fault,omitempty"`
	Queued            int                       `json:"queued"`
	Pending           map[string]agent.Event    `json:"pending"`
	Status            agent.Status              `json:"status"`
	Agents            map[string]agent.Activity `json:"agents"`
	Account           *agent.AccountObservation `json:"account,omitempty"`
	ActivityTruncated bool                      `json:"activityTruncated"`
}

type Engine struct {
	responses map[string]bool
	// replayed holds events already recorded from a native line the previous
	// Runner recorded only in part; the replayed line must not record them twice.
	replayed map[string]int
	// replayedAfter is the last line the previous Runner recorded whole: the
	// replay ends with the first complete line after it, or a new native session.
	replayedAfter uint64
	mu            sync.Mutex
	opMu          sync.Mutex
	provider      agent.Session
	journal       *journal.Journal
	ctx           context.Context
	cancel        context.CancelFunc
	state         Snapshot
	queue         []agent.Operation
	ops           map[string]accepted
	done          chan struct{}
	closeOnce     sync.Once
	closeErr      error
	capture       func(Snapshot, json.RawMessage) error
	// durable records what a later Runner needs to re-attach; nil disables it.
	durable     func(Attachment) error
	lastDurable []byte
	detaching   bool
	detachOnce  sync.Once
	detachErr   error
}

func New(ctx context.Context, adapter agent.Adapter, open agent.Open, spool *journal.Journal) (*Engine, error) {
	return NewPersisted(ctx, adapter, open, spool, nil)
}

// NewPersisted commits observed state through an injected durable recorder.
func NewPersisted(ctx context.Context, adapter agent.Adapter, open agent.Open, spool *journal.Journal, capture func(Snapshot, json.RawMessage) error) (*Engine, error) {
	return NewDurable(ctx, adapter, open, spool, capture, nil)
}

// NewDurable also records an Attachment after every change, so a later Runner
// can re-attach to the live native process with Adopt.
func NewDurable(ctx context.Context, adapter agent.Adapter, open agent.Open, spool *journal.Journal, capture func(Snapshot, json.RawMessage) error, durable func(Attachment) error) (*Engine, error) {
	life, cancel := context.WithCancel(ctx)
	s, err := adapter.Open(life, open)
	if err != nil {
		cancel()
		return nil, err
	}
	e := &Engine{capture: capture, durable: durable, provider: s, journal: spool, ctx: life, cancel: cancel, ops: map[string]accepted{}, responses: map[string]bool{}, done: make(chan struct{}), state: Snapshot{ID: open.ID, Purpose: open.Purpose, Profile: open.Profile, Reference: s.Reference(), Capabilities: s.Capabilities(), Pending: map[string]agent.Event{}}}
	if open.Gateway != nil {
		e.state.GatewayID = open.Gateway.ID
	}
	e.state.Agents = map[string]agent.Activity{"root": {ID: "root", Name: "Main agent", Kind: "agent", Status: agent.Starting, UpdatedAt: time.Now().UTC()}}
	if spool.Count() != 0 {
		cancel()
		s.Close()
		return nil, errors.New("session journal already exists; use a new execution identity and explicit provider resume")
	}
	if _, err = spool.Append(map[string]any{"type": "session_opened", "session": e.state}); err != nil {
		cancel()
		s.Close()
		return nil, err
	}
	if err = e.persistLocked(); err != nil {
		cancel()
		s.Close()
		return nil, err
	}
	go e.receive()
	return e, nil
}

func (e *Engine) Snapshot() Snapshot {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.snapshotLocked()
}
func (e *Engine) snapshotLocked() Snapshot {
	s := e.state
	s.Queued = len(e.queue)
	s.Pending = map[string]agent.Event{}
	for k, v := range e.state.Pending {
		s.Pending[k] = v
	}
	s.Agents = map[string]agent.Activity{}
	for id, activity := range e.state.Agents {
		s.Agents[id] = activity
	}
	if e.state.Account != nil {
		value := *e.state.Account
		value.Windows = append([]agent.UsageWindow(nil), value.Windows...)
		s.Account = &value
	}
	s.Status = snapshotStatus(s)
	return s
}

func validate(op agent.Operation, c agent.Capabilities) error {
	if op.ID == "" || len(op.ID) > 128 {
		return errors.New("operation id required (max 128 bytes)")
	}
	switch op.Kind {
	case agent.Message, agent.Steer:
		if op.Text == "" && len(op.Images) == 0 && len(op.Files) == 0 {
			return errors.New("empty message")
		}
		if len(op.Text) > 1<<20 || len(op.Images) > 4 {
			return errors.New("message exceeds limits")
		}
		for _, img := range op.Images {
			if len(img.Data) > 8<<20 || (img.MediaType != "image/png" && img.MediaType != "image/jpeg" && img.MediaType != "image/webp" && img.MediaType != "image/gif") {
				return errors.New("invalid image")
			}
		}
		if len(op.Images) > 0 && !c.Images {
			return agent.ErrUnsupported
		}
		if len(op.Files) > 4 {
			return errors.New("too many attached files")
		}
		if len(op.Files) > 0 && !c.Files {
			return agent.ErrUnsupported
		}
		if op.Kind == agent.Steer && !c.Steer {
			return agent.ErrUnsupported
		}
	case agent.Interrupt:
	case agent.Answer:
		if !c.Questions {
			return agent.ErrUnsupported
		}
		if op.RequestID == "" {
			return errors.New("request id required")
		}
	case agent.Approve:
		if !c.Approvals {
			return agent.ErrUnsupported
		}
		if op.RequestID == "" || (op.Decision != "accept" && op.Decision != "decline" && op.Decision != "cancel") {
			return errors.New("invalid approval")
		}
	default:
		return agent.ErrUnsupported
	}
	return nil
}

// Apply accepts once under an operation identity. Message queueing is generic;
// delivery uses the engine lifetime rather than the caller's RPC lifetime.
func (e *Engine) Apply(ctx context.Context, op agent.Operation) (agent.Receipt, error) {
	if err := ctx.Err(); err != nil {
		return agent.Receipt{}, err
	}
	e.opMu.Lock()
	defer e.opMu.Unlock()
	b, _ := json.Marshal(op)
	fingerprint := sha256.Sum256(b)
	e.mu.Lock()
	if old, ok := e.ops[op.ID]; ok {
		e.mu.Unlock()
		if !old.adopted && old.fingerprint != fingerprint {
			return agent.Receipt{}, errors.New("operation id reused with different content")
		}
		return old.receipt, nil
	}
	if e.state.Closed || e.state.Fault != "" {
		e.mu.Unlock()
		return agent.Receipt{}, errors.New("session is closed or faulted")
	}
	if err := validate(op, e.state.Capabilities); err != nil {
		e.mu.Unlock()
		return agent.Receipt{}, err
	}
	if len(e.ops) >= 4096 || len(e.queue) >= 64 {
		e.mu.Unlock()
		return agent.Receipt{}, errors.New("session operation/queue capacity exhausted")
	}
	if op.Kind == agent.Answer || op.Kind == agent.Approve {
		pending, ok := e.state.Pending[op.RequestID]
		if !ok || e.responses[op.RequestID] || (op.Kind == agent.Answer && pending.Type != "question") || (op.Kind == agent.Approve && pending.Type != "approval") {
			e.mu.Unlock()
			return agent.Receipt{}, errors.New("request is absent, resolved, or of another kind")
		}
	}
	if (op.Kind == agent.Interrupt || op.Kind == agent.Steer) && !e.state.Busy {
		e.mu.Unlock()
		return agent.Receipt{}, errors.New("no active turn")
	}
	r := agent.Receipt{ID: op.ID, Delivery: "accepted"}
	if op.Kind == agent.Message && e.state.Title == "" {
		title := []rune(strings.TrimSpace(strings.SplitN(op.Text, "\n", 2)[0]))
		if len(title) > 80 {
			title = title[:80]
		}
		e.state.Title = string(title)
	}
	// Do not retain image bytes or credentials in journal events.
	record := map[string]any{"type": "operation_accepted", "operationId": op.ID, "kind": op.Kind, "text": op.Text, "imageCount": len(op.Images), "requestId": op.RequestID, "data": map[string]any{"files": op.Files}}
	// A response's decision and answers are journaled so a request redelivered
	// after a restart can be answered as the user did.
	if op.Decision != "" {
		record["decision"] = op.Decision
	}
	if len(op.Answers) > 0 {
		record["answers"] = op.Answers
	}
	if !e.appendLocked(record) {
		e.mu.Unlock()
		return r, errors.New("journal unavailable")
	}
	e.ops[op.ID] = accepted{fingerprint: fingerprint, receipt: r}
	if op.Kind == agent.Message {
		e.queue = append(e.queue, op)
		e.persistDurableLocked()
		e.mu.Unlock()
		go e.dispatch()
		return r, nil
	}
	if op.Kind == agent.Answer || op.Kind == agent.Approve {
		e.responses[op.RequestID] = true
	}
	e.mu.Unlock()
	r, err := e.provider.Apply(e.ctx, op)
	r.ID = op.ID
	if err != nil {
		r.Delivery = "failed"
		r.Error = err.Error()
	}
	e.mu.Lock()
	e.ops[op.ID] = accepted{fingerprint: fingerprint, receipt: r}
	if errors.Is(err, agent.ErrUnsupported) {
		delete(e.responses, op.RequestID)
	}
	e.appendLocked(map[string]any{"type": "receipt", "receipt": r})
	// A local submission is not request resolution: only the adapter's
	// correlated event can remove the pending request from the registry.
	e.mu.Unlock()
	return r, err
}

func (e *Engine) dispatch() {
	e.opMu.Lock()
	defer e.opMu.Unlock()
	e.mu.Lock()
	if e.state.Busy || e.state.Closed || e.state.Fault != "" || len(e.queue) == 0 {
		e.mu.Unlock()
		return
	}
	op := e.queue[0]
	e.queue = e.queue[1:]
	e.persistDurableLocked()
	e.state.Busy = true
	e.observe(agent.Event{Type: "agent_status", Status: string(agent.Starting)})
	if op.Kind == agent.Answer || op.Kind == agent.Approve {
		e.responses[op.RequestID] = true
	}
	e.mu.Unlock()
	r, err := e.provider.Apply(e.ctx, op)
	r.ID = op.ID
	if err != nil {
		r.Delivery = "failed"
		r.Error = err.Error()
	}
	e.mu.Lock()
	if e.detaching {
		// The receipt is unknowable now; the adopting Runner records that.
		e.mu.Unlock()
		return
	}
	old := e.ops[op.ID]
	old.receipt = r
	e.ops[op.ID] = old
	e.appendLocked(map[string]any{"type": "receipt", "receipt": r})
	if err != nil {
		e.state.Busy = false
		e.state.Fault = "provider delivery failed; reconcile before continuing"
		e.observe(agent.Event{Type: "failure", Status: "delivery_failed"})
	}
	e.mu.Unlock()
}

func (e *Engine) appendLocked(value any) bool {
	if e.detaching {
		// The journal belongs to the next Runner now. Never fault or stop the
		// provider for a write that arrives after detaching.
		return false
	}
	_, err := e.journal.Append(value)
	if err == nil {
		err = e.persistLocked()
	}
	if err != nil {
		e.state.Fault = fmt.Sprintf("event capture failed: %v", err)
		e.cancel()
		go e.provider.Close()
		return false
	}
	return true
}

// endReplayLocked forgets what the previous Runner recorded of a line once
// that line is complete, or once a profile switch starts a new native session
// whose line numbers restart: a leftover must never match a new event.
func (e *Engine) endReplayLocked(ev agent.Event) {
	if e.replayed != nil && (ev.Type == "profile_changed" || (!ev.Partial && ev.Cursor > e.replayedAfter)) {
		e.replayed = nil
	}
}

func (e *Engine) receive() {
	defer close(e.done)
	for {
		ev, err := e.provider.Receive(e.ctx)
		if err != nil {
			e.mu.Lock()
			if !e.state.Closed && !e.detaching && e.ctx.Err() == nil {
				e.state.Fault = fmt.Sprintf("provider stream ended: %v", err)
				exit := agent.Event{Type: "process_exit", Status: "unknown", Text: err.Error()}
				e.observe(exit)
				e.appendLocked(exit)
			}
			e.mu.Unlock()
			return
		}
		e.mu.Lock()
		if key := agent.ReplayKey(ev); ev.Cursor != 0 && e.replayed[key] > 0 {
			e.replayed[key]--
			e.endReplayLocked(ev)
			e.mu.Unlock()
			e.commit(ev)
			continue
		}
		e.endReplayLocked(ev)
		rootEvent := ev.AgentID == "" || ev.AgentID == "root"
		if ev.SessionID != "" && rootEvent {
			e.state.Reference = ev.SessionID
		}
		switch ev.Type {
		case "profile_switching":
			for id, pending := range e.state.Pending {
				e.appendLocked(agent.Event{Type: "request_resolved", RequestID: id, AgentID: pending.AgentID, Status: "profile_switched"})
				delete(e.state.Pending, id)
			}
		case "turn_started":
			if rootEvent {
				e.state.Busy = true
			}
		case "turn_ended":
			if !rootEvent {
				break
			}
			e.state.Busy = false
			for id, pending := range e.state.Pending {
				if pending.AgentID == "" || pending.AgentID == "root" {
					e.appendLocked(agent.Event{Type: "request_resolved", RequestID: id, Status: "turn_ended"})
					delete(e.state.Pending, id)
				}
			}
		case "approval", "question":
			if _, known := e.state.Pending[ev.RequestID]; known && ev.RequestID != "" {
				// A request redelivered after re-attachment is already recorded.
				e.mu.Unlock()
				e.commit(ev)
				continue
			}
			if ev.RequestID == "" || len(e.state.Pending) >= 64 {
				e.state.Fault = "invalid or excessive pending provider requests"
				e.cancel()
			} else {
				e.state.Pending[ev.RequestID] = ev
			}
		case "request_resolved":
			if pending, ok := e.state.Pending[ev.RequestID]; ok {
				ev.AgentID = pending.AgentID
			}
			delete(e.state.Pending, ev.RequestID)
		case "failure":
			if !rootEvent {
				break
			}
			if e.state.Fault == "" || (ev.Status != "unknown" && ev.Status != "provider_error" && ev.Status != "") {
				e.state.Fault = ev.Status
			}
			if e.state.Fault == "" {
				e.state.Fault = "provider_failure"
			}
		}
		e.observe(ev)
		ok := e.appendLocked(ev)
		e.mu.Unlock()
		if !ok {
			return
		}
		e.commit(ev)
		if ev.Type == "turn_ended" && rootEvent {
			go e.dispatch()
		}
	}
}

func (e *Engine) Events(ctx context.Context, cursor uint64) (journal.Record, error) {
	return e.journal.Next(ctx, cursor)
}

func (e *Engine) Close() error {
	e.mu.Lock()
	detached := e.detaching
	e.mu.Unlock()
	if detached {
		// The native process belongs to whichever Runner adopts it next.
		return e.detachErr
	}
	e.closeOnce.Do(func() {
		e.mu.Lock()
		e.state.Closed = true
		previousFault := e.state.Fault
		e.state.Fault = "native_exit_unconfirmed"
		e.mu.Unlock()
		e.closeErr = e.provider.Close()
		e.cancel()
		<-e.done
		// Provider calls have the canceled execution context now. Let their
		// final receipts settle before closing the journal under its readers.
		e.opMu.Lock()
		defer e.opMu.Unlock()
		e.mu.Lock()
		for _, op := range e.queue {
			e.appendLocked(agent.Event{Type: "receipt", OperationID: op.ID, Status: "canceled_before_delivery"})
		}
		e.queue = nil
		if e.closeErr == nil {
			e.state.Fault = previousFault
			e.observe(agent.Event{Type: "session_closed"})
		} else {
			e.observe(agent.Event{Type: "process_exit", Status: "unknown"})
		}
		e.appendLocked(agent.Event{Type: "session_closed", Status: "connection_closed_pending_requests_not_restored"})
		e.mu.Unlock()
		_ = e.journal.Close()
	})
	return e.closeErr
}

// Settings returns only the provider-owned execution policy, never credentials.
func (e *Engine) Settings() json.RawMessage {
	if runtime, ok := e.provider.(agent.RuntimeSettings); ok {
		return runtime.SessionSettings()
	}
	return nil
}
func (e *Engine) persistLocked() error {
	if e.capture != nil {
		if err := e.capture(e.snapshotLocked(), e.Settings()); err != nil {
			return err
		}
	}
	return e.persistDurableLocked()
}
