package engine

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
)

// Attachment is the private record that lets a later Runner re-attach to a live
// execution. The journal remains the record of what happened; this holds only
// what the journal deliberately does not: provider re-attachment state and
// queued messages (with their image bytes) not yet delivered.
type Attachment struct {
	Version  int               `json:"version"`
	Provider json.RawMessage   `json:"provider"`
	Queue    []agent.Operation `json:"queue,omitempty"`
}

// AttachmentVersion is the Attachment format this Runner writes and adopts.
const AttachmentVersion = 1

// ErrNotDetachable means the provider cannot outlive this Runner.
var ErrNotDetachable = errors.New("provider session cannot be detached")

func (e *Engine) persistDurableLocked() error {
	if e.durable == nil || e.detaching {
		return nil
	}
	d, ok := e.provider.(agent.Durable)
	if !ok {
		return nil
	}
	provider := d.DurableState()
	if provider == nil {
		return nil
	}
	record := Attachment{Version: AttachmentVersion, Provider: provider, Queue: e.queue}
	data, err := json.Marshal(record)
	if err != nil {
		return err
	}
	if bytes.Equal(data, e.lastDurable) {
		return nil
	}
	if err = e.durable(record); err != nil {
		return err
	}
	e.lastDurable = data
	return nil
}

// commit tells the provider an event is durably recorded. Never under e.mu.
func (e *Engine) commit(ev agent.Event) {
	if ev.Cursor == 0 {
		return
	}
	if c, ok := e.provider.(agent.Committer); ok {
		c.Commit(ev)
	}
}

// Detachable reports whether this engine's provider can outlive the Runner.
func (e *Engine) Detachable() bool {
	d, ok := e.provider.(agent.Durable)
	if !ok || e.durable == nil {
		return false
	}
	if _, ok = e.provider.(agent.Detacher); !ok {
		return false
	}
	return d.DurableState() != nil
}

// Detach hands the live native process to a later Runner instead of stopping
// it. In-flight provider calls get a bounded chance to settle first so their
// receipts are exact; the snapshot and attachment are recorded last. After
// Detach the engine is inert: Close is a no-op and nothing more is journaled.
func (e *Engine) Detach() error {
	e.detachOnce.Do(func() { e.detachErr = e.detach() })
	return e.detachErr
}

func (e *Engine) detach() error {
	if !e.Detachable() {
		return ErrNotDetachable
	}
	e.mu.Lock()
	closed := e.state.Closed
	e.mu.Unlock()
	if closed {
		return errors.New("execution already closed")
	}
	settled := false
	for deadline := time.Now().Add(5 * time.Second); time.Now().Before(deadline); time.Sleep(10 * time.Millisecond) {
		if e.opMu.TryLock() {
			settled = true
			break
		}
	}
	if settled {
		defer e.opMu.Unlock()
	}
	e.mu.Lock()
	err := e.persistLocked()
	e.detaching = true
	e.mu.Unlock()
	if err != nil {
		e.mu.Lock()
		e.detaching = false
		e.mu.Unlock()
		return fmt.Errorf("could not record re-attachment state: %w", err)
	}
	if err = e.provider.(agent.Detacher).Detach(); err != nil {
		e.mu.Lock()
		e.detaching = false
		e.mu.Unlock()
		return err
	}
	e.cancel()
	<-e.done
	return e.journal.Close()
}

// Adopt re-attaches to a live execution recorded by an earlier Runner. The
// journal (opened for append) is the source of truth for what was already
// recorded; saved is the last persisted snapshot; record is the Attachment.
//
// Operations accepted before the restart keep their receipts. A message the
// old Runner accepted but cannot prove it delivered gets an explicit
// "unknown_after_runner_restart" receipt rather than a silent redelivery.
func Adopt(ctx context.Context, adapter agent.Adapter, open agent.Open, spool *journal.Journal, saved Snapshot, record Attachment, capture func(Snapshot, json.RawMessage) error, durable func(Attachment) error) (*Engine, error) {
	if record.Version != AttachmentVersion || len(record.Provider) == 0 {
		return nil, fmt.Errorf("unsupported attachment record version %d", record.Version)
	}
	if spool.Count() == 0 {
		return nil, errors.New("cannot adopt an execution without its journal")
	}
	if saved.Closed {
		return nil, errors.New("execution already closed")
	}
	scan, err := scanJournal(ctx, spool)
	if err != nil {
		return nil, err
	}
	responded := map[string]bool{}
	for id := range scan.responded {
		if _, pending := saved.Pending[id]; pending {
			responded[id] = true
		}
	}
	if open.Host == nil {
		return nil, errors.New("adoption requires session hosting")
	}
	queued := map[string]bool{}
	for _, op := range record.Queue {
		queued[op.ID] = true
	}
	// A queued message has not reached the agent yet; it joins the transcript
	// when it is delivered.
	var transcript strings.Builder
	for _, u := range scan.said {
		if u.operation == "" || !queued[u.operation] {
			transcript.WriteString(u.text)
		}
	}
	hosting := *open.Host
	hosting.Attach = &agent.Attachment{State: record.Provider, Reference: saved.Reference, Cursor: scan.cursor, Responded: responded, Transcript: transcript.String()}
	open.Host = &hosting
	life, cancel := context.WithCancel(ctx)
	s, err := adapter.Open(life, open)
	if err != nil {
		cancel()
		return nil, err
	}
	e := &Engine{capture: capture, durable: durable, provider: s, journal: spool, ctx: life, cancel: cancel, ops: map[string]accepted{}, responses: map[string]bool{}, done: make(chan struct{}), state: saved}
	if e.state.Pending == nil {
		e.state.Pending = map[string]agent.Event{}
	}
	if e.state.Agents == nil {
		e.state.Agents = map[string]agent.Activity{}
	}
	e.state.Capabilities = s.Capabilities()
	e.queue = append(e.queue, record.Queue...)
	e.mu.Lock()
	for _, id := range scan.order {
		r, ok := scan.receipts[id]
		if !ok && !queued[id] && scan.kinds[id] == agent.Message {
			r = agent.Receipt{ID: id, Delivery: "unknown_after_runner_restart", Error: "the previous Runner stopped before the provider acknowledged this message"}
			e.appendLocked(map[string]any{"type": "receipt", "receipt": r})
		} else if !ok {
			r = agent.Receipt{ID: id, Delivery: "accepted"}
		}
		// Fingerprints are not journaled; a retried ID returns its first receipt.
		e.ops[id] = accepted{receipt: r, adopted: true}
	}
	e.replayed = scan.partial
	ok := e.appendLocked(agent.Event{Type: "runner_reattached", Status: "native_process_adopted", Cursor: scan.cursor})
	e.mu.Unlock()
	if !ok {
		cancel()
		_ = s.(agent.Detacher).Detach()
		return nil, errors.New("journal unavailable during adoption")
	}
	go e.receive()
	if len(e.queue) > 0 && !e.state.Busy {
		go e.dispatch()
	}
	return e, nil
}

type journalScan struct {
	// cursor is the last native line whose every event is recorded.
	cursor uint64
	// partial counts the recorded events of a line recorded only in part,
	// by replayKey: the line is replayed and these must not be recorded twice.
	partial   map[string]int
	order     []string
	kinds     map[string]agent.Kind
	receipts  map[string]agent.Receipt
	responded map[string]bool
	// said is the conversation in journal order: user messages and the root
	// agent's reply text.
	said []utterance
}

type utterance struct {
	operation string // A user message's operation ID; empty for agent text.
	text      string
}

// scanJournal reads the execution's recorded facts. The transport cursor only
// counts events from the current native attempt: a profile rotation starts a new
// host whose cursors restart.
func scanJournal(ctx context.Context, spool *journal.Journal) (journalScan, error) {
	s := journalScan{kinds: map[string]agent.Kind{}, receipts: map[string]agent.Receipt{}, responded: map[string]bool{}}
	partial := map[uint64]map[string]int{}
	for i, n := uint64(0), spool.Count(); i < n; i++ {
		r, err := spool.Next(ctx, i)
		if err != nil {
			return s, err
		}
		var v struct {
			agent.Event
			OperationID string        `json:"operationId"`
			Kind        agent.Kind    `json:"kind"`
			Receipt     agent.Receipt `json:"receipt"`
			Operation   struct {
				Files []agent.File `json:"files"`
			} `json:"data"`
		}
		if json.Unmarshal(r.Value, &v) != nil {
			continue
		}
		switch v.Type {
		case "profile_changed":
			s.cursor = 0
			partial = map[uint64]map[string]int{}
		case "operation_accepted":
			if _, seen := s.kinds[v.OperationID]; !seen {
				s.order = append(s.order, v.OperationID)
			}
			s.kinds[v.OperationID] = v.Kind
			if (v.Kind == agent.Answer || v.Kind == agent.Approve) && v.RequestID != "" {
				s.responded[v.RequestID] = true
			}
			if v.Kind == agent.Message || v.Kind == agent.Steer {
				s.said = append(s.said, utterance{operation: v.OperationID, text: "\nUSER: " + agent.Prompt(agent.Operation{Text: v.Text, Files: v.Operation.Files}) + "\n"})
			}
		case "message_delta":
			if v.AgentID == "" || v.AgentID == "root" {
				s.said = append(s.said, utterance{text: v.Text})
			}
		case "receipt":
			if v.Receipt.ID != "" {
				s.receipts[v.Receipt.ID] = v.Receipt
			} else if v.OperationID != "" {
				s.receipts[v.OperationID] = agent.Receipt{ID: v.OperationID, Delivery: v.Status}
			}
		case "request_resolved":
			delete(s.responded, v.RequestID)
		}
		switch {
		case v.Cursor == 0:
		case v.Partial:
			if partial[v.Cursor] == nil {
				partial[v.Cursor] = map[string]int{}
			}
			partial[v.Cursor][replayKey(v.Event)]++
		case v.Cursor > s.cursor:
			s.cursor = v.Cursor
		}
	}
	s.partial = map[string]int{}
	for cursor, keys := range partial {
		if cursor > s.cursor {
			for key, n := range keys {
				s.partial[key] += n
			}
		}
	}
	return s, nil
}

// replayKey identifies an event derived from a native line by what the provider
// said, not by its position among the line's events, which can differ when
// the line is processed again by a fresh adapter.
func replayKey(ev agent.Event) string {
	return fmt.Sprintf("%d\x00%s\x00%s\x00%s\x00%s\x00%s\x00%s\x00%s", ev.Cursor, ev.Type, ev.RequestID, ev.Status, ev.Name, ev.Text, ev.TurnID, ev.NativeID)
}
