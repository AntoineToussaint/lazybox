package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/accounts"
	"github.com/AntoineToussaint/lazybox/v2/internal/agent"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/rotation"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/sessionstore"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/telemetry"
	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

// sessionAdapter builds the provider stack for one execution: subscription
// rotation around metering around the native adapter. Opening and adopting an
// execution must build the identical stack.
func (b *Bench) sessionAdapter(e *Execution, selected accounts.Profile, options accounts.Options, conversationID string) agent.Adapter {
	id, collectionID, purpose, cwd := e.ID, e.CollectionID, e.Purpose, e.Cwd
	native := telemetry.Adapter{Native: b.Adapters[selected.Provider], Store: b.Telemetry, Prices: b.Prices, Gateway: b.Meter, Scope: telemetry.Scope{Workspace: collectionID, Conversation: conversationID, Execution: id, Provider: selected.Provider}}
	return rotation.Adapter{Native: native, Profiles: b.Profiles(), Eligible: options.RotationProfiles, State: b.Accounts,
		Resolve: func(p accounts.Profile, open agent.Open) (agent.Open, error) {
			open.Isolated = p.Isolation == "environment"
			next, err := accounts.Resolve(p, os.Environ())
			if err != nil {
				return open, err
			}
			open.Env = append(next, b.executionEnv(id, collectionID)...)
			pinned := append([]string{}, open.Env...)
			if !agent.IsAssistant(purpose) {
				open.Tools = func(ctx context.Context, name string, args json.RawMessage) (string, error) {
					return b.toolWithEnv(ctx, e, name, args, pinned)
				}
			}
			return open, nil
		}, Evidence: func() string {
			ctx, cancel := context.WithTimeout(b.ctx, 5*time.Second)
			defer cancel()
			facts := map[string]any{"commands": b.Commands.List(id), "cwd": cwd}
			if collectionID != "" {
				revisions, err := b.Collections.Evidence(ctx, collectionID)
				if err != nil {
					facts["sourceEvidenceError"] = "Collection inspection unavailable"
				} else {
					facts["sourceRevisions"] = revisions
				}
			} else {
				facts["sourceEvidenceError"] = "No Collection revision inventory; inspect the current directory before continuing"
			}
			data, _ := json.Marshal(facts)
			return string(data)
		}}
}

func (b *Bench) executionEnv(id, collectionID string) []string {
	env := []string{"LAZYBOX_V2_ENDPOINT=" + b.Config.Endpoint, "LAZYBOX_V2_SESSION_ID=" + id}
	if collectionID != "" {
		env = append(env, "LAZYBOX_V2_COLLECTION_ID="+collectionID)
	}
	return env
}

// sessionOpen resolves the native open request. Every execution runs under a
// session host so a Runner restart never stops its agent.
func (b *Bench) sessionOpen(e *Execution, contextText, resume string) agent.Open {
	open := agent.Open{ID: e.ID, Cwd: e.Cwd, Purpose: e.Purpose, Isolated: e.Profile.Isolation == "environment", Context: contextText, Model: e.Options.Model, Permission: e.Options.Permission, Workflow: e.Options.Workflow, Gateway: e.Options.Gateway, Profile: e.Profile.ID, Env: e.Env, Resume: resume, Host: &agent.Hosting{Root: b.Config.HostRoot}}
	if !agent.IsAssistant(e.Purpose) {
		env := e.Env
		open.Tools = func(ctx context.Context, name string, args json.RawMessage) (string, error) {
			return b.toolWithEnv(ctx, e, name, args, env)
		}
	}
	return open
}

func (b *Bench) attachmentPath(id string) string {
	return filepath.Join(b.Config.State, "sessions", id+".attach.json")
}

// saveAttachment atomically records what a later Runner needs to re-attach. It
// may hold queued message bytes, so it is private like the journal.
func (b *Bench) saveAttachment(id string, a engine.Attachment) error {
	data, err := json.Marshal(a)
	if err != nil {
		return err
	}
	dir := filepath.Join(b.Config.State, "sessions")
	f, err := os.CreateTemp(dir, ".attach-*")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if closeErr := f.Close(); err == nil {
		err = closeErr
	}
	if err != nil {
		return err
	}
	return os.Rename(f.Name(), b.attachmentPath(id))
}

func (b *Bench) loadAttachment(id string) (engine.Attachment, error) {
	var a engine.Attachment
	data, err := os.ReadFile(b.attachmentPath(id))
	if err != nil {
		return a, err
	}
	return a, json.Unmarshal(data, &a)
}

// Adopt re-attaches every execution a previous Runner left running, then stops
// any session host nobody adopted: an agent this Runner cannot supervise must
// not keep writing in a workspace. Call it once, after New and after any
// adapter injection, before serving clients. Failures are returned joined; an
// execution that could not be adopted falls back to the existing interrupted
// state with an explicit resume.
func (b *Bench) Adopt(ctx context.Context) error {
	var errs []error
	for offset := 0; ; offset += 100 {
		records, err := b.History.List(ctx, 100, offset)
		if err != nil {
			return errors.Join(append(errs, err)...)
		}
		for _, r := range records {
			if r.Snapshot.Closed {
				continue
			}
			if _, err = os.Stat(b.attachmentPath(r.ID)); err != nil {
				continue
			}
			if deleted, err := b.History.Deleted(ctx, r.ID); err != nil || len(deleted) > 0 {
				continue
			}
			if err = b.adopt(r); err != nil {
				errs = append(errs, fmt.Errorf("execution %s not re-attached: %w", r.ID, err))
			}
		}
		if len(records) < 100 {
			break
		}
	}
	return errors.Join(append(errs, b.stopOrphanHosts(ctx))...)
}

func (b *Bench) adopt(r sessionstore.Record) error {
	record, err := b.loadAttachment(r.ID)
	if err != nil {
		return err
	}
	var selected accounts.Profile
	for _, p := range b.Profiles() {
		if p.ID == r.ProfileID {
			selected = p
		}
	}
	if selected.ID == "" || selected.Provider != r.Provider {
		return errors.New("original account profile is no longer configured")
	}
	if b.Adapters[selected.Provider] == nil {
		return errors.New("provider adapter unavailable")
	}
	options := r.Options
	// The gateway route is this Runner's; the host proxy is pointed at it.
	options.Gateway = nil
	switch r.GatewayID {
	case "", "direct":
	case "metered":
		options.Gateway = &agent.Gateway{ID: "metered", URL: b.Meter.BaseURL}
	default:
		for _, g := range b.Config.Gateways {
			if g.ID == r.GatewayID && g.Provider == r.Provider {
				gateway := g.Gateway
				options.Gateway = &gateway
			}
		}
		if options.Gateway == nil {
			return errors.New("saved gateway is no longer configured")
		}
	}
	env, err := accounts.Resolve(selected, os.Environ())
	if err != nil {
		return err
	}
	env = append(env, b.executionEnv(r.ID, r.CollectionID)...)
	spool, err := journal.Open(filepath.Join(b.Config.State, "sessions", r.ID+".jsonl"), b.Config.EventBudget)
	if err != nil {
		return err
	}
	e := &Execution{ID: r.ID, WorkID: r.WorkID, Previous: r.Previous, Cwd: r.Cwd, CollectionID: r.CollectionID, Purpose: r.Purpose, Profile: selected, Options: options, Env: env, spool: spool, Saved: r}
	adapter := b.sessionAdapter(e, selected, options, r.ConversationID)
	open := b.sessionOpen(e, r.Context, "")
	open.ProviderSettings = r.ProviderSettings
	eng, err := engine.Adopt(b.ctx, adapter, open, spool, r.Snapshot, record, func(snapshot engine.Snapshot, settings json.RawMessage) error {
		return b.History.SaveState(context.Background(), r.ID, snapshot, settings)
	}, func(a engine.Attachment) error { return b.saveAttachment(r.ID, a) })
	if err != nil {
		_ = spool.Dispose()
		return err
	}
	e.Engine = eng
	b.mu.Lock()
	b.items[r.ID] = e
	b.mu.Unlock()
	if r.CollectionID != "" && b.Collections != nil {
		if err = b.Collections.Adopted(r.CollectionID, r.ID); err != nil {
			return fmt.Errorf("re-attached, but workspace ownership not confirmed: %w", err)
		}
	}
	return nil
}

// StopUnclaimedHosts stops every agent and command host this Runner is not
// attached to, without adopting anything. A Runner whose agents stop with it
// runs it at startup, so a crashed predecessor's agents do not keep running.
func (b *Bench) StopUnclaimedHosts(ctx context.Context) error { return b.stopOrphanHosts(ctx) }

// stopOrphanHosts stops hosts nothing attached to: agents no adopted execution
// claimed and commands the command manager did not adopt. This Runner holds the
// state lock, so no other Runner can be their client.
func (b *Bench) stopOrphanHosts(ctx context.Context) error {
	var errs []error
	for _, root := range []string{b.Config.HostRoot, b.Config.CommandHostRoot} {
		ids, err := sessionhost.Hosts(root)
		if err != nil {
			errs = append(errs, err)
			continue
		}
		for _, id := range ids {
			status, err := sessionhost.Probe(ctx, root, id)
			if sessionhost.Gone(err) {
				// A stale record of a host that died, for example with its machine.
				_ = sessionhost.Forget(root, id)
				continue
			}
			if err != nil {
				// Slow or interrupted, not gone: deleting its directory would stop
				// a program that may be live and adopted.
				errs = append(errs, fmt.Errorf("probe session host %s: %w", id, err))
				continue
			}
			if status.Attached {
				continue
			}
			if err = sessionhost.Stop(ctx, root, id); err != nil {
				errs = append(errs, fmt.Errorf("orphan session host %s: %w", id, err))
			}
		}
	}
	return errors.Join(errs...)
}
