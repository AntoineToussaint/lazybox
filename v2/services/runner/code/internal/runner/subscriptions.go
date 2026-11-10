package runner

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"
	"unicode"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/claude"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/adapters/codex"
)

type NativeAccount interface {
	Login(context.Context) (accounts.Login, error)
	WaitLogin(context.Context, string) error
	Usage(context.Context) (*agent.AccountObservation, error)
	Close() error
}
type Enrollment struct {
	ID          string
	State       string
	ProfileID   string
	Provider    string
	Label       string
	URL         string
	UserCode    string
	Error       string
	fingerprint [32]byte
	cancel      context.CancelFunc
}

func (b *Bench) accountClient(ctx context.Context, p accounts.Profile) (NativeAccount, error) {
	if b.AccountFactory != nil {
		return b.AccountFactory(ctx, p)
	}
	env, err := accounts.Resolve(p, os.Environ())
	if err != nil {
		return nil, err
	}
	if p.Provider == "claude" {
		return claude.OpenAccount(ctx, b.Config.ClaudeBinary, env, p.Home)
	}
	return codex.OpenAccount(ctx, b.Config.CodexBinary, env, b.Config.State, p.Isolation == "environment")
}

// AddSubscription starts an idempotent one-time enrollment. Credentials and
// OAuth links never enter execution journals or saved client layout state.
func (b *Bench) AddSubscription(key, provider, label, token string) (Enrollment, error) {
	if err := b.ReloadProfiles(); err != nil {
		return Enrollment{}, err
	}
	label = strings.TrimSpace(label)
	if key == "" || len(key) > 128 || len(label) == 0 || len(label) > 64 || strings.IndexFunc(label, unicode.IsControl) >= 0 || (provider != "claude" && provider != "codex") {
		return Enrollment{}, errors.New("request key, Claude/Codex provider and a label (max 64 bytes) required")
	}
	if (provider == "codex" && token != "") || len(token) > 16384 {
		return Enrollment{}, errors.New("Only Claude supports optional token import; use native sign-in otherwise")
	}
	id := subscriptionID(provider, key)
	fingerprint := sha256.Sum256([]byte(provider + "\x00" + label + "\x00" + token))
	b.mu.Lock()
	if b.closed {
		b.mu.Unlock()
		return Enrollment{}, errors.New("Runner closed")
	}
	if old := b.enrollments[id]; old != nil {
		copy := *old
		b.mu.Unlock()
		if copy.fingerprint != fingerprint {
			return Enrollment{}, errors.New("enrollment key reused with different input")
		}
		return copy, nil
	}
	if len(b.enrollments) >= 64 {
		b.mu.Unlock()
		return Enrollment{}, errors.New("enrollment capacity reached")
	}
	for _, p := range b.Profiles() {
		if p.ID == id {
			b.mu.Unlock()
			if p.Provider != provider || p.Label != label {
				return Enrollment{}, errors.New("enrollment identity already exists")
			}
			return Enrollment{ID: id, State: "added", ProfileID: id, Provider: provider, Label: label}, nil
		}
	}
	ctx, cancel := context.WithTimeout(b.ctx, 10*time.Minute)
	flow := &Enrollment{ID: id, State: "starting", ProfileID: id, Provider: provider, Label: label, fingerprint: fingerprint, cancel: cancel}
	b.enrollments[id] = flow
	b.starts.Add(1)
	copy := *flow
	b.mu.Unlock()
	go b.enrollSubscription(ctx, flow, token)
	return copy, nil
}
func (b *Bench) enrollSubscription(ctx context.Context, flow *Enrollment, token string) {
	defer b.starts.Done()
	defer flow.cancel()
	finish := func(state, message string) {
		b.mu.Lock()
		flow.State = state
		flow.Error = message
		flow.URL = ""
		flow.UserCode = ""
		b.mu.Unlock()
	}
	if flow.Provider == "claude" && token != "" {
		if ctx.Err() != nil {
			finish("cancelled", "")
			return
		}
		_, err := accounts.Enroll(ctx, b.Config.SubscriptionState, flow.ProfileID, "claude", "", strings.NewReader(token), nil, io.Discard, io.Discard, flow.Label)
		token = ""
		if err != nil {
			finish("failed", "Could not save this subscription. Existing profiles were not changed.")
			return
		}
		if err = b.ReloadProfiles(); err != nil {
			finish("failed", "Subscription saved, but the account registry could not reload.")
			return
		}
		finish("added", "")
		return
	}
	p, err := accounts.PrepareLogin(b.Config.SubscriptionState, flow.ProfileID, flow.Provider, flow.Label)
	if err != nil {
		finish("failed", "Could not create private account storage.")
		return
	}
	dir := filepath.Dir(p.Home)
	committed := false
	exitUnconfirmed := false
	defer func() {
		if !committed && !exitUnconfirmed {
			_ = os.RemoveAll(dir)
		}
	}()
	client, err := b.accountClient(ctx, p)
	if err != nil {
		finish("failed", "Could not start the native account client.")
		return
	}
	defer func() {
		if client.Close() != nil {
			exitUnconfirmed = true
		}
	}()
	login, err := client.Login(ctx)
	if err != nil {
		if errors.Is(err, codex.ErrDeviceAuthDisabled) {
			finish("failed", codex.ErrDeviceAuthDisabled.Error())
		} else {
			finish("failed", "The provider could not start sign-in. Your existing accounts were not changed.")
		}
		return
	}
	b.mu.Lock()
	flow.State = "awaiting_login"
	flow.URL = login.URL
	flow.UserCode = login.Code
	b.mu.Unlock()
	if err = client.WaitLogin(ctx, login.ID); err != nil {
		if ctx.Err() != nil {
			finish("cancelled", "")
		} else if errors.Is(err, codex.ErrDeviceAuthDisabled) {
			finish("failed", codex.ErrDeviceAuthDisabled.Error())
		} else {
			finish("failed", "Provider sign-in did not complete.")
		}
		return
	}
	// Commit and cancellation are serialized; a completed login is never deleted
	// because a late Cancel request arrived while metadata was being persisted.
	b.mu.Lock()
	if ctx.Err() != nil {
		b.mu.Unlock()
		finish("cancelled", "")
		return
	}
	err = accounts.StoreProfile(b.Config.SubscriptionState, p)
	if err != nil {
		if saved, readErr := accounts.LoadProfiles(b.Config.SubscriptionState); readErr == nil {
			for _, registered := range saved {
				if registered.ID == p.ID {
					committed = true
				}
			}
		}
	}
	if err == nil {
		committed = true
		err = b.ReloadProfiles()
	}
	if err != nil {
		b.mu.Unlock()
		finish("failed", "Sign-in completed but account registration failed.")
		return
	}
	flow.State = "added"
	flow.URL = ""
	flow.UserCode = ""
	b.mu.Unlock()
	// Reading quota is optional and never consumes a model turn.
	if p.Provider == "claude" {
		_, _ = b.RefreshSubscription(ctx, p.ID)
		return
	}
	quotaCtx, stopQuota := context.WithTimeout(ctx, 20*time.Second)
	defer stopQuota()
	if observation, err := client.Usage(quotaCtx); err == nil {
		_ = b.Accounts.Observe(p.ID, *observation)
	}
}
func (b *Bench) Enrollment(id string) (Enrollment, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	flow := b.enrollments[id]
	if flow == nil {
		for _, p := range b.Profiles() {
			if p.ID == id {
				return Enrollment{ID: id, State: "added", ProfileID: id, Provider: p.Provider, Label: p.Label}, nil
			}
		}
		return Enrollment{}, os.ErrNotExist
	}
	return *flow, nil
}
func (b *Bench) CancelEnrollment(id string) (Enrollment, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	flow := b.enrollments[id]
	if flow == nil {
		return Enrollment{}, os.ErrNotExist
	}
	if flow.State == "starting" || flow.State == "awaiting_login" {
		flow.cancel()
		flow.State = "cancelling"
		flow.URL = ""
		flow.UserCode = ""
	}
	return *flow, nil
}

// RefreshSubscription reads native quota without creating an agent conversation.
// Both adapters use native account/usage queries; no model prompt is submitted.
func (b *Bench) RefreshSubscription(ctx context.Context, id string) (accounts.Profile, error) {
	if err := b.ReloadProfiles(); err != nil {
		return accounts.Profile{}, err
	}
	var profile accounts.Profile
	for _, p := range b.Profiles() {
		if p.ID == id {
			profile = p
		}
	}
	if profile.ID == "" {
		return profile, os.ErrNotExist
	}
	if !profile.Managed || profile.Isolation != "environment" {
		return profile, errors.New("local CLI sign-ins are read-only references; add a private subscription to refresh quota")
	}
	b.mu.Lock()
	if b.closed || b.probes >= 4 {
		b.mu.Unlock()
		return profile, errors.New("Runner closed or quota refresh capacity reached")
	}
	b.probes++
	b.starts.Add(1)
	b.mu.Unlock()
	defer func() { b.mu.Lock(); b.probes--; b.mu.Unlock(); b.starts.Done() }()
	ctx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	stop := context.AfterFunc(b.ctx, cancel)
	defer stop()
	var observation *agent.AccountObservation
	var err error
	if profile.Provider == "claude" && b.AccountFactory == nil {
		var env []string
		env, err = accounts.Resolve(profile, os.Environ())
		if err == nil {
			observation, err = claude.ReadUsage(ctx, b.Config.Worker, b.Config.ClaudeBinary, env, profile.Home)
		}
	} else {
		var client NativeAccount
		client, err = b.accountClient(ctx, profile)
		if err == nil {
			defer client.Close()
			observation, err = client.Usage(ctx)
		}
	}
	if err != nil {
		return profile, errors.New("Provider quota unavailable; retained observations were kept")
	}
	if observation.State == "unknown" && len(observation.Windows) == 0 {
		return profile, errors.New("Provider did not report subscription quota; retained observations were kept")
	}
	if err = b.Accounts.Observe(id, *observation); err != nil {
		return profile, err
	}
	return profile, nil
}

// PendingEnrollments exposes resumable view references, never one-time codes or URLs.
func (b *Bench) PendingEnrollments() []Enrollment {
	b.mu.Lock()
	defer b.mu.Unlock()
	values := []Enrollment{}
	for _, flow := range b.enrollments {
		if flow.State == "starting" || flow.State == "awaiting_login" || flow.State == "cancelling" {
			value := *flow
			value.URL = ""
			value.UserCode = ""
			values = append(values, value)
		}
	}
	sort.Slice(values, func(i, j int) bool { return values[i].ID < values[j].ID })
	return values
}

func subscriptionID(provider, key string) string {
	digest := sha256.Sum256([]byte(provider + "\x00" + key))
	return provider + "-" + hex.EncodeToString(digest[:8])
}

// EnrollmentForRequest recovers a lost acknowledgement without retaining a token
// or reproducing the server's enrollment identity algorithm in a client.
func (b *Bench) EnrollmentForRequest(provider, key string) (Enrollment, error) {
	if key == "" || len(key) > 128 || (provider != "codex" && provider != "claude") {
		return Enrollment{}, errors.New("valid provider and enrollment request key required")
	}
	return b.Enrollment(subscriptionID(provider, key))
}
