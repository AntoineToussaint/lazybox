package accounts

import (
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"sort"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// State retains provider identity and availability metadata; authentication stays in provider
// credential stores or private token files, never in this JSON document.
type State struct {
	mu           sync.Mutex
	path         string
	observations map[string]agent.AccountObservation
}

func OpenState(path string) (*State, error) {
	s := &State{path: path, observations: map[string]agent.AccountObservation{}}
	data, err := os.ReadFile(path)
	if err != nil && !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	if err == nil {
		if err = json.Unmarshal(data, &s.observations); err != nil {
			return nil, err
		}
	}
	return s, nil
}
func (s *State) Observe(id string, observation agent.AccountObservation) error {
	if observation.State != "available" && observation.State != "exhausted" && observation.State != "throttled" && observation.State != "auth_required" && observation.State != "billing_error" {
		return nil
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if observation.State == "throttled" && observation.ResetAt == 0 {
		delay := observation.RetryAfterSeconds
		if delay < 1 {
			delay = 60
		}
		observation.ResetAt = time.Now().Unix() + delay
	}
	next := make(map[string]agent.AccountObservation, len(s.observations)+1)
	for id, value := range s.observations {
		next[id] = value
	}
	// A native quota error often omits a reset already supplied by the
	// provider's rate-limit snapshot. Preserve that stronger evidence.
	if old, ok := s.observations[id]; ok && observation.State == "exhausted" && observation.ResetAt == 0 && old.State == "exhausted" && observation.Reason != "spend_control" {
		observation.ResetAt = old.ResetAt
	}
	if observation.ObservedAt == 0 {
		observation.ObservedAt = time.Now().UnixMilli()
	}
	old := s.observations[id]
	if observation.Email == "" {
		observation.Email = old.Email
	}
	windows := make(map[string]agent.UsageWindow)
	for _, window := range old.Windows {
		windows[window.Name] = window
	}
	for _, window := range observation.Windows {
		windows[window.Name] = window
	}
	observation.Windows = nil
	for _, window := range windows {
		if len(observation.Windows) < 32 {
			observation.Windows = append(observation.Windows, window)
		}
	}
	sort.Slice(observation.Windows, func(i, j int) bool { return observation.Windows[i].Name < observation.Windows[j].Name })
	if observation.Plan == "" {
		observation.Plan = old.Plan
	}
	next[id] = observation
	return s.save(next)
}

// ErrNotLimited is a Clear on an account with no limit on record.
var ErrNotLimited = errors.New("no limit is recorded for this subscription")

// Clear forgets a recorded usage limit: the owner says the account can take
// work again (its reset passed, or the provider reset it early). The account
// reads available until a provider reports otherwise; windows at 100% are
// dropped, since their numbers are what the owner overrode. Sign-in and
// billing problems are not limits and stay.
func (s *State) Clear(id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	old, ok := s.observations[id]
	full := false
	for _, window := range old.Windows {
		full = full || window.UsedPercent >= 100
	}
	if !ok || (old.State != "exhausted" && old.State != "throttled" && !(old.State == "available" && full)) {
		return ErrNotLimited
	}
	cleared := agent.AccountObservation{State: "available", Email: old.Email, Plan: old.Plan, ObservedAt: time.Now().UnixMilli()}
	for _, window := range old.Windows {
		if window.UsedPercent < 100 {
			cleared.Windows = append(cleared.Windows, window)
		}
	}
	next := make(map[string]agent.AccountObservation, len(s.observations))
	for key, value := range s.observations {
		next[key] = value
	}
	next[id] = cleared
	return s.save(next)
}

// save writes the observations durably, then adopts them; s.mu is held.
func (s *State) save(next map[string]agent.AccountObservation) error {
	data, err := json.Marshal(next)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(s.path), 0700); err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(s.path), ".availability-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	err = errors.Join(err, f.Close())
	if err != nil {
		return err
	}
	if err = os.Rename(f.Name(), s.path); err != nil {
		return err
	}
	dir, err := os.Open(filepath.Dir(s.path))
	if err != nil {
		return err
	}
	err = errors.Join(dir.Sync(), dir.Close())
	if err != nil {
		return err
	}
	s.observations = next
	return nil
}
func (s *State) Observation(id string) agent.AccountObservation {
	s.mu.Lock()
	defer s.mu.Unlock()
	value, ok := s.observations[id]
	if !ok {
		return agent.AccountObservation{State: "unknown"}
	}
	value.Windows = append([]agent.UsageWindow(nil), value.Windows...)
	return value
}

// Capacity is the account's standing for automatic rotation.
type Capacity uint8

const (
	Unavailable Capacity = iota
	ProbeAllowed
	CapacityReported
)

// RotationCapacity considers both the account status and its usage windows.
// An elapsed reset permits a probe because the provider has yet to confirm capacity.
func (s *State) RotationCapacity(id string, now time.Time) Capacity {
	s.mu.Lock()
	defer s.mu.Unlock()
	value, ok := s.observations[id]
	if !ok {
		return ProbeAllowed
	}
	unix := now.Unix()
	capacity := CapacityReported
	switch value.State {
	case "available":
	case "exhausted", "throttled":
		if value.ResetAt <= 0 || value.ResetAt > unix {
			return Unavailable
		}
		capacity = ProbeAllowed
	default:
		return Unavailable
	}
	for _, window := range value.Windows {
		if window.UsedPercent >= 100 {
			if window.ResetAt <= 0 || window.ResetAt > unix {
				return Unavailable
			}
			capacity = ProbeAllowed
		}
	}
	return capacity
}

// Eligible reports whether automatic rotation may try the account.
func (s *State) Eligible(id string, now time.Time) bool {
	return s.RotationCapacity(id, now) != Unavailable
}
