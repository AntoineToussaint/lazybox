package work

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// missionLimit bounds one mission's log. Missions are never truncated; a
// mission that reaches it refuses further events (compaction to a snapshot
// is ORCHESTRATION.md's answer, for a later slice).
const missionLimit = 64 << 20

// Store holds every mission's log under one directory, one file per mission,
// and the folded state of each, rebuilt on every append. It is the only
// writer of those files.
type Store struct {
	mu       sync.Mutex
	dir      string
	missions map[string]*mission
	version  atomic.Uint64
	// Now is the clock events are stamped with; tests replace it.
	Now func() time.Time
}

type mission struct {
	path   string
	log    *file
	events []Event
	state  *State
	// err: the log could not be read; the mission is listed, never written.
	err error
}

// Open reads every mission under dir. A mission whose log cannot be read is
// kept and reported, so one damaged file never hides the others.
func Open(dir string) (*Store, error) {
	missions := filepath.Join(dir, "missions")
	if err := os.MkdirAll(missions, 0o700); err != nil {
		return nil, err
	}
	s := &Store{dir: dir, missions: map[string]*mission{}, Now: func() time.Time { return time.Now().UTC() }}
	entries, err := os.ReadDir(missions)
	if err != nil {
		return nil, err
	}
	for _, entry := range entries {
		name := entry.Name()
		if entry.IsDir() || !strings.HasSuffix(name, ".jsonl") {
			continue
		}
		id := strings.TrimSuffix(name, ".jsonl")
		m := &mission{path: filepath.Join(missions, name)}
		m.log, m.events, m.err = openFile(m.path, missionLimit)
		if m.err == nil {
			// Replay leniently: an event a newer Runner wrote stays in the log.
			m.state = Replay(m.events)
			if m.state.ID != id {
				m.err = fmt.Errorf("log %s names mission %q", name, m.state.ID)
			}
		}
		if m.err != nil {
			m.state = &State{ID: id, Title: id}
			if m.log != nil {
				_ = m.log.close()
				m.log = nil
			}
		}
		s.missions[id] = m
	}
	return s, nil
}

// Close releases every log.
func (s *Store) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	var errs []error
	for _, m := range s.missions {
		if m.log != nil {
			errs = append(errs, m.log.close())
			m.log = nil
		}
	}
	return errors.Join(errs...)
}

// Version changes whenever any mission does.
func (s *Store) Version() uint64 { return s.version.Load() }

// Snapshot is one mission as listed: its folded state, or why its log could not be read.
type Snapshot struct {
	State *State
	Err   error
}

// Missions lists every mission, the most recently changed first.
func (s *Store) Missions() []Snapshot {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]Snapshot, 0, len(s.missions))
	for _, m := range s.missions {
		out = append(out, Snapshot{State: m.state, Err: m.err})
	}
	sort.Slice(out, func(i, j int) bool {
		a, b := out[i].State, out[j].State
		if !a.Updated.Equal(b.Updated) {
			return a.Updated.After(b.Updated)
		}
		return a.ID < b.ID
	})
	return out
}

// ErrNotFound: no mission has that id.
var ErrNotFound = os.ErrNotExist

// Mission returns one mission's folded state. The state is never mutated
// afterwards; an append replaces it.
func (s *Store) Mission(id string) (*State, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	m := s.missions[id]
	if m == nil {
		return nil, fmt.Errorf("mission %q: %w", id, ErrNotFound)
	}
	return m.state, m.err
}

// Create starts a mission: mission.created and the process it pins.
func (s *Store) Create(actor Actor, idempotency, title, brief string) (*State, error) {
	title, err := cleanText(title, maxTitle, "title")
	if err != nil {
		return nil, err
	}
	if brief, err = cleanText(brief, maxBrief, "brief"); err != nil {
		return nil, err
	}
	if title == "" {
		return nil, refuse("a mission needs a title")
	}
	if len(idempotency) > maxIdentity {
		return nil, refuse("idempotency key is longer than %d bytes", maxIdentity)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if idempotency != "" {
		for _, m := range s.missions {
			if m.err == nil {
				if _, ok := m.state.Event(idempotency); ok {
					return m.state, nil
				}
			}
		}
	}
	policy, err := PolicyFor(DefaultPreset)
	if err != nil {
		return nil, err
	}
	id := shortID("m")
	for s.missions[id] != nil {
		id = shortID("m")
	}
	m := &mission{path: filepath.Join(s.dir, "missions", id+".jsonl")}
	if m.log, _, err = openFile(m.path, missionLimit); err != nil {
		return nil, err
	}
	m.state = &State{tasks: map[string]*Task{}, goals: map[string]*Goal{}, idem: map[string]int{}}
	_, err = s.appendLocked(m, id, []Draft{
		{Subject: MissionSubject, Type: MissionCreated, Actor: actor, Idempotency: idempotency, Payload: MissionPayload{Title: title, Brief: brief}},
		{Subject: MissionSubject, Type: MissionProcessPinned, Actor: Actor{Kind: ActorLazybox}, Payload: payloadOf(policy)},
	})
	if err != nil {
		_ = m.log.close()
		_ = os.Remove(m.path)
		return nil, err
	}
	s.missions[id] = m
	return m.state, nil
}

// Append validates drafts against the mission's log and writes them as one
// fsynced batch, with the events the mission's process adds by rule (a
// report requests the process's human gate). A batch whose idempotency key
// was already used is not written again: the current state is returned.
// A refused batch writes nothing and says why.
func (s *Store) Append(missionID string, drafts ...Draft) (*State, []Event, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	m := s.missions[missionID]
	if m == nil {
		return nil, nil, fmt.Errorf("mission %q: %w", missionID, ErrNotFound)
	}
	if m.err != nil {
		return nil, nil, fmt.Errorf("mission %q cannot be changed: %w", missionID, m.err)
	}
	for _, d := range drafts {
		if d.Idempotency != "" {
			if _, ok := m.state.Event(d.Idempotency); ok {
				return m.state, nil, nil
			}
		}
	}
	events, err := s.appendLocked(m, missionID, drafts)
	return m.state, events, err
}

func (s *Store) appendLocked(m *mission, missionID string, drafts []Draft) ([]Event, error) {
	at := s.Now().UTC()
	seq := m.state.Seq
	var events []Event
	build := func(d Draft) (Event, error) {
		if _, ok := Versions[d.Type]; !ok {
			return Event{}, refuse("unknown event type %q", d.Type)
		}
		if len(d.Idempotency) > maxIdentity {
			return Event{}, refuse("idempotency key is longer than %d bytes", maxIdentity)
		}
		payload, err := json.Marshal(d.Payload)
		if err != nil {
			return Event{}, err
		}
		seq++
		return Event{ID: newID(at), Seq: seq, At: at, Mission: missionID, Subject: d.Subject, Type: d.Type, V: Versions[d.Type], Actor: d.Actor, CausedBy: d.CausedBy, Idempotency: d.Idempotency, Evidence: d.Evidence, Payload: payload}, nil
	}
	for _, d := range drafts {
		e, err := build(d)
		if err != nil {
			return nil, err
		}
		if e.Type == GrantIssued {
			g, _ := decode[GrantPayload](e)
			for otherID, other := range s.missions {
				if otherID != missionID && other.err == nil && other.state.HeldBy(g.Holder) != nil {
					return nil, refuse("that conversation already orchestrates “%s”; revoke that first", other.state.Title)
				}
			}
		}
		events = append(events, e)
	}
	// Rules of the pinned process, for the events of this batch.
	next, err := Extend(m.events, events)
	if err != nil {
		return nil, err
	}
	if next.Policy != nil && next.Policy.ReportGate() != "" {
		for _, e := range events {
			if e.Type != TaskReported {
				continue
			}
			p, _ := decode[ReportPayload](e)
			rule, err := build(Draft{Subject: e.Subject, Type: GateRequested, Actor: Actor{Kind: ActorRule, Role: next.Policy.Preset()}, CausedBy: e.ID, Idempotency: e.ID + "/gate", Payload: GatePayload{Gate: next.Policy.ReportGate(), Task: p.Task}})
			if err != nil {
				return nil, err
			}
			events = append(events, rule)
		}
		if next, err = Extend(m.events, events); err != nil {
			return nil, err
		}
	}
	if err := m.log.append(events); err != nil {
		if errors.Is(err, ErrFull) {
			return nil, refuse("mission log reached %d MiB", missionLimit>>20)
		}
		return nil, err
	}
	m.events = append(m.events, events...)
	m.state = next
	s.version.Add(1)
	return events, nil
}

// Attachment is a live attempt, for routing evidence and observing liveness.
type Attachment struct {
	Mission string
	Task    string
	Attempt Attempt
}

// Attempts lists every live attempt of every mission.
func (s *Store) Attempts() []Attachment {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.attemptsLocked()
}

func (s *Store) attemptsLocked() []Attachment {
	var out []Attachment
	for id, m := range s.missions {
		if m.err != nil || m.state.Canceled {
			continue
		}
		for _, t := range m.state.Tasks {
			if a := t.Current(); a != nil && !a.Ended && !m.state.terminal(t) {
				out = append(out, Attachment{Mission: id, Task: t.ID, Attempt: *a})
			}
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Mission+out[i].Task < out[j].Mission+out[j].Task })
	return out
}
