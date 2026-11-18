package runner

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/docstool"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/engine"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/rotation"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/settingstool"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/telemetry"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// sessionAdapter builds the provider stack for one execution: subscription
// rotation around metering around the native adapter. Opening and adopting an
// execution must build the identical stack.
func (b *Bench) sessionAdapter(e *Execution, selected accounts.Profile, options accounts.Options, conversationID string) agent.Adapter {
	id, collectionID, cwd := e.ID, e.CollectionID, e.Cwd
	native := telemetry.Adapter{Native: b.adapter(selected.Provider), Store: b.Telemetry, Prices: b.Prices, Gateway: b.Meter, Scope: telemetry.Scope{Workspace: collectionID, Conversation: conversationID, Execution: id, Provider: selected.Provider}}
	provider := selected.Provider
	return rotation.Adapter{Native: native, Profiles: b.Profiles(), Current: b.Profiles, Eligible: options.RotationProfiles, State: b.Accounts,
		// Switching by itself at a limit needs an account policy now, not only
		// when the conversation started (account_policy.go).
		Switching: func() bool { _, ok := b.AccountPolicy(provider); return ok },
		Resolve: func(p accounts.Profile, open agent.Open) (agent.Open, error) {
			open.Isolated = p.Isolation == "environment"
			// A new attempt on a saved Claude subscription needs Claude sign-in
			// in Lazybox; re-attaching to a running one starts nothing.
			if open.Host == nil || open.Host.Attach == nil {
				if err := b.claudeSignInAllowed(p); err != nil {
					return open, err
				}
			}
			next, err := b.profileEnv(p)
			if err != nil {
				return open, err
			}
			open.Env = b.executionEnv(next, id, collectionID)
			open.Tools = b.executionTools(e, append([]string{}, open.Env...))
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

// sessionOpen resolves the native open request. Every execution runs under a
// session host so a Runner restart never stops its agent.
func (b *Bench) sessionOpen(e *Execution, contextText, resume string) agent.Open {
	open := agent.Open{ID: e.ID, Cwd: e.Cwd, Purpose: e.Purpose, Isolated: e.Profile.Isolation == "environment", Context: contextText, Model: e.Options.Model, Permission: e.Options.Permission, Workflow: e.Options.Workflow, ClaudeAIConnectors: b.Preferences.Snapshot().ClaudeAIConnectors, Gateway: e.Options.Gateway, Profile: e.Profile.ID, Env: e.Env, Resume: resume, Host: &agent.Hosting{Root: b.Config.HostRoot, Backend: b.hosting}}
	open.Tools = b.executionTools(e, e.Env)
	open.Browser = e.browserTools
	return open
}

// executionTools is the Runner tool boundary of one execution. A coding agent
// gets managed commands, run with env. The help assistant gets only the
// documentation tools (read-only) and the
// settings tools: it may read Lazybox's settings and propose a change, which its
// provider puts to the user before the Runner applies it. Other assistants get
// no tools at all.
func (b *Bench) executionTools(e *Execution, env []string) func(context.Context, string, json.RawMessage) (string, error) {
	switch {
	case e.Purpose == "help":
		return func(ctx context.Context, name string, args json.RawMessage) (string, error) {
			if err := ctx.Err(); err != nil {
				return "", err
			}
			if agent.DocsTool(name) {
				return docstool.Handle(b.Config.DocsDirectory, name, args)
			}
			if !agent.SettingsTool(name) {
				return "", agent.ErrUnsupported
			}
			return settingstool.Handle(b.Preferences, name, args)
		}
	case agent.IsAssistant(e.Purpose):
		return nil
	}
	return func(ctx context.Context, name string, args json.RawMessage) (string, error) {
		return b.toolWithEnv(ctx, e, name, args, env)
	}
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
	type unadopted struct {
		record sessionstore.Record
		err    error
	}
	var failed []unadopted
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
				// Its host is gone, and the agent with it: the machine
				// restarted, or the host ended while no Runner ran. Nothing is
				// left to re-attach; the conversation resumes natively.
				if sessionhost.Gone(err) && !b.agentAnswers(ctx, r.ID) {
					err = b.agentLost(ctx, r)
				} else if err != nil {
					failed = append(failed, unadopted{r, err})
				}
				if err != nil {
					errs = append(errs, fmt.Errorf("execution %s not re-attached: %w", r.ID, err))
				}
			}
		}
		if len(records) < 100 {
			break
		}
	}
	errs = append(errs, b.stopOrphanHosts(ctx))
	// The sweep stopped the agents nothing re-attached: their conversations
	// end as interrupted and resumable, not left open forever.
	for _, f := range failed {
		if err := b.endUnadopted(ctx, f.record, f.err); err != nil {
			errs = append(errs, fmt.Errorf("execution %s not ended after its agent was stopped: %w", f.record.ID, err))
		}
	}
	return errors.Join(errs...)
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
	if b.adapter(selected.Provider) == nil {
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
	env, err := b.profileEnv(selected)
	if err != nil {
		return err
	}
	env = b.executionEnv(env, r.ID, r.CollectionID)
	// This Runner's URL may differ from the one the agent's file names.
	if err = b.writeAgentEndpoint(r.ID); err != nil {
		return err
	}
	spool, err := b.openJournal(r.ID)
	if err != nil {
		return err
	}
	e := &Execution{ID: r.ID, WorkID: r.WorkID, Previous: r.Previous, Cwd: r.Cwd, CollectionID: r.CollectionID, Purpose: r.Purpose, Profile: selected, Options: options, Env: env, spool: spool, Saved: r}
	// The re-attached agent keeps the tools it was started with: one started
	// before the browser was there has none until the person gives it them.
	e.browserTools = b.browserToolsOf(r, b.browserOffered(e))
	if r.BrowserTools == nil {
		// Read from its tool list once; kept from now on.
		has := e.browserTools
		if err := b.History.Amend(b.ctx, r.ID, func(saved *sessionstore.Record) { saved.BrowserTools = &has }); err == nil {
			e.Saved.BrowserTools = &has
		}
	}
	adapter := b.sessionAdapter(e, selected, options, r.ConversationID)
	open := b.sessionOpen(e, r.Context, "")
	open.ProviderSettings = r.ProviderSettings
	// The re-attached process holds them already; a later subscription
	// attempt starts with them.
	open.Grants = r.Snapshot.Grants
	eng, err := engine.Adopt(b.ctx, adapter, open, spool, r.Snapshot, record, func(snapshot engine.Snapshot, settings json.RawMessage) error {
		return b.History.SaveState(context.Background(), r.ID, snapshot, settings)
	}, func(a engine.Attachment) error { return b.saveAttachment(r.ID, a) })
	if err != nil {
		_ = spool.Dispose()
		return err
	}
	e.Engine = eng
	b.watchTurns(e)
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

// reconnect is Resume for a live conversation whose only fault is that this
// Runner lost its stream from the agent (agent.TransportFault): the agent may
// well still run in its session host. It never starts a second agent. If
// this engine reads the agent again, the fault is simply cleared. Otherwise
// the conversation is re-attached in place exactly as a Runner restart would
// (Detach, then adopt): a live agent goes on where it was, one that exited
// reports its exit again, and one whose host is gone is lost as after a
// reboot (agentLost) and resumes natively.
func (b *Bench) reconnect(ctx context.Context, e *Execution, live *engine.Engine, processesStopped bool) (*Execution, error) {
	if live.SettleTransportFault() {
		return e, nil
	}
	if err := live.Detach(); err != nil {
		return nil, fmt.Errorf("could not reconnect to the agent: %w", err)
	}
	record, err := b.History.Get(ctx, e.ID)
	if err == nil {
		err = b.adopt(record)
	}
	b.mu.Lock()
	next := b.items[e.ID]
	if next == e {
		// Not re-attached: from here it is a conversation whose adoption
		// failed at startup, restored from what is saved.
		delete(b.items, e.ID)
		next = nil
	}
	delete(b.archives, e.ID)
	b.mu.Unlock()
	if next != nil {
		if err != nil {
			// Re-attached; only the workspace's ownership is unconfirmed.
			log.Printf("reconnect %s: %v", e.ID, err)
		}
		return next, nil
	}
	if record.ID == "" {
		return nil, fmt.Errorf("could not reconnect to the agent: %w", err)
	}
	if sessionhost.Gone(err) && !b.agentAnswers(ctx, e.ID) {
		err = b.agentLost(ctx, record)
	} else if stopErr := b.stopHostsOf(ctx, e.ID); stopErr != nil {
		// Its agent runs, and no Runner supervises it: say so; the next
		// Runner's sweep stops it.
		return nil, fmt.Errorf("could not reconnect to the agent (%v), nor stop it: %w", err, stopErr)
	} else {
		err = b.endUnadopted(ctx, record, err)
	}
	if err != nil {
		return nil, err
	}
	// Ended as interrupted: it continues as any saved conversation does.
	return b.Resume(ctx, e.ID, processesStopped)
}

// AgentLostEvent is the conversation's notice that its agent did not survive
// while no Runner ran (SESSION_HOSTS.md, "After a reboot").
const AgentLostEvent = "agent_lost"

// agentAnswers reports whether any session host started for execution id
// still answers. Only one that is provably gone (no record, or nothing
// listening) counts as not answering: a slow host is not a lost agent.
func (b *Bench) agentAnswers(ctx context.Context, id string) bool {
	ids, err := b.hosting.Hosts(b.Config.HostRoot)
	if err != nil {
		return true
	}
	for _, host := range ids {
		record, err := sessionhost.ReadRecord(filepath.Join(b.Config.HostRoot, host))
		if err != nil || record.Owner != id {
			continue
		}
		probe, cancel := context.WithTimeout(ctx, 5*time.Second)
		_, err = b.hosting.Probe(probe, b.Config.HostRoot, host)
		cancel()
		if !sessionhost.Gone(err) {
			return true
		}
	}
	return false
}

// carriedPath holds the messages a lost agent never received, until the
// conversation is resumed and they are delivered to its successor. Like the
// attachment it replaces, it may hold image bytes, so it is private.
func (b *Bench) carriedPath(id string) string {
	return filepath.Join(b.Config.State, "sessions", id+".carried.json")
}

func (b *Bench) loadCarried(id string) ([]agent.Operation, error) {
	var queue []agent.Operation
	data, err := os.ReadFile(b.carriedPath(id))
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	return queue, json.Unmarshal(data, &queue)
}

func (b *Bench) saveCarried(id string, queue []agent.Operation) error {
	if len(queue) == 0 {
		if err := os.Remove(b.carriedPath(id)); err != nil && !errors.Is(err, os.ErrNotExist) {
			return err
		}
		return nil
	}
	data, err := json.Marshal(queue)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Join(b.Config.State, "sessions"), ".carried-*")
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
	return os.Rename(f.Name(), b.carriedPath(id))
}

// agentLost ends a conversation whose agent is gone with everything a
// person needs to carry on: a notice saying what happened, its saved
// provider session to resume, and the messages it never received, kept for
// the resumed agent. It is not "recovery required": the Runner checked that
// nothing of the old agent runs, so the person is not asked to.
//
// Each step can be repeated if this Runner dies part way: the notice is
// written once, and the attachment goes last, so a later Runner either
// finishes the job or skips a conversation already ended.
func (b *Bench) agentLost(ctx context.Context, r sessionstore.Record) error {
	return b.endLost(ctx, r, "")
}

// endLost is agentLost, its notice led by why the agent is gone when it is not
// the usual reason (a restart that found it gone).
func (b *Bench) endLost(ctx context.Context, r sessionstore.Record, why string) error {
	record, err := b.loadAttachment(r.ID)
	if err != nil && (why == "" || errors.Is(err, os.ErrNotExist)) {
		return err
	}
	if err != nil {
		// An attachment this Runner cannot read is why it could not adopt;
		// the conversation still ends, with no queued message to carry.
		record = engine.Attachment{}
	}
	return b.endInterrupted(ctx, r, record, why)
}

// endInterrupted is endLost once the attachment is read: the notice, the
// carried messages and the interrupted snapshot, then the attachment goes.
func (b *Bench) endInterrupted(ctx context.Context, r sessionstore.Record, record engine.Attachment, why string) error {
	if err := b.saveCarried(r.ID, record.Queue); err != nil {
		return err
	}
	spool, err := b.openJournal(r.ID)
	if err != nil {
		return err
	}
	noted := false
	if n := spool.Count(); n > 0 {
		last, cancel := context.WithTimeout(ctx, time.Second)
		if rec, err := spool.Next(last, n-1); err == nil {
			var ev agent.Event
			noted = json.Unmarshal(rec.Value, &ev) == nil && ev.Type == AgentLostEvent
		}
		cancel()
	}
	snapshot := r.Snapshot
	if !noted {
		_, err = spool.Append(agent.Event{Type: AgentLostEvent, Status: lostStatus(why), Text: lostNotice(snapshot, len(record.Queue), why)})
	}
	if disposeErr := spool.Dispose(); err == nil {
		err = disposeErr
	}
	if err != nil {
		return err
	}
	snapshot.Closed, snapshot.Busy = true, false
	snapshot.Status, snapshot.Fault = agent.Interrupted, ""
	snapshot.Pending = map[string]agent.Event{}
	for id, a := range snapshot.Agents {
		if a.Status == agent.Running || a.Status == agent.Starting || a.Status == agent.WaitingInput || a.Status == agent.WaitingApproval {
			a.Status = agent.Interrupted
			snapshot.Agents[id] = a
		}
	}
	snapshot.Queued, snapshot.QueuedMessages = len(record.Queue), engine.QueuedMessages(record.Queue, nil)
	if err = b.History.SaveState(ctx, r.ID, snapshot, r.ProviderSettings); err != nil {
		return err
	}
	if err = os.Remove(b.attachmentPath(r.ID)); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

// lostStatus is the agent_lost status for why (endLost).
func lostStatus(why string) string {
	if why != "" {
		return "not_reattached_after_restart"
	}
	return "host_gone_after_restart"
}

// unadoptedNotice leads the notice of a conversation whose agent this Runner
// could not re-attach (err) and stopped: it must not go on unsupervised.
func unadoptedNotice(err error) string {
	return fmt.Sprintf("Lazybox restarted but could not re-attach to this agent (%v), so it stopped it: an agent no one supervises must not keep working in a workspace.", err)
}

// endUnadopted ends a conversation whose live agent this Runner could not
// re-attach (for a reason other than its host being gone), once its host is
// stopped, as agentLost ends one whose host is gone: interrupted, resumable,
// never left open as "recovery required". A host that still answers is left
// alone, and so is the conversation.
func (b *Bench) endUnadopted(ctx context.Context, r sessionstore.Record, cause error) error {
	if b.agentAnswers(ctx, r.ID) {
		return nil
	}
	return b.endLost(ctx, r, unadoptedNotice(cause))
}

// stopHostsOf stops every agent host started for execution id.
func (b *Bench) stopHostsOf(ctx context.Context, id string) error {
	ids, err := b.hosting.Hosts(b.Config.HostRoot)
	if err != nil {
		return err
	}
	var errs []error
	for _, host := range ids {
		record, err := sessionhost.ReadRecord(filepath.Join(b.Config.HostRoot, host))
		if err != nil || record.Owner != id {
			continue
		}
		if err = b.hosting.Stop(ctx, b.Config.HostRoot, host); err != nil && !sessionhost.Gone(err) {
			errs = append(errs, err)
		}
	}
	return errors.Join(errs...)
}

// lostNotice says what happened to the agent, what was lost with it, and
// what to do: the conversation shows it where the person will look.
func lostNotice(s engine.Snapshot, queued int, why string) string {
	var text strings.Builder
	if why != "" {
		text.WriteString(why + " ")
	} else {
		text.WriteString("Lazybox restarted and this agent was no longer running: the Mac restarted, or the agent stopped while Lazybox was not running. ")
	}
	switch {
	case len(s.Pending) > 0:
		text.WriteString("The turn in progress stopped while it waited for you; its request is no longer open. ")
	case s.Busy:
		text.WriteString("The turn in progress stopped before it finished. ")
	}
	switch queued {
	case 0:
	case 1:
		text.WriteString("The message you queued was not sent yet; it goes first when you continue. ")
	default:
		fmt.Fprintf(&text, "The %d messages you queued were not sent yet; they go first when you continue. ", queued)
	}
	text.WriteString("The conversation, its checkpoints and commands are saved: send a message to resume the agent's session.")
	return text.String()
}

// StopUnclaimedHosts stops every agent and command host this Runner is not
// attached to, without adopting anything. A Runner whose agents stop with it
// runs it at startup, so a crashed predecessor's agents do not keep running.
func (b *Bench) StopUnclaimedHosts(ctx context.Context) error { return b.stopOrphanHosts(ctx) }

// stopOrphanHosts stops hosts nothing attached to: agents no adopted execution
// claimed, and commands and terminals their managers did not adopt. This Runner holds the
// state lock, so no other Runner can be their client: a host is claimed only if
// this process holds it (sessionhost.Held). The host's own "attached" is not
// evidence of a claim: a host counts a crashed or detached predecessor as
// attached until it has read that connection's close, and trusting it left
// such a host's agent running, supervised by no one, until the orphan limit.
func (b *Bench) stopOrphanHosts(ctx context.Context) error {
	_, err := b.sweepOrphanHosts(ctx)
	return err
}

// sweepOrphanHosts is stopOrphanHosts, counting the live hosts it stopped.
func (b *Bench) sweepOrphanHosts(ctx context.Context) (int, error) {
	var errs []error
	stopped := 0
	for _, root := range []string{b.Config.HostRoot, b.Config.CommandHostRoot, b.Config.TerminalHostRoot} {
		ids, err := b.hosting.Hosts(root)
		if err != nil {
			errs = append(errs, err)
			continue
		}
		for _, id := range ids {
			_, err := b.hosting.Probe(ctx, root, id)
			if sessionhost.Gone(err) {
				// A stale record of a host that died, for example with its machine.
				_ = b.hosting.Forget(root, id)
				continue
			}
			if err != nil {
				// Slow or interrupted, not gone: deleting its directory would stop
				// a program that may be live and adopted.
				errs = append(errs, fmt.Errorf("probe session host %s: %w", id, err))
				continue
			}
			if b.hosting.Held(root, id) {
				continue
			}
			if err = b.hosting.Stop(ctx, root, id); err != nil {
				errs = append(errs, fmt.Errorf("orphan session host %s: %w", id, err))
			} else {
				stopped++
			}
		}
	}
	return stopped, errors.Join(errs...)
}
