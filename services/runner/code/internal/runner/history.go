package runner

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strings"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// SessionRuntime is independent of history storage and UI transports. A live
// engine (started here or adopted from a session host after a Runner restart)
// and a recovered read-only attempt implement the same inspection API.
type SessionRuntime interface {
	Snapshot() engine.Snapshot
	Apply(context.Context, agent.Operation) (agent.Receipt, error)
	Events(context.Context, uint64) (journal.Record, error)
	Settings() json.RawMessage
	Close() error
}
type archivedRuntime struct {
	state    engine.Snapshot
	spool    *journal.Journal
	settings json.RawMessage
}

func (a *archivedRuntime) Snapshot() engine.Snapshot { return a.state }
func (a *archivedRuntime) Apply(context.Context, agent.Operation) (agent.Receipt, error) {
	return agent.Receipt{}, errors.New("historical attempt is read-only; resume the conversation")
}
func (a *archivedRuntime) Events(ctx context.Context, cursor uint64) (journal.Record, error) {
	return a.spool.Next(ctx, cursor)
}
func (a *archivedRuntime) Settings() json.RawMessage {
	return append(json.RawMessage(nil), a.settings...)
}
func (a *archivedRuntime) Close() error { return nil }
func profileBinding(p accounts.Profile) string {
	// Account location/references are identity; renewable token contents are not.
	p.Label = ""
	data, _ := json.Marshal(p)
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}
func (b *Bench) restore(r sessionstore.Record) (*Execution, error) {
	// Use an immutable read view; recovery must never append to an old execution.
	// A journal that cannot be read in full still leaves the conversation
	// listed, readable up to the damage, and deletable: the fault says why.
	spool, err := b.readJournal(r.ID)
	if err != nil {
		spool = journal.Unreadable(err)
	}
	spool.Close()
	state := r.Snapshot
	uncertain := !state.Closed || state.Fault == "native_exit_unconfirmed"
	for _, c := range b.Commands.List(r.ID) {
		if c.OwnershipUnconfirmed() {
			uncertain = true
		}
	}
	if uncertain {
		state.Status = agent.Interrupted
		state.Fault = ""
	}
	if damage := spool.Damage(); damage != nil {
		state.Fault = fmt.Sprintf("transcript unreadable past cursor %d: %v", spool.Count(), damage)
	}
	state.Closed = true
	state.Busy = false
	state.Queued = 0
	state.QueuedMessages = nil
	// Messages a lost agent never received still wait: resuming delivers them.
	if queue, err := b.loadCarried(r.ID); err == nil && len(queue) > 0 {
		state.Queued, state.QueuedMessages = len(queue), engine.QueuedMessages(queue, nil)
	}
	state.Pending = map[string]agent.Event{}
	for id, a := range state.Agents {
		if a.Status == agent.Running || a.Status == agent.Starting || a.Status == agent.WaitingInput || a.Status == agent.WaitingApproval {
			a.Status = agent.Interrupted
			state.Agents[id] = a
		}
	}
	profile := accounts.Profile{ID: state.Profile, Provider: r.Provider}
	for _, p := range b.Profiles() {
		if p.ID == state.Profile {
			profile = p
		}
	}
	return &Execution{ID: r.ID, WorkID: r.WorkID, Previous: r.Previous, Cwd: r.Cwd, CollectionID: r.CollectionID, Purpose: r.Purpose, Profile: profile, Options: r.Options, spool: spool, Archived: true, RecoveryRequired: uncertain, Saved: r, Engine: &archivedRuntime{state: state, spool: spool, settings: r.ProviderSettings}}, nil
}

// Resume starts one successor, never replays operations or stale approvals.
// It is the fallback for executions Adopt could not re-attach (a reboot, an
// incompatible host). Unclean shutdown still requires explicit reconciliation.
func (b *Bench) Resume(ctx context.Context, id string, processesStopped bool) (*Execution, error) {
	e, err := b.Get(id)
	if err != nil {
		return nil, err
	}
	e.Continuation.Lock()
	defer e.Continuation.Unlock()
	if successor, err := b.History.Successor(ctx, id); err == nil {
		return b.Get(successor.ID)
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	if e.Saved.ResumeBlocked != "" {
		return nil, errors.New(e.Saved.ResumeBlocked)
	}
	if e.RecoveryRequired && !processesStopped {
		return nil, errors.New("previous process ownership is unconfirmed; stop/reconcile its agent and commands before resuming")
	}
	state := e.Engine.Snapshot()
	if live, ok := e.Engine.(*engine.Engine); ok && !e.Archived && !state.Closed && agent.TransportFault(state.Fault) {
		return b.reconnect(ctx, e, live, processesStopped)
	}
	// Its native process exited (the exit was observed): there is nothing to
	// re-attach. Its session is over; the provider's conversation goes on in
	// a new process below, as for any ended session.
	if !e.Archived && !state.Closed && state.Fault == agent.ProcessExitFault {
		if err := e.Engine.Close(); err != nil {
			return nil, fmt.Errorf("could not end the session whose agent exited: %w", err)
		}
		state = e.Engine.Snapshot()
	}
	if !e.Archived && !state.Closed {
		return nil, errors.New("end the existing session before resuming; an active session can be reattached")
	}
	// Its agent's exit could not be confirmed: the person confirms it (they
	// stopped it, or it is gone), as for a conversation that needs recovery.
	if !e.Archived && state.Fault == "native_exit_unconfirmed" && !processesStopped {
		return nil, errors.New("the previous agent's exit is unconfirmed; stop it, then resume confirming it is stopped")
	}
	for _, c := range b.Commands.List(id) {
		if !c.Done {
			return nil, errors.New("finish or cancel owned commands before resuming")
		}
	}
	if state.Reference == "" || !state.Capabilities.Resume {
		return nil, errors.New("provider conversation reference is unavailable; start a new conversation")
	}
	// Reload metadata because the engine may have observed a later account/ref.
	saved, err := b.History.Get(ctx, id)
	if err != nil {
		return nil, err
	}
	var profile accounts.Profile
	for _, p := range b.Profiles() {
		if p.ID == state.Profile {
			profile = p
		}
	}
	if profile.ID == "" || profile.Provider != saved.Provider {
		return nil, errors.New("original account profile is unavailable")
	}
	if saved.ProfileBindings[state.Profile] == "" || profileBinding(profile) != saved.ProfileBindings[state.Profile] {
		return nil, errors.New("saved account configuration changed; reconcile before resuming")
	}
	if saved.Options.Gateway != nil && saved.GatewayID != "metered" {
		found := false
		for _, g := range b.Config.Gateways {
			if g.ID == saved.GatewayID && g.Provider == saved.Provider && g.Gateway == *saved.Options.Gateway {
				found = true
			}
		}
		if !found {
			return nil, errors.New("saved gateway configuration changed or is unavailable")
		}
	}
	// Collection ownership reconciliation remains with its manager. No forceful
	// lease takeover is implied by a resume request.
	next, err := b.open(profile.ID, saved.Cwd, state.Reference, saved.Options, saved.GatewayID, saved.WorkID, id, saved.CollectionID, saved.Purpose, saved.Context, false)
	if err != nil {
		return nil, err
	}
	b.handOverCarried(ctx, id, next)
	return next, nil
}

// handOverCarried delivers to a resumed conversation the messages its lost
// agent never received (agentLost), in the order they were queued, ahead of
// anything sent next. A message the successor refuses stays carried, and
// the rest with it, so the predecessor still lists what was not delivered.
func (b *Bench) handOverCarried(ctx context.Context, id string, next *Execution) {
	queue, err := b.loadCarried(id)
	if err != nil || len(queue) == 0 {
		return
	}
	delivered := 0
	for _, op := range queue {
		if _, err = next.Engine.Apply(ctx, op); err != nil {
			log.Printf("resume %s: queued message %s not delivered: %v", id, op.ID, err)
			break
		}
		delivered++
	}
	if err = b.saveCarried(id, queue[delivered:]); err != nil {
		log.Printf("resume %s: carried messages not updated: %v", id, err)
	}
	// The predecessor's listing is rebuilt from what is still carried.
	b.mu.Lock()
	delete(b.archives, id)
	b.mu.Unlock()
}

// lineage is the execution and its predecessors, newest first.
func (b *Bench) lineage(id string) ([]*Execution, error) {
	chain := []*Execution{}
	seen := map[string]bool{}
	for id != "" {
		if seen[id] || len(chain) >= 10000 {
			return nil, errors.New("invalid execution lineage")
		}
		seen[id] = true
		e, err := b.Get(id)
		if err != nil {
			return nil, err
		}
		chain = append(chain, e)
		id = e.Previous
	}
	return chain, nil
}

// Retained is the last cursor of the timeline Timeline replays for id: the
// records of every attempt in its lineage, numbered contiguously.
func (b *Bench) Retained(id string) (uint64, error) {
	t, err := b.OpenTimeline(id)
	if err != nil {
		return 0, err
	}
	return t.Retained(), nil
}

// Retained is the last cursor of this attempt's own journal.
func (e *Execution) Retained() uint64 { return e.spool.Count() }

// Timeline replays immutable predecessors before the current attempt. Cursor
// numbering remains contiguous for the whole chain; only the last can grow.
func (b *Bench) Timeline(ctx context.Context, id string, cursor uint64) (journal.Record, error) {
	t, err := b.OpenTimeline(id)
	if err != nil {
		return journal.Record{}, err
	}
	return t.Next(ctx, cursor)
}

// importLegacy preserves old transcripts without pretending missing account/
// policy metadata is sufficient for safe native resume. It never edits journals.
func (b *Bench) importLegacy() error {
	entries, err := os.ReadDir(filepath.Join(b.Config.State, "sessions"))
	if err != nil {
		return err
	}
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".jsonl") {
			continue
		}
		id := strings.TrimSuffix(entry.Name(), ".jsonl")
		if deleted, err := b.History.Deleted(b.ctx, id); err != nil {
			return err
		} else if len(deleted) > 0 {
			if err = b.purgeConversation(deleted); err != nil {
				return err
			}
			continue
		}
		if _, err = b.History.Get(b.ctx, id); err == nil {
			continue
		} else if !errors.Is(err, os.ErrNotExist) {
			return err
		}
		data, err := os.ReadFile(filepath.Join(b.Config.State, "sessions", entry.Name()))
		if err != nil {
			return err
		}
		r := sessionstore.Record{ID: id, ConversationID: id, Title: "Earlier conversation", ResumeBlocked: "This earlier transcript lacks complete execution settings; it is available as read-only history."}
		scanner := bufio.NewScanner(bytes.NewReader(data))
		scanner.Buffer(make([]byte, 4096), 64<<20)
		for scanner.Scan() {
			var line journal.Record
			if json.Unmarshal(scanner.Bytes(), &line) != nil {
				r.ResumeBlocked = "Earlier transcript contains an incomplete record"
				break
			}
			if r.CreatedAt.IsZero() {
				r.CreatedAt = line.Time
			}
			r.UpdatedAt = line.Time
			var meta struct {
				Type                                                  string
				Session                                               engine.Snapshot
				WorkID, CollectionID, Cwd, Profile, Model, Permission string
			}
			if json.Unmarshal(line.Value, &meta) != nil {
				continue
			}
			if meta.Type == "session_opened" {
				r.Snapshot = meta.Session
				r.ProfileID = meta.Session.Profile
				r.Purpose = meta.Session.Purpose
			}
			if meta.Type == "execution_context" {
				r.WorkID = meta.WorkID
				r.CollectionID = meta.CollectionID
				r.Cwd = meta.Cwd
				r.Title = filepath.Base(meta.Cwd)
				r.Options = accounts.Options{Model: meta.Model, Permission: meta.Permission}
			}
			var ev agent.Event
			_ = json.Unmarshal(line.Value, &ev)
			if ev.AgentID == "" || ev.AgentID == "root" {
				if ev.SessionID != "" {
					r.Snapshot.Reference = ev.SessionID
				}
				if ev.Type == "profile_changed" {
					r.Snapshot.Profile = ev.Text
				}
			}
			if ev.Type == "session_closed" {
				r.Snapshot.Closed = true
				r.Snapshot.Status = agent.Stopped
			}
		}
		if err = scanner.Err(); err != nil {
			return fmt.Errorf("read legacy transcript: %w", err)
		}
		r.Snapshot.ID = id
		for _, p := range b.Profiles() {
			if p.ID == r.Snapshot.Profile {
				r.Provider = p.Provider
			}
		}
		if r.Provider == "" {
			r.Provider = "unknown"
		}
		if err = b.History.Create(b.ctx, r); err != nil {
			return err
		}
	}
	return nil
}
