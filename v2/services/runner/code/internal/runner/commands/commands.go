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
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/processguard"
	"github.com/AntoineToussaint/lazybox/v2/internal/runner/journal"
	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

type Spec struct {
	SessionID string   `json:"sessionId"`
	Title     string   `json:"title"`
	Cwd       string   `json:"cwd"`
	Argv      []string `json:"argv,omitempty"`
	Env       []string `json:"-"`
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
}

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
	host      *sessionhost.Client
	hostID    string
	hostRoot  string
	seq       uint64
	detaching bool
	following chan struct{}
}

type Manager struct {
	mu       sync.Mutex
	dir      string
	limit    int64
	maxLive  int
	hostRoot string
	items    map[string]*Command
	closed   bool
	// starting counts hosted starts between registration and their first
	// follow: shutdown waits for them, or it would see a command with no host
	// yet and miss the program that starts a moment later.
	starting sync.WaitGroup
}

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
	m := &Manager{dir: dir, limit: perCommandBudget, maxLive: maxLive, hostRoot: hostRoot, items: map[string]*Command{}}
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
			c := &Command{spool: spool, status: state, finished: make(chan struct{}), hostID: host, hostRoot: hostRoot, seq: seq}
			m.items[state.ID] = c
			c.adopt()
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
	return m, nil
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

func (m *Manager) register(spec Spec, host *sessionhost.Client) (*Command, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if err := m.admitLocked(); err != nil {
		return nil, err
	}
	if spec.SessionID == "" || len(spec.Title) > 256 {
		return nil, errors.New("session and bounded title required")
	}
	id := ID()
	spool, err := journal.Open(filepath.Join(m.dir, id+".jsonl"), m.limit)
	if err != nil {
		return nil, err
	}
	c := &Command{spool: spool, finished: make(chan struct{}), status: Status{ID: id, Spec: spec, Mode: "pipe"}, hostRoot: m.hostRoot}
	if len(spec.Argv) > 0 {
		c.status.Mode = "managed"
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

func ptr(s Status) *Status { s.Spec.Env = nil; return &s }
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
func (c *Command) Status() Status { c.mu.Lock(); defer c.mu.Unlock(); return *ptr(c.status) }

func (c *Command) write(channel string, data []byte, expected *uint64) error {
	c.mu.Lock()
	defer c.mu.Unlock()
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
	return nil
}

func (c *Command) Publish(offset uint64, data []byte) error {
	if c.Status().Mode != "pipe" {
		return errors.New("managed command cannot accept pipe publication")
	}
	return c.write("merged", data, &offset)
}

func (c *Command) finish(exit *int, signal string, cause error) {
	c.once.Do(func() {
		c.mu.Lock()
		c.status.Done = true
		c.status.ExitCode = exit
		c.status.Signal = signal
		if cause != nil {
			c.status.Error = cause.Error()
		}
		if _, err := c.spool.Append(Output{Type: "completed", Offset: c.status.Offset, Status: ptr(c.status)}); err != nil && c.status.Error == "" {
			c.status.Error = "capture completion failed: " + err.Error()
		}
		c.mu.Unlock()
		_ = c.spool.Close()
		close(c.finished)
	})
}

// FinishPipe establishes capture EOF, never an upstream process exit status.
func (c *Command) FinishPipe(cause error) error {
	if c.Status().Mode != "pipe" {
		return errors.New("managed command owns its completion")
	}
	c.finish(nil, "", cause)
	return nil
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
	host, launchErr := sessionhost.Launch(ctx, m.hostRoot, sessionhost.Options{})
	c, err := m.register(spec, host)
	if err != nil {
		if host != nil {
			_ = sessionhost.Stop(ctx, m.hostRoot, host.ID())
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
		_ = sessionhost.Stop(ctx, m.hostRoot, host.ID())
		c.finish(nil, "", errors.New("runner closed before command start"))
		return c, nil
	}
	beforeSpawn()
	err = host.Spawn(ctx, sessionhost.Spec{Path: spec.Argv[0], Args: spec.Argv[1:], Env: spec.Env, Cwd: spec.Cwd, Stream: true})
	if err != nil {
		_ = sessionhost.Stop(ctx, m.hostRoot, host.ID())
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
	host, err := sessionhost.Attach(ctx, c.hostRoot, c.hostID, c.seq)
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
		_ = sessionhost.Forget(c.hostRoot, c.hostID)
		c.finish(nil, "", errors.New("command outcome unknown: its session host exited while no Runner was attached"))
		return
	default:
		cause = "command stopped: its session host could not be adopted after a Runner restart"
	}
	if sessionhost.Stop(ctx, c.hostRoot, c.hostID) != nil {
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
				c.finish(exitCode(processguard.Outcome(exit.Outcome)), exit.Outcome.Signal, cause)
				_ = host.Release()
				_ = host.Detach()
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
	case host != nil:
		return host.Terminate()
	case process == nil:
		return errors.New("pipe capture has no owned process")
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

// Close stops every managed command, hosted or not.
func (m *Manager) Close() error { return m.shutdown(false) }

// Detach shuts the manager down but leaves hosted commands running for the
// next Runner, which re-attaches in NewHosted. Commands without a host stop.
func (m *Manager) Detach() error { return m.shutdown(true) }

func (m *Manager) shutdown(detach bool) error {
	m.mu.Lock()
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
