// Package preferences owns mutable Runner preferences. It never edits provider
// configuration, credential stores, or settings of an already-running attempt.
package preferences

import (
	"encoding/json"
	"errors"
	"fmt"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

const DefaultContext = "You are running inside Lazybox. The Runner owns your execution; closing a chat window does not stop your work. A workspace may contain several repository worktrees shared by multiple conversations. Use the supplied workspace facts to locate repositories, read their applicable agent instructions, and respect the resolved work policies. Keep changes scoped to the requested task and report what you verified. Lazybox displays tool approvals and managed command output; use the exposed tools rather than inventing UI actions."

type Data struct {
	Revision      uint64             `json:"revision"`
	Naming        string             `json:"naming"`
	Permission    string             `json:"permission"`
	Context       string             `json:"context"`
	ClaudeContext string             `json:"claudeContext"`
	CodexContext  string             `json:"codexContext"`
	Prices        telemetry.RateCard `json:"prices"`
}
type Store struct {
	mu   sync.RWMutex
	path string
	data Data
	base telemetry.RateCard
}

var ErrConflict = errors.New("settings changed in another window; reload before saving")

func Open(path string, base telemetry.RateCard) (*Store, error) {
	s := &Store{path: path, base: clone(Data{Prices: base}).Prices, data: Data{Revision: 1, Naming: "repositories", Context: DefaultContext, Prices: telemetry.RateCard{}}}
	raw, err := os.ReadFile(path)
	if err == nil {
		if len(raw) > 1<<20 {
			return nil, errors.New("settings too large")
		}
		if err = json.Unmarshal(raw, &s.data); err != nil {
			return nil, err
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return nil, err
	}
	if err = validate(s.data); err != nil {
		return nil, err
	}
	return s, nil
}
func validate(d Data) error {
	if d.Revision == 0 || (d.Naming != "repositories" && d.Naming != "generated") {
		return errors.New("invalid settings version or naming mode")
	}
	if d.Permission != "" && d.Permission != "ask" && d.Permission != "read-only" && d.Permission != "yolo" {
		return errors.New("invalid default permissions")
	}
	if len(d.Context)+len(d.ClaudeContext)+len(d.CodexContext) > 64<<10 {
		return errors.New("agent context exceeds 64 KiB")
	}
	if len(d.Prices) > 512 {
		return errors.New("at most 512 model price overrides")
	}
	for key, rate := range d.Prices {
		if err := validateRate(key, rate); err != nil {
			return err
		}
	}
	return nil
}
func validateRate(key string, r telemetry.Rate) error {
	provider, model, ok := strings.Cut(key, "/")
	if !ok || (provider != "claude" && provider != "codex") || model == "" || len(model) > 128 || strings.ContainsAny(model, " /\n\r\t") {
		return errors.New("provider and exact model ID required")
	}
	if r.Input <= 0 || r.Output <= 0 || r.Version == "" || len(r.Version) > 128 {
		return errors.New("positive input/output prices and a price version required")
	}
	for _, v := range []int64{r.Input, r.Output, r.CacheRead, r.CacheWrite5m, r.CacheWrite1h, r.MaxInput} {
		if v < 0 || v > 1e9 {
			return errors.New("invalid price or input threshold")
		}
	}
	return nil
}
func clone(d Data) Data {
	copy := telemetry.RateCard{}
	for k, v := range d.Prices {
		copy[k] = v
	}
	d.Prices = copy
	return d
}
func (s *Store) Snapshot() Data { s.mu.RLock(); defer s.mu.RUnlock(); return clone(s.data) }
func (s *Store) SnapshotRates() (Data, telemetry.RateCard) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	rates := telemetry.RateCard{}
	for k, v := range s.base {
		rates[k] = v
	}
	for k, v := range s.data.Prices {
		rates[k] = v
	}
	return clone(s.data), rates
}
func (s *Store) Rates() telemetry.RateCard                 { _, rates := s.SnapshotRates(); return rates }
func (s *Store) Price(r telemetry.Record) telemetry.Record { return s.Rates().Price(r) }
func (s *Store) commit(next Data) error {
	if err := validate(next); err != nil {
		return err
	}
	raw, err := json.Marshal(next)
	if err != nil {
		return err
	}
	if err = os.MkdirAll(filepath.Dir(s.path), 0700); err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(s.path), ".settings-")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	_, err = f.Write(raw)
	if err == nil {
		err = f.Sync()
	}
	err = errors.Join(err, f.Close())
	if err == nil {
		err = os.Rename(f.Name(), s.path)
	}
	if err != nil {
		return err
	}
	s.data = next
	return nil
}
func (s *Store) Save(next Data) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if next.Revision != s.data.Revision {
		return ErrConflict
	}
	next.Prices = clone(s.data).Prices
	next.Revision++
	return s.commit(next)
}
func (s *Store) SetRate(revision uint64, key string, rate telemetry.Rate, remove bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if revision != s.data.Revision {
		return ErrConflict
	}
	next := clone(s.data)
	next.Revision++
	if remove {
		delete(next.Prices, key)
	} else {
		rate.Version = fmt.Sprintf("custom-%s-r%d", time.Now().UTC().Format("2006-01-02"), next.Revision)
		if err := validateRate(key, rate); err != nil {
			return err
		}
		next.Prices[key] = rate
	}
	return s.commit(next)
}
