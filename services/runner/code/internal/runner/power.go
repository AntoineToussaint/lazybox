package runner

import (
	"bytes"
	"context"
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/power"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/work"
)

// Power-user extensibility (POWER.md): the actions and hooks file, the hooks
// it declares, and the requests the CLI makes of an open window.

// powerState is the Bench's part of it.
type powerState struct {
	// Actions is the user's actions file, reloaded when it changes.
	Actions *power.Manager
	// Hooks runs the file's hooks on events.
	Hooks *power.Dispatcher
	// UI routes `lazybox open|shot|actions run|do` to an open window.
	UI *power.Broker
	// Config is keys.yaml, settings.yaml and layouts/*.yaml beside the actions file.
	Config *power.Config
	// Extensions are the sandboxed panels in the config folder's extensions/
	// (EXTENSIONS.md), with the person's approvals kept in the Runner's state.
	Extensions *power.Extensions

	credMu sync.Mutex
	// hookCreds: the grants of each running hook that holds a credential.
	hookCreds map[string][]string
	// hookStreams: per hook run, the cancel funcs of the streams its
	// credential opened (BindHook), ended by revokeHook.
	hookStreams map[string]map[*context.CancelCauseFunc]struct{}
	observer    hookObserver
	// hookClient posts webhooks.
	hookClient *http.Client
}

// hookTokenPrefix marks a hook credential; no other credential has it.
const hookTokenPrefix = "lbx-hook."

// powerPoll is how often the file is reread and events are looked for.
const powerPoll = time.Second

func (b *Bench) openPower() {
	path := b.Config.ActionsFile
	if path == "" {
		path = filepath.Join(b.Config.State, "actions.yaml")
	}
	b.power.Actions = power.NewManager(path)
	b.power.Config = power.NewConfig(filepath.Dir(path))
	b.power.Extensions = power.NewExtensions(filepath.Join(filepath.Dir(path), "extensions"), filepath.Join(b.Config.State, "extensions.json"))
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		b.power.Extensions.WatchDiskScans(b.ctx)
	}()
	b.power.UI = power.NewBroker()
	b.power.Hooks = power.NewDispatcher(b.power.Actions, b.launchHook)
	b.power.hookCreds = map[string][]string{}
	b.power.hookStreams = map[string]map[*context.CancelCauseFunc]struct{}{}
	b.power.hookClient = &http.Client{Timeout: 15 * time.Second, CheckRedirect: func(r *http.Request, via []*http.Request) error {
		if len(via) >= 3 || (r.URL.Scheme != "http" && r.URL.Scheme != "https") {
			return errors.New("webhook redirect refused")
		}
		return nil
	}}
	b.watchers.Add(1)
	go func() {
		defer b.watchers.Done()
		ticker := time.NewTicker(powerPoll)
		defer ticker.Stop()
		for {
			select {
			case <-b.ctx.Done():
				return
			case <-ticker.C:
				b.power.Actions.Reload()
				b.power.Config.Reload()
				b.power.Extensions.Reload()
				b.ObserveHookEvents()
			}
		}
	}()
}

// Actions is the user's actions file.
func (b *Bench) Actions() *power.Manager { return b.power.Actions }

// Hooks runs the file's hooks.
func (b *Bench) Hooks() *power.Dispatcher { return b.power.Hooks }

// UI routes the CLI's requests to an open window.
func (b *Bench) UI() *power.Broker { return b.power.UI }

// ConfigFiles are the person's config files beside the actions file.
func (b *Bench) ConfigFiles() *power.Config { return b.power.Config }

// Extensions are the person's sandboxed panel extensions.
func (b *Bench) Extensions() *power.Extensions { return b.power.Extensions }

// ExtensionScope returns the extension and approved grants of a live
// extension credential (transport/extension_auth.go), or an error for a
// forged, revoked or expired one.
func (b *Bench) ExtensionScope(token string) (string, []string, error) {
	if b.power.Extensions == nil {
		return "", nil, power.ErrExtensionCredential
	}
	return b.power.Extensions.Scope(token)
}

// hookObserver turns state changes into events. Its first pass only records
// what is already there: a Runner restart replays nothing.
type hookObserver struct {
	mu       sync.Mutex
	started  bool
	pending  map[string]bool
	turns    map[string]time.Time
	faults   map[string]string
	known    map[string]bool
	version  uint64
	missions map[string]uint64
}

// ObserveHookEvents looks once for new events and dispatches them. The
// Runner calls it every second; tests call it directly.
func (b *Bench) ObserveHookEvents() {
	o := &b.power.observer
	o.mu.Lock()
	if o.pending == nil {
		o.pending, o.turns, o.faults, o.known, o.missions = map[string]bool{}, map[string]time.Time{}, map[string]string{}, map[string]bool{}, map[string]uint64{}
	}
	baseline := !o.started
	o.started = true
	var events []power.Event
	live := map[string]bool{}
	for _, e := range b.List() {
		if e.Engine == nil || agent.IsAssistant(e.Purpose) {
			continue
		}
		live[e.ID] = true
		s := e.Engine.Snapshot()
		fresh := !o.known[e.ID]
		o.known[e.ID] = true
		for id, request := range s.Pending {
			key := e.ID + "\x00" + id
			if o.pending[key] {
				continue
			}
			o.pending[key] = true
			if !baseline {
				ev := b.hookEvent(e, power.EventNeedsInput, s.Title)
				ev.Request = &power.Request{ID: id, Kind: request.Type}
				ev.Reason, ev.Line = requestReason(request), shortLine(request.Text)
				ev.Key = key
				events = append(events, ev)
			}
		}
		if root, ok := s.Agents["root"]; ok {
			previous, seen := o.turns[e.ID]
			o.turns[e.ID] = root.TurnEndedAt
			// An execution's first sighting is its baseline too: a resumed
			// conversation's old turn is not a new one.
			if !baseline && seen && root.TurnEndedAt.After(previous) {
				name := power.EventTurnDone
				switch {
				case root.Status == agent.Failed || root.LastTurnStatus == "failed":
					name = power.EventTurnFailed
				case root.Status == agent.Interrupted || root.LastTurnStatus == "interrupted":
					name = "" // the person interrupted it
				}
				if name != "" {
					ev := b.hookEvent(e, name, s.Title)
					ev.Line = shortLine(s.LastMessage)
					ev.Reason = map[string]string{power.EventTurnDone: "turn ended", power.EventTurnFailed: "turn failed"}[name]
					ev.Key = e.ID + "\x00" + root.TurnEndedAt.Format(time.RFC3339Nano)
					events = append(events, ev)
				}
			}
		}
		if s.Fault != "" && o.faults[e.ID] != s.Fault {
			o.faults[e.ID] = s.Fault
			if !baseline && !fresh {
				ev := b.hookEvent(e, power.EventTurnFailed, s.Title)
				ev.Reason, ev.Line, ev.Key = "fault", shortLine(s.Fault), e.ID+"\x00fault\x00"+s.Fault
				events = append(events, ev)
			}
		}
	}
	for key := range o.pending {
		if id, _, _ := strings.Cut(key, "\x00"); !live[id] {
			delete(o.pending, key)
		}
	}
	for id := range o.known {
		if !live[id] {
			delete(o.known, id)
			delete(o.turns, id)
			delete(o.faults, id)
		}
	}
	events = append(events, b.missionEvents(o, baseline)...)
	o.mu.Unlock()
	for _, ev := range events {
		b.power.Hooks.Dispatch(ev)
	}
}

// missionEvents: a person passed a task's gate (accepted its work).
func (b *Bench) missionEvents(o *hookObserver, baseline bool) []power.Event {
	if b.Work == nil || b.Work.Version() == o.version {
		return nil
	}
	o.version = b.Work.Version()
	var out []power.Event
	for _, snap := range b.Work.Missions() {
		if snap.State == nil {
			continue
		}
		id := snap.State.ID
		if snap.State.Seq == o.missions[id] {
			continue
		}
		state, err := b.Work.Mission(id)
		if err != nil {
			continue
		}
		after := o.missions[id]
		o.missions[id] = state.Seq
		if baseline {
			continue
		}
		for _, ev := range state.Events {
			if ev.Seq <= after || ev.Type != work.GatePassed || ev.Actor.Kind != work.ActorHuman {
				continue
			}
			var gate work.GatePayload
			_ = json.Unmarshal(ev.Payload, &gate)
			m := &power.Mission{ID: id, Title: state.Title, Task: gate.Task, Note: gate.Note}
			event := power.Event{Name: power.EventMissionAccepted, At: ev.At, Mission: m, Reason: "accepted", Key: ev.ID}
			if t := state.Task(gate.Task); t != nil {
				m.TaskTitle = t.Title
				for i := len(t.Attempts) - 1; i >= 0; i-- {
					if a := t.Attempts[i]; a.Workspace != "" {
						event.Workspace = b.workspaceRef(a.Workspace)
						if a.Conversation != "" {
							event.Conversation = &power.Conversation{ID: a.Conversation, Session: a.Execution}
						}
						break
					}
				}
			}
			out = append(out, event)
		}
	}
	return out
}

func (b *Bench) workspaceRef(id string) *power.Ref {
	if id == "" {
		return nil
	}
	ref := &power.Ref{ID: id}
	if b.Collections != nil {
		if c, err := b.Collections.Get(id); err == nil {
			ref.Name = c.Name
		}
	}
	return ref
}

func (b *Bench) hookEvent(e *Execution, name, title string) power.Event {
	if title == "" {
		title = e.Saved.Title
	}
	return power.Event{Name: name, At: time.Now(), Workspace: b.workspaceRef(e.CollectionID),
		Conversation: &power.Conversation{ID: conversationOf(e), Session: e.ID, Title: title, Provider: e.Profile.Provider, Model: e.Options.Model}}
}

func requestReason(r agent.Event) string {
	kind := "Approval"
	if strings.Contains(r.Type, "question") {
		kind = "Question"
	}
	if text := shortLine(r.Text); text != "" {
		return kind + ": " + text
	}
	return kind
}

func shortLine(text string) string {
	lines := strings.Split(strings.TrimSpace(text), "\n")
	line := strings.TrimSpace(lines[len(lines)-1])
	if len(line) > 160 {
		line = line[:159] + "…"
	}
	return line
}

// SampleHookEvent is what Settings' Test sends: the newest conversation of
// the newest workspace, or a placeholder, marked as a test.
func (b *Bench) SampleHookEvent(hookID string) power.Event {
	ev := power.Event{At: time.Now(), Test: true, Reason: "test from Settings"}
	for _, h := range b.power.Actions.Snapshot().File.Hooks {
		if h.ID == hookID {
			ev.Name = h.On[0]
		}
	}
	executions := b.List()
	slices.SortFunc(executions, func(x, y *Execution) int { return strings.Compare(y.ID, x.ID) })
	for _, e := range executions {
		if e.CollectionID != "" && !agent.IsAssistant(e.Purpose) {
			sample := b.hookEvent(e, ev.Name, "")
			sample.Test, sample.Reason = true, ev.Reason
			return sample
		}
	}
	if b.Collections != nil {
		if list := b.Collections.List(); len(list) > 0 {
			ev.Workspace = &power.Ref{ID: list[0].ID, Name: list[0].Name}
		}
	}
	return ev
}

// launchHook starts one hook run as a managed command of the event's
// workspace: a program, or for a webhook a pipe holding the request's log.
func (b *Bench) launchHook(h power.Hook, e power.Event, run power.HookRun) (string, error) {
	if b.Collections == nil {
		return "", errors.New("workspace storage unavailable")
	}
	workspace, err := b.Collections.Get(run.CollectionID)
	if err != nil {
		return "", fmt.Errorf("the event's workspace is gone: %w", err)
	}
	cwd := workspace.Root
	if info, statErr := os.Stat(cwd); statErr != nil || !info.IsDir() {
		cwd = b.Config.State
	}
	title := "Hook " + h.ID + " · " + e.Name
	if e.Test {
		title += " (test)"
	}
	spec := commands.Spec{CollectionID: workspace.ID, Kind: commands.KindHook, Action: h.ID, Title: title, Cwd: cwd}
	if h.Webhook != "" {
		c, err := b.Commands.Register(spec)
		if err != nil {
			return "", err
		}
		go b.postWebhook(c, h, e, run.ID)
		return c.Status().ID, nil
	}
	data, err := json.MarshalIndent(e, "", "  ")
	if err != nil {
		return "", err
	}
	dir := filepath.Join(b.Config.State, "hook-runs")
	if err = os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	eventFile := filepath.Join(dir, run.ID+".json")
	if err = os.WriteFile(eventFile, data, 0o600); err != nil {
		return "", err
	}
	env := b.withCLI(power.Environment(os.Environ(), e, eventFile))
	if len(h.Grants) > 0 {
		endpoint, err := b.grantHook(run.ID, h.Grants)
		if err != nil {
			_ = os.Remove(eventFile)
			return "", err
		}
		env = append(env, "LAZYBOX_V2_ENDPOINT="+endpoint)
	}
	spec.Argv, spec.Env, spec.StopGraceMs = h.Run, env, 2000
	if h.Shell != "" {
		spec.Argv = []string{"/bin/sh", "-c", h.Shell}
	}
	c, err := b.Commands.Start(spec)
	if err != nil {
		b.revokeHook(run.ID)
		_ = os.Remove(eventFile)
		return "", err
	}
	go b.waitHook(c, h.Timeout, run.ID, eventFile)
	return c.Status().ID, nil
}

func (b *Bench) waitHook(c *commands.Command, timeout time.Duration, runID, eventFile string) {
	defer func() { b.revokeHook(runID); _ = os.Remove(eventFile) }()
	ctx, cancel := context.WithTimeout(b.ctx, timeout)
	status, err := c.Wait(ctx)
	cancel()
	timedOut := errors.Is(err, context.DeadlineExceeded)
	if err != nil {
		_ = c.Cancel()
		stopped, stopCancel := context.WithTimeout(context.WithoutCancel(b.ctx), 10*time.Second)
		status, _ = c.Wait(stopped)
		stopCancel()
	}
	b.power.Hooks.Finish(runID, func(r *power.HookRun) {
		r.ExitCode = status.ExitCode
		switch {
		case timedOut:
			r.State, r.Error = power.RunFailed, fmt.Sprintf("stopped after its timeout (%s)", timeout)
		case err != nil:
			r.State, r.Error = power.RunFailed, "the Runner stopped before the hook finished"
		case status.Error != "":
			r.State, r.Error = power.RunFailed, status.Error
		case status.ExitCode == nil || *status.ExitCode != 0:
			r.State = power.RunFailed
			if status.ExitCode != nil {
				r.Error = fmt.Sprintf("exited %d", *status.ExitCode)
			} else if status.Signal != "" {
				r.Error = "stopped by " + status.Signal
			}
		}
	})
}

// postWebhook sends the event as JSON and logs the exchange to the run's pipe.
func (b *Bench) postWebhook(c *commands.Command, h power.Hook, e power.Event, runID string) {
	var log bytes.Buffer
	offset := uint64(0)
	write := func(format string, args ...any) {
		log.Reset()
		fmt.Fprintf(&log, format, args...)
		if c.Publish(offset, log.Bytes()) == nil {
			offset += uint64(log.Len())
		}
	}
	body, _ := json.Marshal(e)
	write("POST %s\n%s\n\n", h.Webhook, body)
	status, failure := 0, ""
	request, err := http.NewRequestWithContext(b.ctx, http.MethodPost, h.Webhook, bytes.NewReader(body))
	if err == nil {
		request.Header.Set("Content-Type", "application/json")
		request.Header.Set("User-Agent", "Lazybox-Hooks/2")
		request.Header.Set("X-Lazybox-Event", e.Name)
		for name, value := range h.Headers {
			request.Header.Set(name, value)
		}
		started := time.Now()
		ctx, cancel := context.WithTimeout(b.ctx, h.Timeout)
		var response *http.Response
		response, err = b.power.hookClient.Do(request.WithContext(ctx))
		if err == nil {
			reply, _ := io.ReadAll(io.LimitReader(response.Body, 2048))
			response.Body.Close()
			status = response.StatusCode
			write("→ %s in %s\n%s\n", response.Status, time.Since(started).Round(time.Millisecond), reply)
			if status >= 300 {
				failure = response.Status
			}
		}
		cancel()
	}
	if err != nil {
		failure = err.Error()
		write("→ failed: %v\n", err)
	}
	var cause error
	if failure != "" {
		cause = errors.New(failure)
	}
	_ = c.FinishPipe(cause)
	b.power.Hooks.Finish(runID, func(r *power.HookRun) {
		r.HTTPStatus = status
		if failure != "" {
			r.State, r.Error = power.RunFailed, failure
		}
	})
}

// Hook credentials. A hook holds one only when its file grants it, only while
// it runs, and only for its grants: the transport admits it to the RPCs a
// paired phone with the same grants reaches, never more.

func (b *Bench) hookMAC(run string, grants []string) []byte {
	mac := hmac.New(sha256.New, b.agentKey)
	mac.Write([]byte("lazybox v2 hook credential\x00" + run + "\x00" + strings.Join(grants, ",")))
	return mac.Sum(nil)
}

func (b *Bench) hookEndpointDir() string { return filepath.Join(b.Config.State, "hook-endpoints") }

// grantHook issues a running hook's credential and writes its endpoint file.
func (b *Bench) grantHook(run string, grants []string) (string, error) {
	if !validExecutionID(run) {
		return "", errors.New("invalid hook run")
	}
	grants = slices.Clone(grants)
	slices.Sort(grants)
	token := hookTokenPrefix + run + "." + base64.RawURLEncoding.EncodeToString(b.hookMAC(run, grants))
	if err := os.MkdirAll(b.hookEndpointDir(), 0o700); err != nil {
		return "", err
	}
	data, err := json.Marshal(map[string]string{"url": b.Config.Address, "token": token})
	if err != nil {
		return "", err
	}
	path := filepath.Join(b.hookEndpointDir(), run+".json")
	if err = os.WriteFile(path, data, 0o600); err != nil {
		return "", err
	}
	b.power.credMu.Lock()
	b.power.hookCreds[run] = grants
	b.power.credMu.Unlock()
	return path, nil
}

func (b *Bench) revokeHook(run string) {
	b.power.credMu.Lock()
	delete(b.power.hookCreds, run)
	// The streams the hook opened end with its credential.
	for cancel := range b.power.hookStreams[run] {
		(*cancel)(ErrHookCredentialEnded)
	}
	delete(b.power.hookStreams, run)
	b.power.credMu.Unlock()
	if validExecutionID(run) {
		if err := os.Remove(filepath.Join(b.hookEndpointDir(), run+".json")); err != nil && !errors.Is(err, fs.ErrNotExist) {
			return
		}
	}
}

// HookScope returns the run and grants of a running hook's credential, or an
// error for a forged one or one whose run has ended.
func (b *Bench) HookScope(token string) (string, []string, error) {
	rest, ok := strings.CutPrefix(token, hookTokenPrefix)
	if !ok {
		return "", nil, errors.New("not a hook credential")
	}
	dot := strings.LastIndexByte(rest, '.')
	if dot <= 0 {
		return "", nil, errors.New("malformed hook credential")
	}
	run, encoded := rest[:dot], rest[dot+1:]
	presented, err := base64.RawURLEncoding.DecodeString(encoded)
	if err != nil || !validExecutionID(run) {
		return "", nil, errors.New("invalid hook credential")
	}
	b.power.credMu.Lock()
	grants, live := b.power.hookCreds[run]
	b.power.credMu.Unlock()
	if !live || !hmac.Equal(presented, b.hookMAC(run, grants)) {
		return "", nil, errors.New("invalid or expired hook credential")
	}
	return run, slices.Clone(grants), nil
}

// ErrHookCredentialEnded is the cause of a context bound to a hook's
// credential (BindHook) ending because the hook's run ended.
var ErrHookCredentialEnded = errors.New("this hook's run ended; its credential is no longer valid")

// BindHook returns a context derived from parent that ends, with cause
// ErrHookCredentialEnded, when the hook run's credential is revoked (its run
// ended), so a stream it opened does not outlive it. It fails for a run whose
// credential already ended. release must be called when the call ends.
func (b *Bench) BindHook(parent context.Context, run string) (context.Context, context.CancelFunc, error) {
	b.power.credMu.Lock()
	defer b.power.credMu.Unlock()
	if _, live := b.power.hookCreds[run]; !live {
		return nil, nil, ErrHookCredentialEnded
	}
	ctx, cancel := context.WithCancelCause(parent)
	key := &cancel
	if b.power.hookStreams[run] == nil {
		b.power.hookStreams[run] = map[*context.CancelCauseFunc]struct{}{}
	}
	b.power.hookStreams[run][key] = struct{}{}
	return ctx, func() {
		b.power.credMu.Lock()
		delete(b.power.hookStreams[run], key)
		if len(b.power.hookStreams[run]) == 0 {
			delete(b.power.hookStreams, run)
		}
		b.power.credMu.Unlock()
		cancel(context.Canceled)
	}, nil
}
