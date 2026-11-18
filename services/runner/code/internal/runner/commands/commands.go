package commands

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/processguard"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

type Spec struct {
	SessionID string `json:"sessionId"`
	// CollectionID, with no SessionID, makes a workspace run: published from a
	// workspace terminal and shown in every conversation of that workspace.
	CollectionID string `json:"collectionId,omitempty"`
	// TerminalID is the workspace terminal whose credential registered the
	// run; only that credential may publish to it or finish it.
	TerminalID string `json:"terminalId,omitempty"`
	// ShareWith is the conversation whose message box receives the run's
	// compact view once it finishes (`lazybox log --to`).
	ShareWith string `json:"shareWith,omitempty"`
	// Kind, with a CollectionID, makes a managed workspace command the
	// workspace runtime started: "setup", "archive" or "action"; or a hook
	// run (KindHook), which no conversation's console lists.
	Kind string `json:"kind,omitempty"`
	// Action is the runtime entry it runs ("<member>/<name>" for an action,
	// the member for setup and archive); Repository is that member's
	// repository, which identifies "the same action" across workspaces.
	Action     string `json:"action,omitempty"`
	Repository string `json:"repository,omitempty"`
	// StopGraceMs gives the command a graceful stop: Cancel sends SIGTERM to
	// its process group and kills it only after this many milliseconds.
	StopGraceMs int64    `json:"stopGraceMs,omitempty"`
	Title       string   `json:"title"`
	Cwd         string   `json:"cwd"`
	Argv        []string `json:"argv,omitempty"`
	Env         []string `json:"-"`
}

type Status struct {
	ID       string `json:"id"`
	Spec     Spec   `json:"spec"`
	Mode     string `json:"mode"`
	Offset   uint64 `json:"offset"`
	Done     bool   `json:"done"`
	ExitCode *int   `json:"exitCode,omitempty"`
	Signal   string `json:"signal,omitempty"`
	Error    string `json:"error,omitempty"`
	// Report is what a `lazybox run` gave its agent; pipes only.
	Report *Report `json:"report,omitempty"`
	// StartedAt is when the command was registered (Unix milliseconds).
	StartedAt int64 `json:"startedAt,omitempty"`
	// Following: the `lazybox run` that publishes this pipe follows the
	// Runner (FollowPipe), so a stop and a move to the background reach it
	// and the pipe outlives a Runner restart.
	Following bool `json:"following,omitempty"`
	// Background: that run went on in the background (a server or watcher).
	Background bool `json:"background,omitempty"`
	// PID and Ports are what that run last reported: its process (group) and
	// the TCP ports its processes listen on.
	PID   int      `json:"pid,omitempty"`
	Ports []uint32 `json:"ports,omitempty"`
}

// CanBackground reports a `lazybox run` still in the foreground that the
// Runner can move to the background.
func (state Status) CanBackground() bool {
	return state.Mode == "pipe" && state.Following && !state.Background && !state.Done
}

// Report compares what a `lazybox run` gave its agent with what the command
// printed. The exit status is the command's own, reported by the CLI that ran
// it: a pipe's capture alone never observes one.
type Report struct {
	ExitCode    int    `json:"exitCode"`
	OutputBytes uint64 `json:"outputBytes"`
	AgentBytes  uint64 `json:"agentBytes"`
	Filter      string `json:"filter,omitempty"`
	OutputPath  string `json:"outputPath,omitempty"`
	// AgentView is the compact view itself, kept for a workspace run so a
	// person can hand it to a conversation. Bounded by MaxAgentView.
	AgentView string `json:"agentView,omitempty"`
	// Piped: the output came through `lazybox log`, so ExitCode is not the
	// producer's.
	Piped bool `json:"piped,omitempty"`
}

// MaxAgentView bounds the compact view a report may carry: gortk bounds its
// own view to 32 KiB, so this leaves room for the header and notes.
const MaxAgentView = 64 << 10

type Output struct {
	Type    string  `json:"type"`
	Channel string  `json:"channel,omitempty"`
	Offset  uint64  `json:"offset"`
	Data    []byte  `json:"data,omitempty"`
	Status  *Status `json:"status,omitempty"`
	// Host is the session host running a managed command (on "registered").
	Host string `json:"host,omitempty"`
	// Seq is the host's sequence number of this output (on "output"): the
	// cursor a re-attaching Runner resumes after.
	Seq uint64 `json:"seq,omitempty"`
}

type Command struct {
	mu       sync.Mutex
	status   Status
	spool    *journal.Journal
	process  *processguard.Guard
	finished chan struct{}
	once     sync.Once

	// A hosted command runs in a session host and outlives the Runner.
	host        *sessionhost.Client
	hostID      string
	hostRoot    string
	hostBackend sessionhost.Backend
	seq         uint64
	detaching   bool
	following   chan struct{}

	// A followed pipe: what the Runner asks of its `lazybox run`, the channel
	// closed to wake a waiting FollowPipe, and when the run was last heard.
	stopRequested       bool
	backgroundRequested bool
	wake                chan struct{}
	lastSeen            time.Time
	// lastPublished is when a pipe last received output (or was
	// registered): an unfollowed pipe that stays silent is ended
	// (pipeIdle).
	lastPublished time.Time
}

type Manager struct {
	mu       sync.Mutex
	dir      string
	limit    int64
	maxLive  int
	hostRoot string
	backend  sessionhost.Backend
	items    map[string]*Command
	closed   bool
	// starting counts hosted starts between registration and their first
	// follow: shutdown waits for them, or it would see a command with no host
	// yet and miss the program that starts a moment later.
	starting sync.WaitGroup
	// quit ends the watch on followed pipes (watchFollowers).
	quit chan struct{}
}

// followSilence is how long a followed pipe may go without hearing from its
// `lazybox run` (which asks at least every 25 seconds) before it is ended.
var followSilence = 90 * time.Second

// pipeIdle is how long an unfollowed pipe (`lazybox log`, a terminal's run)
// may receive nothing before it is ended: abandoned registrations do not
// hold live slots for ever. One that never received anything goes sooner
// than a quiet log that did.
var pipeIdle = struct{ Never, After time.Duration }{Never: 15 * time.Minute, After: 6 * time.Hour}

// MaxOpenPipes bounds the pipes one execution, or one terminal, has open at
// once: each holds a live slot of the registry and a capture file.
const MaxOpenPipes = 16

// beforeSpawn is a test seam: it runs just before a hosted command spawns.
var beforeSpawn = func() {}

func New(dir string, perCommandBudget int64) (*Manager, error) {
	return NewWithLimit(dir, perCommandBudget, 0)
}

// NewWithLimit bounds live commands and capture streams. Zero selects the
// existing 128-command default; negative values are invalid, never unlimited.
// Its managed commands stop with the Runner.
func NewWithLimit(dir string, perCommandBudget int64, maxLive int) (*Manager, error) {
	return NewHosted(dir, perCommandBudget, maxLive, "")
}

// NewHosted runs every managed command in its own session host under hostRoot,
// so a command outlives the Runner that started it. It re-attaches to the hosts
// of commands a previous Runner left running and resumes their capture exactly
// after the last chunk it recorded. If opening fails, the commands it had
// already re-attached are detached again, never stopped: a Runner that cannot
// start does not take running work down with it.
func NewHosted(dir string, perCommandBudget int64, maxLive int, hostRoot string) (*Manager, error) {
	return NewHostedOn(dir, perCommandBudget, maxLive, hostRoot, nil)
}

// NewHostedOn is NewHosted with the backend that starts new command hosts (the
// default session host when nil). Hosts any backend started are re-attached.
func NewHostedOn(dir string, perCommandBudget int64, maxLive int, hostRoot string, backend sessionhost.Backend) (*Manager, error) {
	if hostRoot != "" && !filepath.IsAbs(hostRoot) {
		return nil, errors.New("command host root must be absolute")
	}
	if maxLive < 0 {
		return nil, errors.New("live command limit must not be negative")
	}
	if maxLive == 0 {
		maxLive = 128
	}
	if perCommandBudget < 4096 {
		return nil, errors.New("command budget must be >=4096 bytes")
	}
	if err := os.MkdirAll(dir, 0700); err != nil {
		return nil, err
	}
	m := &Manager{dir: dir, limit: perCommandBudget, maxLive: maxLive, hostRoot: hostRoot, backend: sessionhost.Use(backend), items: map[string]*Command{}, quit: make(chan struct{})}
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".jsonl") {
			continue
		}
		spool, err := journal.OpenExisting(filepath.Join(dir, entry.Name()), perCommandBudget)
		if err != nil {
			m.Detach()
			return nil, err
		}
		var state Status
		var host string
		var seq uint64
		for cursor := uint64(0); cursor < spool.Count(); cursor++ {
			record, err := spool.Next(context.Background(), cursor)
			if err != nil {
				spool.Dispose()
				m.Detach()
				return nil, err
			}
			var output Output
			if err = json.Unmarshal(record.Value, &output); err != nil {
				spool.Dispose()
				m.Detach()
				return nil, err
			}
			if output.Status != nil {
				state = *output.Status
			}
			if output.Host != "" {
				host = output.Host
			}
			if output.Type == "output" {
				state.Offset = output.Offset + uint64(len(output.Data))
				seq = max(seq, output.Seq)
			}
		}
		if state.ID == "" {
			spool.Dispose()
			continue
		}
		if !state.Done && host != "" {
			spool.Dispose()
			if spool, err = journal.Open(filepath.Join(dir, entry.Name()), perCommandBudget); err != nil {
				m.Detach()
				return nil, err
			}
			c := &Command{spool: spool, status: state, finished: make(chan struct{}), hostID: host, hostRoot: hostRoot, hostBackend: m.backend, seq: seq}
			m.items[state.ID] = c
			c.adopt()
			continue
		}
		if !state.Done && state.Mode == "pipe" && state.Following {
			// A `lazybox run` that follows the Runner keeps publishing after
			// the restart; if it stopped too, watchFollowers ends the pipe.
			spool.Dispose()
			if spool, err = journal.Open(filepath.Join(dir, entry.Name()), perCommandBudget); err != nil {
				m.Detach()
				return nil, err
			}
			m.items[state.ID] = &Command{spool: spool, status: state, finished: make(chan struct{}), wake: make(chan struct{}), lastSeen: time.Now()}
			continue
		}
		if !state.Done {
			state.Done = true
			state.Error = "command ownership unconfirmed after Runner restart"
		}
		c := &Command{spool: spool, status: state, finished: make(chan struct{})}
		close(c.finished)
		c.once.Do(func() {})
		m.items[state.ID] = c
	}
	// The limits are read here, not in the goroutine: a test that shortens them
	// for its own Manager would otherwise race with every earlier Manager's watcher.
	go m.watchFollowers(followSilence, pipeIdle)
	return m, nil
}

// watchFollowers ends a followed pipe whose `lazybox run` went silent: its
// CLI was killed, or the machine slept through it.
func (m *Manager) watchFollowers(silence time.Duration, idleLimits struct{ Never, After time.Duration }) {
	tick := time.NewTicker(silence / 6)
	defer tick.Stop()
	for {
		select {
		case <-m.quit:
			return
		case <-tick.C:
		}
		m.mu.Lock()
		var silent, idle []*Command
		for _, c := range m.items {
			c.mu.Lock()
			if c.status.Following && !c.status.Done && !c.detaching && time.Since(c.lastSeen) > silence {
				silent = append(silent, c)
			}
			if c.status.Mode == "pipe" && !c.status.Following && !c.status.Done && !c.detaching && !c.lastPublished.IsZero() {
				limit := idleLimits.After
				if c.status.Offset == 0 {
					limit = idleLimits.Never
				}
				if time.Since(c.lastPublished) > limit {
					idle = append(idle, c)
				}
			}
			c.mu.Unlock()
		}
		m.mu.Unlock()
		for _, c := range silent {
			c.complete(nil, "", errors.New("its lazybox run stopped reporting: the command may still be running outside Lazybox"), nil)
		}
		for _, c := range idle {
			c.complete(nil, "", errors.New("nothing was published to it for a long time, so Lazybox closed it; publish again with a new lazybox log"), nil)
		}
	}
}

func ID() string { return rand.Text() }

func (m *Manager) Register(spec Spec) (*Command, error) { return m.register(spec, nil) }

// admitLocked refuses a command the registry cannot take now.
func (m *Manager) admitLocked() error {
	active := 0
	for _, c := range m.items {
		state := c.Status()
		if !state.Done || state.CleanupUnconfirmed() {
			active++
		}
	}
	if m.closed || active >= m.maxLive {
		return errors.New("command registry closed or capacity exhausted")
	}
	return nil
}

// admitPipeLocked refuses a pipe past MaxOpenPipes open for its execution
// or its terminal.
func (m *Manager) admitPipeLocked(spec Spec) error {
	if len(spec.Argv) > 0 || (spec.SessionID == "" && spec.TerminalID == "") {
		return nil
	}
	open := 0
	for _, c := range m.items {
		c.mu.Lock()
		s := c.status
		c.mu.Unlock()
		if s.Mode != "pipe" || s.Done {
			continue
		}
		if (spec.SessionID != "" && s.Spec.SessionID == spec.SessionID) || (spec.TerminalID != "" && s.Spec.TerminalID == spec.TerminalID) {
			open++
		}
	}
	if open >= MaxOpenPipes {
		return fmt.Errorf("%d consoles are already open here; let one finish (or stop it) before starting another", MaxOpenPipes)
	}
	return nil
}

func (m *Manager) register(spec Spec, host *sessionhost.Client) (*Command, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if err := m.admitLocked(); err != nil {
		return nil, err
	}
	if spec.Kind == KindRunner {
		if spec.SessionID != "" || spec.CollectionID != "" || len(spec.Argv) == 0 || spec.TerminalID != "" || spec.ShareWith != "" || len(spec.Title) > 256 {
			return nil, errors.New("a Runner command is a program of no session or workspace, with a bounded title")
		}
	} else if (spec.SessionID == "") == (spec.CollectionID == "") || len(spec.Title) > 256 {
		return nil, errors.New("a session or a workspace (not both) and a bounded title required")
	}
	if spec.SessionID != "" && (spec.TerminalID != "" || spec.ShareWith != "") {
		return nil, errors.New("only a workspace run comes from a terminal or is shared")
	}
	// A hook run (POWER.md) is a program or, for a webhook, a pipe the Runner
	// writes the request's log to.
	if spec.CollectionID != "" && spec.Kind != KindHook && (len(spec.Argv) > 0) != (spec.Kind != "") {
		return nil, errors.New("a workspace run is a pipe, a workspace command has a kind")
	}
	if spec.Kind != "" && (spec.Kind != "setup" && spec.Kind != "archive" && spec.Kind != "action" && spec.Kind != KindHook && spec.Kind != KindRunner || spec.TerminalID != "" || spec.ShareWith != "") {
		return nil, errors.New("invalid workspace command")
	}
	if spec.StopGraceMs < 0 || spec.StopGraceMs > 60_000 {
		return nil, errors.New("stop grace must be within a minute")
	}
	if err := m.admitPipeLocked(spec); err != nil {
		return nil, err
	}
	id := ID()
	spool, err := journal.Open(filepath.Join(m.dir, id+".jsonl"), m.limit)
	if err != nil {
		return nil, err
	}
	c := &Command{spool: spool, finished: make(chan struct{}), status: Status{ID: id, Spec: spec, Mode: "pipe", StartedAt: time.Now().UnixMilli()}, hostRoot: m.hostRoot, hostBackend: m.backend, wake: make(chan struct{})}
	if len(spec.Argv) > 0 {
		c.status.Mode = "managed"
	} else {
		c.lastPublished = time.Now()
	}
	if host != nil {
		c.hostID = host.ID()
	}
	if _, err = spool.Append(Output{Type: "registered", Status: ptr(c.status), Host: c.hostID}); err != nil {
		spool.Dispose()
		return nil, err
	}
	if host != nil {
		m.starting.Add(1) // Under m.mu with closed checked: shutdown cannot miss it.
	}
	m.items[id] = c
	return c, nil
}

func ptr(s Status) *Status {
	s.Spec.Env = nil
	if s.Report != nil {
		report := *s.Report
		s.Report = &report
	}
	return &s
}
func (m *Manager) Get(id string) (*Command, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	c, ok := m.items[id]
	if !ok {
		return nil, os.ErrNotExist
	}
	return c, nil
}
func (m *Manager) List(session string) []Status {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := []Status{}
	for _, c := range m.items {
		s := c.Status()
		if session == "" || s.Spec.SessionID == session {
			out = append(out, s)
		}
	}
	return out
}

// ListWorkspace returns a workspace's runs: pipes published from its
// terminals, which belong to no single conversation.
func (m *Manager) ListWorkspace(collectionID string) []Status {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := []Status{}
	for _, c := range m.items {
		if s := c.Status(); collectionID != "" && s.Spec.SessionID == "" && s.Spec.CollectionID == collectionID && s.Spec.Kind != KindHook {
			out = append(out, s)
		}
	}
	return out
}

// ClaimShare takes a shared run for one message box. Only the first call
// returns true, across Runner restarts: the claim is a file created once.
func (m *Manager) ClaimShare(id string) (bool, error) {
	c, err := m.Get(id)
	if err != nil {
		return false, err
	}
	state := c.Status()
	if state.Spec.ShareWith == "" || !state.Done {
		return false, nil
	}
	f, err := os.OpenFile(filepath.Join(m.dir, id+".shared"), os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
	if errors.Is(err, os.ErrExist) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	return true, f.Close()
}
func (c *Command) Status() Status { c.mu.Lock(); defer c.mu.Unlock(); return *ptr(c.status) }

func (c *Command) write(channel string, data []byte, expected *uint64) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.detaching {
		return errors.New("the Runner is restarting; publish again in a moment")
	}
	if c.status.Done || c.status.Error != "" {
		return errors.New("command capture is complete or failed")
	}
	if len(data) > 32<<10 {
		return errors.New("output chunk exceeds 32 KiB")
	}
	if expected != nil && *expected != c.status.Offset {
		return fmt.Errorf("output offset mismatch: retained %d", c.status.Offset)
	}
	if _, err := c.spool.Append(Output{Type: "output", Channel: channel, Offset: c.status.Offset, Data: data}); err != nil {
		c.status.Error = "capture failed: " + err.Error()
		return err
	}
	c.status.Offset += uint64(len(data))
	if c.status.Mode == "pipe" {
		c.lastPublished = time.Now()
	}
	return nil
}

func (c *Command) Publish(offset uint64, data []byte) error {
	if c.Status().Mode != "pipe" {
		return errors.New("managed command cannot accept pipe publication")
	}
	return c.write("merged", data, &offset)
}

func (c *Command) finish(exit *int, signal string, cause error) { c.complete(exit, signal, cause, nil) }

func (c *Command) complete(exit *int, signal string, cause error, report *Report) {
	c.completeThen(exit, signal, cause, report, nil)
}

// completeThen runs recorded, if any, once the completion is in the journal
// and before waiters see the command done.
func (c *Command) completeThen(exit *int, signal string, cause error, report *Report, recorded func()) {
	c.once.Do(func() {
		c.mu.Lock()
		c.status.Done = true
		c.status.ExitCode = exit
		c.status.Signal = signal
		c.status.Report = report
		if cause != nil {
			c.status.Error = cause.Error()
		}
		if _, err := c.spool.Append(Output{Type: "completed", Offset: c.status.Offset, Status: ptr(c.status)}); err != nil && c.status.Error == "" {
			c.status.Error = "capture completion failed: " + err.Error()
		}
		c.mu.Unlock()
		_ = c.spool.Close()
		if recorded != nil {
			recorded()
		}
		close(c.finished)
	})
}

// FinishPipe establishes capture EOF, never an upstream process exit status.
func (c *Command) FinishPipe(cause error) error { return c.FinishPipeWith(cause, nil) }

// FinishPipeWith also records what a `lazybox run` gave its agent. The report
// is kept beside the status, not as its exit code: the Runner did not observe
// the process.
func (c *Command) FinishPipeWith(cause error, report *Report) error {
	return c.FinishPipeSignalled(cause, "", report)
}

// FinishPipeSignalled is FinishPipeWith for a run whose command a signal ended
// (a stop), as its `lazybox run` saw it.
func (c *Command) FinishPipeSignalled(cause error, signal string, report *Report) error {
	if c.Status().Mode != "pipe" {
		return errors.New("managed command owns its completion")
	}
	if report != nil {
		copied := *report
		report = &copied
		if len(report.AgentView) > MaxAgentView {
			return errors.New("the agent view exceeds 64 KiB")
		}
	}
	if len(signal) > 32 {
		return errors.New("invalid signal name")
	}
	c.complete(nil, signal, cause, report)
	return nil
}

// FollowUpdate is what a `lazybox run` reports when it follows its pipe.
type FollowUpdate struct {
	Background bool
	PID        int
	Ports      []uint32
}

// FollowReply is what the Runner asks of it: stop the command, or move it to
// the background; Offset is the output the Runner holds.
type FollowReply struct {
	Stop, Background bool
	Offset           uint64
}

// MaxFollowWait bounds one FollowPipe wait.
const MaxFollowWait = 30 * time.Second

// ErrRestarting means the Runner is handing its commands to the next one: a
// `lazybox run` asks again in a moment.
var ErrRestarting = errors.New("the Runner is restarting; ask again in a moment")

// ErrEnded means Lazybox ended the pipe (it was stopped, its conversation
// deleted, or the Runner closed): nobody follows the command any more.
var ErrEnded = errors.New("command capture is complete or failed")

// Follow records a `lazybox run`'s state and waits up to wait for a request
// (CancelCommand, RequestBackground). The first call makes the pipe followed.
func (c *Command) Follow(ctx context.Context, update FollowUpdate, wait time.Duration) (FollowReply, error) {
	if len(update.Ports) > 64 || update.PID < 0 {
		return FollowReply{}, errors.New("invalid run state")
	}
	wait = min(max(wait, 0), MaxFollowWait)
	c.mu.Lock()
	if c.status.Mode != "pipe" {
		c.mu.Unlock()
		return FollowReply{}, errors.New("only a pipe is followed")
	}
	if c.detaching {
		c.mu.Unlock()
		return FollowReply{}, ErrRestarting
	}
	if c.status.Done {
		c.mu.Unlock()
		return FollowReply{}, ErrEnded
	}
	if c.wake == nil {
		c.wake = make(chan struct{})
	}
	c.lastSeen = time.Now()
	if update.Background {
		c.backgroundRequested = false
	}
	ports := append([]uint32(nil), update.Ports...)
	slices.Sort(ports)
	if !c.status.Following || c.status.Background != update.Background || c.status.PID != update.PID || !slices.Equal(c.status.Ports, ports) {
		c.status.Following, c.status.Background, c.status.PID, c.status.Ports = true, update.Background, update.PID, ports
		if _, err := c.spool.Append(Output{Type: "status", Offset: c.status.Offset, Status: ptr(c.status)}); err != nil {
			c.mu.Unlock()
			return FollowReply{}, err
		}
	}
	reply := func() FollowReply {
		return FollowReply{Stop: c.stopRequested, Background: c.backgroundRequested && !c.status.Background, Offset: c.status.Offset}
	}
	if r := reply(); r.Stop || r.Background || wait == 0 {
		c.mu.Unlock()
		return r, nil
	}
	wake := c.wake
	c.mu.Unlock()
	timer := time.NewTimer(wait)
	defer timer.Stop()
	select {
	case <-wake:
	case <-c.finished:
	case <-timer.C:
	case <-ctx.Done():
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.lastSeen = time.Now()
	return reply(), nil
}

// RequestBackground asks a foreground `lazybox run` to move to the background.
func (c *Command) RequestBackground() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if !c.status.CanBackground() {
		return errors.New("only a lazybox run still in the foreground can move to the background")
	}
	c.backgroundRequested = true
	c.wakeLocked()
	return nil
}

func (c *Command) wakeLocked() {
	if c.wake != nil {
		close(c.wake)
	}
	c.wake = make(chan struct{})
}

// PID is the managed command's process (its group leader), or what a
// followed run reported; 0 when not known.
func (c *Command) PID() int {
	c.mu.Lock()
	defer c.mu.Unlock()
	switch {
	case c.host != nil:
		return c.host.AgentPID()
	case c.process != nil:
		return c.process.PID()
	}
	return c.status.PID
}

type writer struct {
	c       *Command
	channel string
}

func (w writer) Write(p []byte) (int, error) {
	total := 0
	for len(p) > 0 {
		n := len(p)
		if n > 32<<10 {
			n = 32 << 10
		}
		if err := w.c.write(w.channel, p[:n], nil); err != nil {
			_ = w.c.Cancel()
			return total, err
		}
		total += n
		p = p[n:]
	}
	return total, nil
}

func (m *Manager) Start(spec Spec) (*Command, error) {
	if len(spec.Argv) == 0 || !filepath.IsAbs(spec.Cwd) {
		return nil, errors.New("managed command requires argv and absolute cwd")
	}
	if m.hostRoot != "" {
		return m.startHosted(spec)
	}
	c, err := m.Register(spec)
	if err != nil {
		return nil, err
	}
	cmd := exec.Command(spec.Argv[0], spec.Argv[1:]...)
	cmd.Dir = spec.Cwd
	cmd.Env = spec.Env
	cmd.Stdout = writer{c, "stdout"}
	cmd.Stderr = writer{c, "stderr"}
	// Bound inherited descriptors after the direct child exits. Cancellation
	// terminates the entire process group, including background grandchildren.
	cmd.WaitDelay = time.Second
	// Close must either observe an owned process or prevent this start.
	// Registration and process creation are separate fallible steps.
	m.mu.Lock()
	if m.closed {
		m.mu.Unlock()
		c.finish(nil, "", errors.New("runner closed before command start"))
		return c, nil
	}
	c.mu.Lock()
	guard, err := processguard.Start(cmd)
	if err == nil {
		c.process = guard
	}
	c.mu.Unlock()
	m.mu.Unlock()
	if err != nil {
		c.finish(nil, "", fmt.Errorf("start failed: %w", err))
		return c, nil
	}
	go func() {
		outcome, err := guard.Wait()
		c.finish(exitCode(outcome), outcome.Signal, err)
	}()
	return c, nil
}

// startHosted records the command with its host before the host runs anything,
// so a Runner that dies at any point leaves a record its successor can adopt.
func (m *Manager) startHosted(spec Spec) (*Command, error) {
	// Refuse before launching a host that registration would then refuse.
	m.mu.Lock()
	err := m.admitLocked()
	m.mu.Unlock()
	if err != nil {
		return nil, err
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	host, launchErr := m.backend.Launch(ctx, m.hostRoot, sessionhost.Options{Kind: "command", Owner: spec.SessionID, Label: Label(spec)})
	c, err := m.register(spec, host)
	if err != nil {
		if host != nil {
			_ = m.backend.Stop(ctx, m.hostRoot, host.ID())
		}
		return nil, err
	}
	if host != nil {
		defer m.starting.Done()
	}
	if launchErr != nil {
		c.finish(nil, "", fmt.Errorf("start failed: %w", launchErr))
		return c, nil
	}
	m.mu.Lock()
	closed := m.closed
	m.mu.Unlock()
	if closed {
		_ = m.backend.Stop(ctx, m.hostRoot, host.ID())
		c.finish(nil, "", errors.New("runner closed before command start"))
		return c, nil
	}
	beforeSpawn()
	err = host.Spawn(ctx, sessionhost.Spec{Path: spec.Argv[0], Args: spec.Argv[1:], Env: spec.Env, Cwd: spec.Cwd, Stream: true, StopGraceMs: spec.StopGraceMs})
	if err != nil {
		_ = m.backend.Stop(ctx, m.hostRoot, host.ID())
		c.finish(nil, "", fmt.Errorf("start failed: %w", err))
		return c, nil
	}
	c.follow(host)
	return c, nil
}

// adopt re-attaches to the host a previous Runner left running. A command whose
// host cannot be adopted is stopped; if even that fails its cleanup is unconfirmed.
func (c *Command) adopt() {
	if c.hostRoot == "" {
		c.finish(nil, "", processguard.ErrCleanupUnconfirmed)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	host, err := c.hostBackend.Attach(ctx, c.hostRoot, c.hostID, c.seq)
	var cause string
	switch {
	case err == nil && host.Spawned():
		c.follow(host)
		return
	case err == nil:
		_ = host.Detach()
		cause = "command did not start before the Runner restarted"
	case sessionhost.Gone(err):
		// A host's exit kills the command's group (processguard): the command
		// is gone, and its exit status with it.
		_ = c.hostBackend.Forget(c.hostRoot, c.hostID)
		c.finish(nil, "", errors.New("command outcome unknown: its session host exited while no Runner was attached"))
		return
	default:
		cause = "command stopped: its session host could not be adopted after a Runner restart"
	}
	if c.hostBackend.Stop(ctx, c.hostRoot, c.hostID) != nil {
		cause = processguard.ErrCleanupUnconfirmed.Error()
	}
	c.finish(nil, "", errors.New(cause))
}

// follow records the host's chunks, acknowledging each only once it is in the
// command's journal, until the command exits or the Runner detaches.
func (c *Command) follow(host *sessionhost.Client) {
	c.mu.Lock()
	c.host = host
	c.following = make(chan struct{})
	done := c.following
	c.mu.Unlock()
	go func() {
		defer close(done)
		for {
			m, exit, err := host.Next(context.Background())
			if err != nil {
				c.mu.Lock()
				detaching := c.detaching
				superseded := errors.Is(err, sessionhost.ErrSuperseded)
				c.mu.Unlock()
				if superseded {
					// Another Runner adopted the command: it owns the journal now.
					c.handOver()
					return
				}
				if !detaching {
					// The host vanished: nothing confirms the command's group stopped.
					c.finish(nil, "", processguard.ErrCleanupUnconfirmed)
				}
				return
			}
			if exit != nil {
				var cause error
				if exit.Outcome.Error != "" {
					cause = errors.New(exit.Outcome.Error)
				}
				// The outcome is recorded before the host may discard it. The host
				// exits on its own once released, a moment later; its record goes
				// first, so that once the command is done no inventory, stop or
				// orphan sweep finds the host still listed.
				retired := false
				retire := func() {
					_ = host.Release()
					_ = host.Detach()
					_ = c.hostBackend.Forget(c.hostRoot, host.ID())
					retired = true
				}
				c.completeThen(exitCode(processguard.Outcome(exit.Outcome)), exit.Outcome.Signal, cause, nil, retire)
				if !retired {
					retire() // Completed earlier on another path.
				}
				return
			}
			var chunk sessionhost.Chunk
			if err = json.Unmarshal(m.Line, &chunk); err != nil || (chunk.Channel != "stdout" && chunk.Channel != "stderr") {
				_ = host.Terminate()
				continue
			}
			if err = c.record(chunk.Channel, chunk.Data, m.Seq); err != nil {
				_ = host.Terminate()
				continue
			}
			_ = host.Ack(m.Seq)
		}
	}()
}

// record appends one hosted chunk, skipping one this journal already holds.
func (c *Command) record(channel string, data []byte, seq uint64) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if seq <= c.seq {
		return nil
	}
	if c.status.Done || c.status.Error != "" {
		return errors.New("command capture is complete or failed")
	}
	if _, err := c.spool.Append(Output{Type: "output", Channel: channel, Offset: c.status.Offset, Data: data, Seq: seq}); err != nil {
		c.status.Error = "capture failed: " + err.Error()
		return err
	}
	c.status.Offset += uint64(len(data))
	c.seq = seq
	return nil
}

// exitCode is the program's own exit status, if it ran.
func exitCode(o processguard.Outcome) *int {
	if o.Started || o.Cancelled {
		return &o.ExitCode
	}
	return nil
}

// handOver ends this Runner's interest in a command another Runner adopted.
// The other Runner owns the journal now, so nothing is written to it; the
// command just stops counting here and waiting on it returns.
func (c *Command) handOver() {
	c.once.Do(func() {
		c.mu.Lock()
		c.detaching = true
		c.status.Done = true
		c.status.Error = "command is owned by another Runner"
		c.mu.Unlock()
		_ = c.spool.Close()
		close(c.finished)
	})
}

// detach leaves a hosted command running for the next Runner.
func (c *Command) detach() {
	c.mu.Lock()
	host, done := c.host, c.following
	c.detaching = true
	c.mu.Unlock()
	if host == nil {
		return
	}
	_ = host.Detach()
	<-done
	_ = c.spool.Close()
}

func (c *Command) Cancel() error {
	c.mu.Lock()
	done, unconfirmed, detaching := c.status.Done, c.status.CleanupUnconfirmed(), c.detaching
	host, process := c.host, c.process
	c.mu.Unlock()
	// Stopping happens outside the lock: a host write can wait on the network,
	// and Status, List and capture must not wait with it.
	switch {
	case done && unconfirmed:
		return processguard.ErrCleanupUnconfirmed
	case done:
		return nil
	case detaching:
		return errors.New("command is owned by another Runner")
	case host == nil && process == nil && c.Status().Following:
		// A `lazybox run` stops its own command when it next asks (FollowPipe).
		c.mu.Lock()
		c.stopRequested = true
		c.wakeLocked()
		c.mu.Unlock()
		return nil
	case host != nil:
		// The host applies the command's grace period itself, so a stop
		// in progress completes even if this Runner goes away.
		return host.Terminate()
	case process == nil:
		return errors.New("pipe capture has no owned process")
	}
	if grace := c.Status().Spec.StopGraceMs; grace > 0 && process.Interrupt() == nil {
		go func() {
			select {
			case <-c.finished:
			case <-time.After(time.Duration(grace) * time.Millisecond):
				_ = process.Stop()
			}
		}()
		return nil
	}
	return process.Stop()
}
func (c *Command) Wait(ctx context.Context) (Status, error) {
	select {
	case <-ctx.Done():
		return c.Status(), ctx.Err()
	case <-c.finished:
		return c.Status(), nil
	}
}
func (c *Command) Next(ctx context.Context, cursor uint64) (journal.Record, error) {
	r, err := c.spool.Next(ctx, cursor)
	if err == io.EOF {
		s := c.Status()
		if s.Error != "" {
			return r, errors.New(s.Error)
		}
	}
	return r, err
}

// Read preserves byte offsets even when a reader resumes in the middle of a
// chunk. Returning a bounded slice does not discard the remaining evidence.
func (c *Command) Read(ctx context.Context, offset uint64, limit int) ([]byte, Status, error) {
	s := c.Status()
	if offset > s.Offset {
		return nil, s, journal.ErrCursor
	}
	if limit <= 0 || limit > 64<<10 {
		limit = 64 << 10
	}
	out := []byte{}
	count := c.spool.Count()
	for cursor := uint64(0); cursor < count && len(out) < limit; cursor++ {
		r, err := c.spool.Next(ctx, cursor)
		if err != nil {
			return nil, s, err
		}
		var chunk Output
		if err = json.Unmarshal(r.Value, &chunk); err != nil {
			return nil, s, err
		}
		if chunk.Type != "output" || chunk.Offset+uint64(len(chunk.Data)) <= offset {
			continue
		}
		start := uint64(0)
		if offset > chunk.Offset {
			start = offset - chunk.Offset
		}
		data := chunk.Data[start:]
		if len(data) > limit-len(out) {
			data = data[:limit-len(out)]
		}
		out = append(out, data...)
	}
	return out, s, nil
}

// Tail returns up to limit bytes of the command's latest output, reading the
// journal backwards from its end so a long-running server costs nothing extra.
func (c *Command) Tail(ctx context.Context, limit int) []byte {
	var chunks [][]byte
	total := 0
	for cursor := c.spool.Count(); cursor > 0 && total < limit; cursor-- {
		r, err := c.spool.Next(ctx, cursor-1)
		if err != nil {
			break
		}
		var chunk Output
		if json.Unmarshal(r.Value, &chunk) != nil || chunk.Type != "output" {
			continue
		}
		chunks = append(chunks, chunk.Data)
		total += len(chunk.Data)
	}
	out := make([]byte, 0, total)
	for i := len(chunks) - 1; i >= 0; i-- {
		out = append(out, chunks[i]...)
	}
	if len(out) > limit {
		out = out[len(out)-limit:]
	}
	return out
}

// Close stops every managed command, hosted or not.
func (m *Manager) Close() error { return m.shutdown(false) }

// Detach shuts the manager down but leaves hosted commands running for the
// next Runner, which re-attaches in NewHosted. Commands without a host stop.
func (m *Manager) Detach() error { return m.shutdown(true) }

func (m *Manager) shutdown(detach bool) error {
	m.mu.Lock()
	if !m.closed && m.quit != nil {
		close(m.quit)
	}
	m.closed = true
	m.mu.Unlock()
	m.starting.Wait()
	m.mu.Lock()
	items := make([]*Command, 0, len(m.items))
	for _, c := range m.items {
		items = append(items, c)
	}
	m.mu.Unlock()
	var failures []error
	for _, c := range items {
		if c.Status().Mode != "managed" {
			if state := c.Status(); detach && state.Following && !state.Done {
				// Its `lazybox run` keeps the command; the next Runner reopens the pipe.
				c.mu.Lock()
				c.detaching = true
				c.wakeLocked()
				c.mu.Unlock()
				_ = c.spool.Close()
				continue
			}
			_ = c.FinishPipe(errors.New("runner closed"))
			continue
		}
		c.mu.Lock()
		hosted, superseded := c.host != nil, c.detaching
		c.mu.Unlock()
		if superseded {
			continue
		}
		if detach && hosted && !c.Status().Done {
			c.detach()
			continue
		}
		if err := c.Cancel(); err != nil {
			failures = append(failures, err)
		}
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		state, err := c.Wait(ctx)
		if err != nil {
			failures = append(failures, err)
		} else if state.CleanupUnconfirmed() {
			failures = append(failures, processguard.ErrCleanupUnconfirmed)
		}
		cancel()
	}
	return errors.Join(failures...)
}

// CleanupUnconfirmed reports a missing process-guard cleanup receipt.
// Completion of output alone is not evidence that owned resources stopped.
func (state Status) CleanupUnconfirmed() bool {
	return state.Error == processguard.ErrCleanupUnconfirmed.Error()
}
