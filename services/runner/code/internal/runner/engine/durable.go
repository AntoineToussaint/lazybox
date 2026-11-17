package engine

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
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
	responded := map[string]agent.Operation{}
	for id, response := range scan.responded {
		if _, pending := saved.Pending[id]; pending {
			responded[id] = response
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
	// when it is delivered. (One accepted but not queued, without a receipt,
	// may have reached it: the previous Runner died while delivering it.)
	var transcript strings.Builder
	for _, u := range scan.said {
		if u.operation == "" || !queued[u.operation] {
			transcript.WriteString(u.text)
		}
	}
	hosting := *open.Host
	replayed := make(map[string]int, len(scan.partial))
	for key, n := range scan.partial {
		replayed[key] = n
	}
	hosting.Attach = &agent.Attachment{State: record.Provider, Reference: saved.Reference, Cursor: scan.cursor, Responded: responded, Transcript: transcript.String(), Replayed: replayed}
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
	// The previous Runner stopped while delivering a message that never
	// started a turn (it waited for its code checkpoint): the conversation
	// was saved busy, and no turn_ended would ever come. It is not busy; the
	// message's outcome is unknown (its receipt above says so), and what is
	// queued goes next. A turn it did start reports turn_started from the host.
	if e.state.Busy {
		for _, id := range scan.order {
			if scan.startedNothing(id, queued) {
				e.state.Busy = false
				if root, ok := e.state.Agents["root"]; ok && (root.Status == agent.Starting || root.Status == agent.Running) {
					root.Status, root.Phase, root.Tool = agent.Ready, "", ""
					e.state.Agents["root"] = root
				}
				break
			}
		}
	}
	e.replayed, e.replayedAfter = scan.partial, scan.cursor
	// A stop the previous Runner asked for is still the stop that ends the
	// turn in progress: its report is not a provider error here either.
	e.stop.adopted(scan.lastStarted > scan.lastEnded, scan.interrupting != "" && e.state.Busy)
	// The previous Runner lost its stream from the agent, or could not record
	// it (a full disk once ended it so): this is the agent's live process
	// again, so that fault is settled and what the agent reports from here
	// says how it is. An agent that really exited reports its exit again from
	// its host and fails again; provider faults (limits, sign-in) stay.
	e.settleTransportFaultLocked()
	ok, retry := e.recordLocked(agent.Event{Type: "runner_reattached", Status: "native_process_adopted", Cursor: scan.cursor})
	e.mu.Unlock()
	// A disk that refuses the notice does not cost the agent its Runner: the
	// first event it records waits for room (recordAgain).
	if !ok && !retry {
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
	// by agent.ReplayKey: the line is replayed and these must not be recorded twice.
	partial   map[string]int
	order     []string
	kinds     map[string]agent.Kind
	receipts  map[string]agent.Receipt
	responded map[string]agent.Operation
	// said is the conversation in the order the agent saw it: each user
	// message where it was delivered, and the root agent's reply text.
	said []utterance
	// interrupting is the interrupt operation accepted since the main agent's
	// last turn ended, if the provider did not refuse it.
	interrupting string
	// awaiting indexes the user messages in said whose delivery is not
	// recorded yet: they stay where they were accepted until their receipt.
	awaiting map[string]int
	// acceptedAt is where each operation was accepted, and lastStarted and
	// lastEnded where the main agent's last turn started and ended, as
	// record positions counted from 1 (0: never).
	acceptedAt             map[string]int
	lastStarted, lastEnded int
}

// startedNothing reports whether message id was taken from the queue for
// delivery but never started a turn: no receipt says it reached the agent
// and no turn started since it was accepted, or since the last turn ended.
// A Runner that stopped while a message waited for its turn's code
// checkpoint leaves exactly that, with the conversation saved as busy.
func (s *journalScan) startedNothing(id string, queued map[string]bool) bool {
	if s.kinds[id] != agent.Message || queued[id] {
		return false
	}
	if _, ok := s.receipts[id]; ok {
		return false
	}
	return s.lastStarted < max(s.acceptedAt[id], s.lastEnded)
}

type utterance struct {
	operation string // A user message's operation ID; empty for agent text.
	text      string
}

// delivered places a user message where its receipt shows the agent got it,
// as the live conversation does, or drops one the agent never got. An unknown
// outcome leaves it where it was accepted.
func (s *journalScan) delivered(r agent.Receipt) {
	i, ok := s.awaiting[r.ID]
	if !ok {
		return
	}
	switch r.Delivery {
	case "unknown_after_runner_restart":
		return
	case "failed", "canceled_before_delivery":
		s.said[i].text = ""
	default:
		if r.Error != "" {
			s.said[i].text = ""
			break
		}
		s.said = append(s.said, s.said[i])
		s.said[i].text = ""
	}
	delete(s.awaiting, r.ID)
}

// scanJournal reads the execution's recorded facts. The transport cursor only
// counts events from the current native attempt: a profile rotation starts a new
// host whose cursors restart.
func scanJournal(ctx context.Context, spool *journal.Journal) (journalScan, error) {
	s := journalScan{kinds: map[string]agent.Kind{}, receipts: map[string]agent.Receipt{}, responded: map[string]agent.Operation{}, awaiting: map[string]int{}, acceptedAt: map[string]int{}}
	partial := map[uint64]map[string]int{}
	for i, n := uint64(0), spool.Count(); i < n; i++ {
		r, err := spool.Next(ctx, i)
		if err != nil {
			return s, err
		}
		var v struct {
			agent.Event
			OperationID string              `json:"operationId"`
			Kind        agent.Kind          `json:"kind"`
			Receipt     agent.Receipt       `json:"receipt"`
			Decision    string              `json:"decision"`
			Always      string              `json:"always"`
			Reason      string              `json:"reason"`
			Answers     map[string][]string `json:"answers"`
			Operation   struct {
				Files []agent.File `json:"files"`
			} `json:"data"`
		}
		if json.Unmarshal(r.Value, &v) != nil {
			continue
		}
		root := v.AgentID == "" || v.AgentID == "root"
		switch v.Type {
		case "profile_changed":
			s.cursor = 0
			s.interrupting = ""
			partial = map[uint64]map[string]int{}
			s.lastStarted = int(i) + 1
		case "turn_started":
			if root {
				s.lastStarted = int(i) + 1
			}
		case "turn_ended":
			if root {
				s.interrupting = ""
				s.lastEnded = int(i) + 1
			}
		case "operation_accepted":
			if _, seen := s.acceptedAt[v.OperationID]; !seen {
				s.acceptedAt[v.OperationID] = int(i) + 1
			}
			if _, seen := s.kinds[v.OperationID]; !seen {
				s.order = append(s.order, v.OperationID)
			}
			s.kinds[v.OperationID] = v.Kind
			if v.Kind == agent.Interrupt {
				s.interrupting = v.OperationID
			}
			if (v.Kind == agent.Answer || v.Kind == agent.Approve) && v.RequestID != "" {
				s.responded[v.RequestID] = agent.Operation{Kind: v.Kind, RequestID: v.RequestID, Decision: v.Decision, Always: v.Always, Reason: v.Reason, Answers: v.Answers}
			}
			if v.Kind == agent.Message || v.Kind == agent.Steer {
				s.said = append(s.said, utterance{operation: v.OperationID, text: "\nUSER: " + agent.Prompt(agent.Operation{Text: v.Text, Files: v.Operation.Files}) + "\n"})
				s.awaiting[v.OperationID] = len(s.said) - 1
			}
		case "message_delta":
			if v.AgentID == "" || v.AgentID == "root" {
				s.said = append(s.said, utterance{text: v.Text})
			}
		case "receipt":
			r := v.Receipt
			if r.ID == "" && v.OperationID != "" {
				r = agent.Receipt{ID: v.OperationID, Delivery: v.Status}
			}
			if r.ID != "" {
				s.receipts[r.ID] = r
				s.delivered(r)
				if r.ID == s.interrupting && (r.Delivery == "failed" || r.Error != "") {
					s.interrupting = ""
				}
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
			partial[v.Cursor][agent.ReplayKey(v.Event)]++
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
