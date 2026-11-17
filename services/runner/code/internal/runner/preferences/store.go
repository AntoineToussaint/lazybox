// Package preferences owns mutable Runner preferences. It never edits provider
// configuration, credential stores, or settings of an already-running attempt.
package preferences

import (
	"encoding/json"
	"errors"
	"fmt"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
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
	// ProcessBackend hosts new agents, commands and terminals: "sessionhost" or
	// "tmux"; "" keeps the Runner configuration's.
	ProcessBackend string `json:"processBackend,omitempty"`
	// RotationExcluded lists subscriptions left out of automatic switching.
	RotationExcluded []string `json:"rotationExcluded,omitempty"`
	// ClaudeAIConnectors lets new Claude sessions load the person's claude.ai
	// connectors. Off by default, so documents stay in Lazybox artifacts.
	ClaudeAIConnectors bool `json:"claudeAiConnectors,omitempty"`
	// PlainTitles keeps conversations titled by their first message: no
	// title is generated after the first turn. Off by default.
	PlainTitles bool `json:"plainTitles,omitempty"`
	// ClaudeSubscriptionSignIn lets Lazybox offer a Claude subscription
	// sign-in and run conversations on the subscriptions saved that way. Off
	// by default (CLAUDE_COMPLIANCE.md).
	ClaudeSubscriptionSignIn bool `json:"claudeSubscriptionSignIn,omitempty"`
	// ClaudeSignInCarriedOver: the migration turned ClaudeSubscriptionSignIn on
	// because Claude subscriptions were already saved in Lazybox.
	ClaudeSignInCarriedOver bool `json:"claudeSignInCarriedOver,omitempty"`
	// Notices are one-time notices not dismissed yet, by id.
	Notices []string `json:"notices,omitempty"`
	// Migration is the last one-time migration of these settings that ran.
	Migration int `json:"migration,omitempty"`

	// Review is agent review (GITHUB.md, "Agent review").
	Review Review `json:"review,omitzero"`
}

// Review is how agent reviews start: the default reviewer ("" the other
// provider than the reviewed conversation's, else a profile id), the brief
// template ("" the built-in one) and whether a turn that changed files starts
// one on its own.
type Review struct {
	Reviewer  string `json:"reviewer,omitempty"`
	Template  string `json:"template,omitempty"`
	AutoStart bool   `json:"autoStart,omitempty"`
	// Loop is how findings reach the author between rounds: "ask" (the
	// person sends them; "" too), "auto" or "off". MaxRounds: 0 is 3.
	Loop      string `json:"loop,omitempty"`
	MaxRounds int    `json:"maxRounds,omitempty"`
	// ReviewerMode is who reviews by default: "other" (the other agent than
	// the author's), "same" (the author's agent, a fresh conversation) or
	// "fixed" (Reviewer, a profile id or a provider, on ReviewerModel). ""
	// is "fixed" when Reviewer is set, else "other" (ReviewerModeOf).
	ReviewerMode  string `json:"reviewerMode,omitempty"`
	ReviewerModel string `json:"reviewerModel,omitempty"`
}

// Reviewer modes.
const (
	ReviewerOther = "other"
	ReviewerSame  = "same"
	ReviewerFixed = "fixed"
)

// ReviewerModeOf is the mode a review setting means, an older one without a
// mode included: a reviewer named is a fixed one.
func (r Review) ReviewerModeOf() string {
	if r.ReviewerMode != "" {
		return r.ReviewerMode
	}
	if r.Reviewer != "" {
		return ReviewerFixed
	}
	return ReviewerOther
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
	if d.ProcessBackend != "" && d.ProcessBackend != "sessionhost" && d.ProcessBackend != "tmux" {
		return errors.New("invalid process backend")
	}
	if len(d.Context)+len(d.ClaudeContext)+len(d.CodexContext) > 64<<10 {
		return errors.New("agent context exceeds 64 KiB")
	}
	if len(d.Review.Reviewer) > 128 || strings.ContainsAny(d.Review.Reviewer, " \t\r\n") {
		return errors.New("invalid default reviewer")
	}
	if m := d.Review.ReviewerMode; m != "" && m != ReviewerOther && m != ReviewerSame && m != ReviewerFixed {
		return errors.New("the default reviewer is other, same or fixed")
	}
	if d.Review.ReviewerMode == ReviewerFixed && d.Review.Reviewer == "" {
		return errors.New("choose the agent that reviews")
	}
	if len(d.Review.ReviewerModel) > 128 || strings.ContainsAny(d.Review.ReviewerModel, " \t\r\n") {
		return errors.New("invalid reviewer model")
	}
	if d.Review.Loop != "" && d.Review.Loop != "ask" && d.Review.Loop != "auto" && d.Review.Loop != "off" {
		return errors.New("the review loop is ask, auto or off")
	}
	if d.Review.MaxRounds < 0 || d.Review.MaxRounds > 10 {
		return errors.New("a review loop runs 1 to 10 rounds")
	}
	if len(d.Review.Template) > 32<<10 {
		return errors.New("the review brief template is limited to 32 KiB")
	}
	if len(d.Notices) > 16 {
		return errors.New("at most 16 notices")
	}
	for _, id := range d.Notices {
		if id == "" || len(id) > 64 {
			return errors.New("invalid notice")
		}
	}
	if len(d.RotationExcluded) > 256 {
		return errors.New("at most 256 subscriptions excluded from switching")
	}
	for _, id := range d.RotationExcluded {
		if id == "" || len(id) > 128 {
			return errors.New("invalid subscription excluded from switching")
		}
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
	d.RotationExcluded = append([]string(nil), d.RotationExcluded...)
	d.Notices = append([]string(nil), d.Notices...)
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

// Migrate records that one-time migration n ran, after apply changed what it
// carries over and added notices for the person. It changes the revision only
// when something a client sees changed, so a fresh Runner's settings stay at
// their first revision.
func (s *Store) Migrate(n int, apply func(*Data)) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.data.Migration >= n {
		return nil
	}
	next := clone(s.data)
	if apply != nil {
		apply(&next)
	}
	next.Migration, next.Revision = n, s.data.Revision
	seen := clone(s.data)
	seen.Migration = n
	if a, b := comparable(next), comparable(seen); a != b {
		next.Revision++
	}
	return s.commit(next)
}

// comparable is d as a client would compare it.
func comparable(d Data) string { data, _ := json.Marshal(d); return string(data) }

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
