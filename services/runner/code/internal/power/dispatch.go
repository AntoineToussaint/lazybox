package power

import (
	"errors"
	"slices"
	"sync"
	"time"
)

// Hook run states.
const (
	RunRunning   = "running"
	RunSucceeded = "succeeded"
	RunFailed    = "failed"
	RunSkipped   = "skipped"
)

// Limits on hook runs, so a hook that loops (a turn.done hook that sends a
// message ends another turn) or a burst of events cannot run away.
const (
	MaxRunsPerHookPerMinute = 10
	MaxConcurrentRuns       = 4
	keptRuns                = 100
	seenFor                 = 6 * time.Hour
)

// HookRun is one firing of a hook.
type HookRun struct {
	ID, HookID, Event, State string
	CommandID                string
	ExitCode                 *int
	Error, Summary           string
	CollectionID, SessionID  string
	Started, Ended           time.Time
	HTTPStatus               int
	Grants                   []string
	Test                     bool
}

// Launcher starts a hook run (the Runner's managed commands). It returns at
// once; the run is finished later with Dispatcher.Finish.
type Launcher func(h Hook, e Event, run HookRun) (commandID string, err error)

// Dispatcher matches events against the file's hooks, runs each match once,
// and keeps the recent runs.
type Dispatcher struct {
	Manager *Manager
	Launch  Launcher
	Now     func() time.Time

	mu     sync.Mutex
	seen   map[string]time.Time
	fired  map[string][]time.Time
	runs   []*HookRun
	active int
}

func NewDispatcher(m *Manager, launch Launcher) *Dispatcher {
	return &Dispatcher{Manager: m, Launch: launch, Now: time.Now, seen: map[string]time.Time{}, fired: map[string][]time.Time{}}
}

// Dispatch runs every hook that follows the event and has not run for it.
func (d *Dispatcher) Dispatch(e Event) []HookRun {
	var out []HookRun
	for _, h := range d.Manager.Snapshot().File.Hooks {
		if !h.Matches(e) {
			continue
		}
		if run, ok := d.fire(h, e); ok {
			out = append(out, run)
		}
	}
	return out
}

// Test runs one hook now with a sample event, whatever its filters say.
func (d *Dispatcher) Test(hookID string, e Event) (HookRun, error) {
	for _, h := range d.Manager.Snapshot().File.Hooks {
		if h.ID == hookID {
			e.Test = true
			if e.Name == "" {
				e.Name = h.On[0]
			}
			e.Key = "test:" + newID()
			run, _ := d.fire(h, e)
			return run, nil
		}
	}
	return HookRun{}, errors.New("no hook with that id in the actions file")
}

func (d *Dispatcher) fire(h Hook, e Event) (HookRun, bool) {
	now := d.Now()
	d.mu.Lock()
	key := h.ID + "\x00" + e.Name + "\x00" + e.Key
	if e.Key != "" {
		if _, done := d.seen[key]; done {
			d.mu.Unlock()
			return HookRun{}, false
		}
		d.seen[key] = now
	}
	for k, at := range d.seen {
		if now.Sub(at) > seenFor {
			delete(d.seen, k)
		}
	}
	recent := slices.DeleteFunc(d.fired[h.ID], func(at time.Time) bool { return now.Sub(at) > time.Minute })
	run := &HookRun{ID: newID(), HookID: h.ID, Event: e.Name, State: RunRunning, Summary: e.Summary(), Started: now, Grants: h.Grants, Test: e.Test}
	if e.Workspace != nil {
		run.CollectionID = e.Workspace.ID
	}
	if e.Conversation != nil {
		run.SessionID = e.Conversation.Session
	}
	skip := ""
	switch {
	case len(recent) >= MaxRunsPerHookPerMinute:
		skip = "skipped: this hook already ran 10 times in the last minute"
	case d.active >= MaxConcurrentRuns:
		skip = "skipped: 4 hooks are already running"
	case run.CollectionID == "":
		skip = "skipped: hooks run for events in a workspace"
	}
	if skip != "" {
		run.State, run.Error, run.Ended = RunSkipped, skip, now
		d.keepLocked(run)
		d.mu.Unlock()
		return *run, true
	}
	d.fired[h.ID] = append(recent, now)
	d.active++
	d.keepLocked(run)
	d.mu.Unlock()
	commandID, err := d.Launch(h, e, *run)
	d.mu.Lock()
	run.CommandID = commandID
	d.mu.Unlock()
	if err != nil {
		d.Finish(run.ID, func(r *HookRun) { r.State, r.Error = RunFailed, err.Error() })
	}
	return d.run(run.ID), true
}

func (d *Dispatcher) keepLocked(run *HookRun) {
	d.runs = append(d.runs, run)
	if len(d.runs) > keptRuns {
		d.runs = d.runs[len(d.runs)-keptRuns:]
	}
}

// Finish records a run's outcome once.
func (d *Dispatcher) Finish(id string, outcome func(*HookRun)) {
	d.mu.Lock()
	defer d.mu.Unlock()
	for _, r := range d.runs {
		if r.ID == id && r.State == RunRunning {
			outcome(r)
			if r.State == RunRunning {
				r.State = RunSucceeded
			}
			r.Ended = d.Now()
			d.active--
			return
		}
	}
}

// Update changes a running run (its command, say) without finishing it.
func (d *Dispatcher) Update(id string, change func(*HookRun)) {
	d.mu.Lock()
	defer d.mu.Unlock()
	for _, r := range d.runs {
		if r.ID == id {
			change(r)
		}
	}
}

func (d *Dispatcher) run(id string) HookRun {
	d.mu.Lock()
	defer d.mu.Unlock()
	for _, r := range d.runs {
		if r.ID == id {
			return *r
		}
	}
	return HookRun{}
}

// Runs lists the recent runs, newest first.
func (d *Dispatcher) Runs(limit int) []HookRun {
	d.mu.Lock()
	defer d.mu.Unlock()
	out := make([]HookRun, 0, len(d.runs))
	for i := len(d.runs) - 1; i >= 0 && (limit <= 0 || len(out) < limit); i-- {
		out = append(out, *d.runs[i])
	}
	return out
}
