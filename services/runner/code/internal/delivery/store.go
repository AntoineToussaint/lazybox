package delivery

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"reflect"
	"sync"
	"time"
)

// Binding pins a published spec revision to an ordinary conversation, which
// has no Mission plan to carry one. Each binding of a new revision is a new
// attempt, so proof gathered against one revision never counts for another.
type Binding struct {
	Conversation string   `json:"conversation"`
	Attempt      string   `json:"attempt"`
	Contract     Contract `json:"contract"`
	At           int64    `json:"at"`
}

// record is one journal line: a binding or an observation.
type record struct {
	Binding     *Binding     `json:"binding,omitempty"`
	Observation *Observation `json:"observation,omitempty"`
}

// Store is the Runner's delivery journal: conversation bindings and the
// observations recorded against them or against Mission attempts. Records are
// appended and synced before they are acknowledged; an observation is keyed by
// its ID, so recording the same evidence again (a retry, a replay after a
// restart) leaves one observation.
type Store struct {
	mu           sync.Mutex
	f            *os.File
	bindings     []Binding
	observations []Observation
	byID         map[string]int
	size         int64
	// skipped counts unreadable lines left in the journal; broken stops
	// writing after a failed write could not be undone.
	skipped int
	broken  error
	Now     func() time.Time
}

// OpenStore reads the journal at path, dropping a torn last line that was
// never acknowledged. An unreadable line elsewhere is skipped and counted
// (Skipped), so one damaged record never keeps the Runner from starting.
func OpenStore(path string) (*Store, error) {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return nil, err
	}
	s := &Store{f: f, byID: map[string]int{}, Now: time.Now}
	reader := bufio.NewReader(f)
	var size int64
	for {
		line, err := reader.ReadBytes('\n')
		if errors.Is(err, io.EOF) {
			if len(line) > 0 {
				err = f.Truncate(size)
			} else {
				err = nil
			}
			if err != nil {
				f.Close()
				return nil, err
			}
			break
		}
		if err != nil {
			f.Close()
			return nil, err
		}
		var r record
		size += int64(len(line))
		if err := json.Unmarshal(bytes.TrimSpace(line), &r); err != nil {
			s.skipped++
			continue
		}
		switch {
		case r.Binding != nil:
			s.bindings = append(s.bindings, *r.Binding)
		case r.Observation != nil:
			s.byID[r.Observation.ID] = len(s.observations)
			s.observations = append(s.observations, *r.Observation)
		}
	}
	if _, err := f.Seek(size, io.SeekStart); err != nil {
		f.Close()
		return nil, err
	}
	s.size = size
	return s, nil
}

func (s *Store) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.f.Close()
}

// Skipped is the number of unreadable journal lines OpenStore left out.
func (s *Store) Skipped() int { return s.skipped }

func (s *Store) append(r record) error {
	if s.broken != nil {
		return s.broken
	}
	line, err := json.Marshal(r)
	if err != nil {
		return err
	}
	line = append(line, '\n')
	_, err = s.f.Write(line)
	if err == nil {
		err = s.f.Sync()
	}
	if err != nil {
		// Never leave a partial line for the next record to follow.
		if terr := s.f.Truncate(s.size); terr != nil {
			s.broken = fmt.Errorf("the delivery journal could not be repaired after a failed write (%v); restart the Runner", terr)
		} else if _, serr := s.f.Seek(s.size, io.SeekStart); serr != nil {
			s.broken = fmt.Errorf("the delivery journal could not be repaired after a failed write (%v); restart the Runner", serr)
		}
		return err
	}
	s.size += int64(len(line))
	return nil
}

// Bind pins contract to conversation. Binding the revision already in force
// keeps its attempt; any other revision starts the next attempt.
func (s *Store) Bind(conversation string, contract Contract) (Binding, error) {
	if conversation == "" {
		return Binding{}, errors.New("a binding needs a conversation")
	}
	if !contract.Bound() {
		return Binding{}, errors.New("only a spec.v2 revision has named criteria to bind")
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	count := 0
	var current *Binding
	for i := range s.bindings {
		if s.bindings[i].Conversation == conversation {
			count++
			current = &s.bindings[i]
		}
	}
	if current != nil && current.Contract.Digest == contract.Digest && current.Contract.Artifact == contract.Artifact && current.Contract.Version == contract.Version {
		return *current, nil
	}
	b := Binding{Conversation: conversation, Attempt: fmt.Sprintf("conversation:%s/%d", conversation, count+1), Contract: contract, At: s.Now().UnixMilli()}
	if err := s.append(record{Binding: &b}); err != nil {
		return Binding{}, err
	}
	s.bindings = append(s.bindings, b)
	return b, nil
}

// Binding is the revision in force for conversation.
func (s *Store) Binding(conversation string) (Binding, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	for i := len(s.bindings) - 1; i >= 0; i-- {
		if s.bindings[i].Conversation == conversation {
			return s.bindings[i], true
		}
	}
	return Binding{}, false
}

// ErrConflict refuses a second, different observation under an ID already recorded.
var ErrConflict = errors.New("a different observation is already recorded under this ID")

// Record appends a valid observation once. It reports whether it was new; the
// same observation again is not an error, a different one under its ID is.
func (s *Store) Record(o Observation) (bool, error) {
	if err := o.Validate(); err != nil {
		return false, err
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if i, ok := s.byID[o.ID]; ok {
		if reflect.DeepEqual(s.observations[i], o) {
			return false, nil
		}
		return false, fmt.Errorf("%w: %s", ErrConflict, o.ID)
	}
	if err := s.append(record{Observation: &o}); err != nil {
		return false, err
	}
	s.byID[o.ID] = len(s.observations)
	s.observations = append(s.observations, o)
	return true, nil
}

// Has reports whether an observation is recorded under id.
func (s *Store) Has(id string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	_, ok := s.byID[id]
	return ok
}

// Observations is every recorded observation, in journal order.
func (s *Store) Observations() []Observation {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]Observation(nil), s.observations...)
}
