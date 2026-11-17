package runner

import (
	"context"
	"errors"
	"slices"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/preferences"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// liveEngine is the running engine of e, or an error a person can act on.
func liveEngine(e *Execution) (*engine.Engine, error) {
	live, ok := e.Engine.(*engine.Engine)
	if !ok || e.Archived {
		return nil, errors.New("the conversation has ended; resume it to continue")
	}
	return live, nil
}

// SwitchSubscription continues a running conversation on another signed-in
// subscription of its provider, in the same execution: workspace, commands,
// queued messages and operation receipts stay; the new account starts from the
// portable checkpoint, exactly as automatic rotation does. The conversation's
// current subscription retries the interrupted request in place (after a
// reset). This is the person's explicit choice and needs no account policy;
// enableRotation also turns automatic switching on for it, which does.
func (b *Bench) SwitchSubscription(ctx context.Context, e *Execution, profileID string, enableRotation bool) error {
	live, err := liveEngine(e)
	if err != nil {
		return err
	}
	e.Continuation.Lock()
	defer e.Continuation.Unlock()
	var target accounts.Profile
	for _, p := range b.Profiles() {
		if p.ID == profileID {
			target = p
		}
	}
	if target.ID == "" {
		return errors.New("subscription not found")
	}
	if target.Provider != e.Profile.Provider {
		return errors.New("a conversation can only switch to a subscription of its own provider")
	}
	if err = b.claudeSignInAllowed(target); err != nil {
		return err
	}
	if enableRotation {
		if _, ok := b.AccountPolicy(target.Provider); !ok {
			return ErrNoAccountPolicy
		}
		if err = b.setRotation(ctx, e, live, target, true); err != nil {
			return err
		}
	}
	if err = b.Record(e, map[string]any{"type": "subscription_switch_requested", "profile": profileID}); err != nil {
		return err
	}
	return live.SwitchProfile(profileID)
}

// SetSessionRotation turns automatic switching at a limit on or off for a
// running conversation. With all, it applies to every running conversation of
// the provider that did not opt out at start, and turning it on also includes
// every subscription of the provider in automatic switching (Settings →
// Subscriptions). Turning it on needs an account policy for the provider
// (account_policy.go); turning it off never does.
func (b *Bench) SetSessionRotation(ctx context.Context, e *Execution, enabled, all bool) error {
	live, err := liveEngine(e)
	if err != nil {
		return err
	}
	provider := e.Profile.Provider
	if _, switching := b.AccountPolicy(provider); enabled && !switching {
		return ErrNoAccountPolicy
	}
	if all && enabled {
		if err = b.includeProvider(provider); err != nil {
			return err
		}
	}
	if err = b.setCurrentRotation(ctx, e, live, enabled); err != nil || !all {
		return err
	}
	var errs []error
	for _, other := range b.List() {
		if other == e || other.Profile.Provider != provider || other.Saved.Options.NoRotation {
			continue
		}
		running, err := liveEngine(other)
		if err != nil || running.Snapshot().Closed {
			continue
		}
		errs = append(errs, b.setCurrentRotation(ctx, other, running, enabled))
	}
	return errors.Join(errs...)
}

func (b *Bench) setCurrentRotation(ctx context.Context, e *Execution, live *engine.Engine, enabled bool) error {
	e.Continuation.Lock()
	defer e.Continuation.Unlock()
	id := live.Snapshot().Profile
	for _, p := range b.Profiles() {
		if p.ID == id {
			return b.setRotation(ctx, e, live, p, enabled)
		}
	}
	return errors.New("the conversation's subscription is no longer configured")
}

// setRotation resolves the pool as a new conversation on current would (minus
// current itself), applies it to the live engine and saves it, so a Runner
// that re-attaches or resumes the conversation keeps it. Holds e.Continuation.
func (b *Bench) setRotation(ctx context.Context, e *Execution, live *engine.Engine, current accounts.Profile, enabled bool) error {
	pool := []string{}
	if policy, ok := b.AccountPolicy(current.Provider); enabled && ok {
		if configured := b.Config.Defaults.RotationProfiles; len(configured) > 0 {
			for _, id := range configured {
				if id != current.ID {
					pool = append(pool, id)
				}
			}
		} else {
			pool = policy.Pool(b.Profiles(), current, b.Preferences.Snapshot().RotationExcluded)
		}
	}
	if err := live.SetRotation(pool); err != nil {
		return err
	}
	if err := b.History.Amend(ctx, e.ID, func(r *sessionstore.Record) {
		r.Options.RotationProfiles, r.Options.NoRotation = pool, !enabled
	}); err != nil {
		return err
	}
	e.activity.Lock()
	e.Options.RotationProfiles, e.Options.NoRotation = pool, !enabled
	e.Saved.Options.RotationProfiles, e.Saved.Options.NoRotation = pool, !enabled
	e.activity.Unlock()
	return nil
}

// includeProvider takes every subscription of provider back into automatic
// switching. A concurrent settings save is retried on its new revision.
func (b *Bench) includeProvider(provider string) error {
	ids := map[string]bool{}
	for _, p := range b.Profiles() {
		if p.Provider == provider {
			ids[p.ID] = true
		}
	}
	for range 3 {
		data := b.Preferences.Snapshot()
		kept := slices.DeleteFunc(slices.Clone(data.RotationExcluded), func(id string) bool { return ids[id] })
		if len(kept) == len(data.RotationExcluded) {
			return nil
		}
		data.RotationExcluded = kept
		if err := b.Preferences.Save(data); !errors.Is(err, preferences.ErrConflict) {
			return err
		}
	}
	return preferences.ErrConflict
}
