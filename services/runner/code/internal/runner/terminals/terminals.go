// Package terminals runs a human's interactive shells in workspaces. Each
// terminal is a pseudo-terminal in its own session host (internal/sessionhost),
// so the shell outlives a Runner restart exactly as agents and managed commands
// do; the next Runner re-attaches and receives the host's scrollback.
//
// A terminal is started only by a person through the authenticated Runner API,
// never by an agent tool. Its output is a view, not evidence: unlike a managed
// command it is not journalled, and only a bounded scrollback is kept.
package terminals

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// Spec is what a terminal runs and where. Env is never persisted.
type Spec struct {
	CollectionID string `json:"collectionId"`
	// Directory is the workspace-relative directory ("" for the root).
	Directory string   `json:"directory"`
	Cwd       string   `json:"cwd"`
	Title     string   `json:"title"`
	Shell     string   `json:"shell"`
	Env       []string `json:"-"`
	Cols      uint16   `json:"cols"`
	Rows      uint16   `json:"rows"`
	// Prepare runs once the terminal has its id, before the shell starts, and
	// returns variables added to Env (the terminal's own Runner credential).
	Prepare func(id string) ([]string, error) `json:"-"`
	// Place is "" in the workspace; "repository-home" (RepositoryID's home
	// checkout) or "user-home" for a shell started outside it, at Cwd
	// (runner/homes.go). Directory is then "".
	Place        string `json:"place,omitempty"`
	RepositoryID string `json:"repositoryId,omitempty"`
}

// Status is a terminal's state. Output [Offset-Retained, Offset) is readable.
type Status struct {
	ID        string    `json:"id"`
	Spec      Spec      `json:"spec"`
	CreatedAt time.Time `json:"createdAt"`
	Offset    uint64    `json:"offset"`
	Retained  uint64    `json:"retained"`
	Done      bool      `json:"done"`
	ExitCode  *int      `json:"exitCode,omitempty"`
	Signal    string    `json:"signal,omitempty"`
	Error     string    `json:"error,omitempty"`

	// CommandEnds counts the command lines the shell said ended (its OSC
	// 133;D;<status> mark, agentshell's zsh integration) in the output this
	// Runner has seen, and LastCommandStatus is the latest one's status: how
	// the window learns that a command it typed for the person finished,
	// without following the output itself.
	CommandEnds       uint64 `json:"commandEnds,omitempty"`
	LastCommandStatus int    `json:"lastCommandStatus,omitempty"`
}

// Options bound a manager. Zero values select the defaults.
type Options struct {
	// MaxLive bounds running terminals; 16 if zero.
	MaxLive int
	// Scrollback bounds the output kept per terminal, by the Runner and by
	// its host; sessionhost.DefaultScrollback if zero.
	Scrollback int
	// Backend starts new terminal hosts; the default session host if nil.
	// Hosts any backend started are re-attached.
	Backend sessionhost.Backend
}

// record is a running terminal's private file: what a later Runner needs to
// find its host again.
type record struct {
	ID        string    `json:"id"`
	Host      string    `json:"host"`
	Spec      Spec      `json:"spec"`
	CreatedAt time.Time `json:"createdAt"`
}

// ReadLimit bounds the output returned by one Next.
const ReadLimit = 64 << 10

// hostBuffer bounds the output a terminal host holds for a Runner that is not
// reading. The scrollback, not this buffer, is what a re-attaching Runner gets.
const hostBuffer = 4 << 20

type Manager struct {
	mu       sync.Mutex
	dir      string
	hostRoot string
	opts     Options
	backend  sessionhost.Backend
	items    map[string]*Terminal
	keys     map[string]string
	closed   bool
	pending  int
	starting sync.WaitGroup
}

type Terminal struct {
	m *Manager
	// hostID is the session host running the shell, fixed at start.
	hostID   string
	mu       sync.Mutex
	status   Status
	buf      []byte
	changed  chan struct{}
	host     *sessionhost.Client
	finished chan struct{}
	once     sync.Once
	// detaching: this Runner is leaving the shell running for the next one.
	detaching bool
	following chan struct{}
	// markTail is an unfinished escape at the end of the output, which a
	// command-end mark may complete in the next chunk.
	markTail []byte
}

// commandEnd is the shell's mark that a command line ended, with its status.
var commandEnd = regexp.MustCompile("\x1b\\]133;D;([0-9]{1,3})(?:\x07|\x1b\\\\)")

// countCommandEnds counts the command-end marks in data into s and returns
// what to carry into the next chunk: an escape that may be a mark cut short.
func countCommandEnds(s *Status, data []byte) []byte {
	last := 0
	for _, m := range commandEnd.FindAllSubmatchIndex(data, -1) {
		s.CommandEnds++
		s.LastCommandStatus, _ = strconv.Atoi(string(data[m[2]:m[3]]))
		last = m[1]
	}
	rest := data[last:]
	if i := bytes.LastIndexByte(rest, 0x1b); i >= 0 && len(rest)-i < 24 {
		return append([]byte(nil), rest[i:]...)
	}
	return nil
}

// Open re-attaches every terminal a previous Runner left running. One whose
// host is gone is forgotten (the host's exit hung up its shell); one whose host
// cannot be adopted is stopped.
func Open(dir, hostRoot string, opts Options) (*Manager, error) {
	if !filepath.IsAbs(dir) || !filepath.IsAbs(hostRoot) {
		return nil, errors.New("terminal state and host roots must be absolute")
	}
	if opts.MaxLive < 0 || opts.Scrollback < 0 {
		return nil, errors.New("terminal limits must not be negative")
	}
	if opts.MaxLive == 0 {
		opts.MaxLive = 16
	}
	if opts.Scrollback == 0 {
		opts.Scrollback = sessionhost.DefaultScrollback
	}
	if err := os.MkdirAll(dir, 0700); err != nil {
		return nil, err
	}
	m := &Manager{dir: dir, hostRoot: hostRoot, opts: opts, backend: sessionhost.Use(opts.Backend), items: map[string]*Terminal{}, keys: map[string]string{}}
	entries, err := os.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	for _, entry := range entries {
		if entry.IsDir() || !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		data, err := os.ReadFile(filepath.Join(dir, entry.Name()))
		var r record
		if err != nil || json.Unmarshal(data, &r) != nil || r.ID+".json" != entry.Name() || r.Host == "" {
			_ = os.Remove(filepath.Join(dir, entry.Name()))
			continue
		}
		m.adopt(r)
	}
	return m, nil
}

func (m *Manager) adopt(r record) {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	host, err := m.backend.Attach(ctx, m.hostRoot, r.Host, 0)
	switch {
	case err == nil && host.Spawned():
		t := m.newTerminal(r)
		data, produced := host.Scrollback()
		t.append(data, produced)
		m.items[r.ID] = t
		t.follow(host)
		return
	case err == nil:
		_ = host.Detach()
		_ = m.backend.Stop(ctx, m.hostRoot, r.Host)
	case sessionhost.Gone(err):
		_ = m.backend.Forget(m.hostRoot, r.Host)
	default:
		_ = m.backend.Stop(ctx, m.hostRoot, r.Host)
	}
	_ = os.Remove(m.recordPath(r.ID))
}

func (m *Manager) newTerminal(r record) *Terminal {
	return &Terminal{m: m, hostID: r.Host, status: Status{ID: r.ID, Spec: r.Spec, CreatedAt: r.CreatedAt}, changed: make(chan struct{}), finished: make(chan struct{})}
}

func (m *Manager) recordPath(id string) string { return filepath.Join(m.dir, id+".json") }

func (m *Manager) save(r record) error {
	data, err := json.Marshal(r)
	if err != nil {
		return err
	}
	f, err := os.CreateTemp(m.dir, ".terminal-*")
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
	return os.Rename(f.Name(), m.recordPath(r.ID))
}

// ValidSize reports whether a terminal size is within bounds.
func ValidSize(cols, rows uint16) bool {
	return cols > 0 && rows > 0 && cols <= 1000 && rows <= 500
}

// Start runs spec.Shell as a login shell in a new terminal host. A repeated
// non-empty requestKey returns the terminal it already started.
func (m *Manager) Start(spec Spec, requestKey string) (*Terminal, error) {
	if !filepath.IsAbs(spec.Shell) || !filepath.IsAbs(spec.Cwd) || spec.CollectionID == "" {
		return nil, errors.New("terminal requires an absolute shell, an absolute directory and a workspace")
	}
	if !ValidSize(spec.Cols, spec.Rows) {
		return nil, errors.New("terminal size must be 1-1000 columns and 1-500 rows")
	}
	if len(requestKey) > 128 || len(spec.Title) > 256 {
		return nil, errors.New("terminal request key or title too long")
	}
	m.mu.Lock()
	if id, ok := m.keys[requestKey]; ok && requestKey != "" {
		t := m.items[id]
		m.mu.Unlock()
		if t == nil {
			return nil, errors.New("terminal for this request was closed")
		}
		return t, nil
	}
	// Starts still in flight count, so concurrent opens cannot pass the limit.
	live := m.pending
	for _, t := range m.items {
		if !t.Status().Done {
			live++
		}
	}
	if m.closed || live >= m.opts.MaxLive {
		m.mu.Unlock()
		return nil, fmt.Errorf("runner closed or terminal limit (%d) reached", m.opts.MaxLive)
	}
	m.pending++
	m.starting.Add(1)
	m.mu.Unlock()
	defer func() {
		m.mu.Lock()
		m.pending--
		m.mu.Unlock()
		m.starting.Done()
	}()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	host, err := m.backend.Launch(ctx, m.hostRoot, sessionhost.Options{BufferLimit: hostBuffer, Scrollback: m.opts.Scrollback, Kind: "terminal", Owner: spec.CollectionID, Label: Label(spec)})
	if err != nil {
		return nil, fmt.Errorf("terminal host failed to start: %w", err)
	}
	r := record{ID: rand.Text(), Host: host.ID(), Spec: spec, CreatedAt: time.Now().UTC()}
	r.Spec.Env, r.Spec.Prepare = nil, nil
	// Recorded before the shell runs: a Runner that dies at any point leaves a
	// record its successor adopts or cleans up.
	if err = m.save(r); err != nil {
		_ = m.backend.Stop(ctx, m.hostRoot, host.ID())
		return nil, err
	}
	fail := func(err error) (*Terminal, error) {
		_ = m.backend.Stop(ctx, m.hostRoot, host.ID())
		_ = os.Remove(m.recordPath(r.ID))
		return nil, err
	}
	m.mu.Lock()
	closed := m.closed
	m.mu.Unlock()
	if closed {
		return fail(errors.New("runner closed before the terminal started"))
	}
	env := spec.Env
	if spec.Prepare != nil {
		extra, err := spec.Prepare(r.ID)
		if err != nil {
			return fail(fmt.Errorf("terminal credential: %w", err))
		}
		env = append(append([]string(nil), env...), extra...)
	}
	err = host.Spawn(ctx, sessionhost.Spec{Path: spec.Shell, Argv0: "-" + filepath.Base(spec.Shell), Env: env, Cwd: spec.Cwd, Terminal: &sessionhost.Size{Cols: spec.Cols, Rows: spec.Rows}})
	if err != nil {
		return fail(fmt.Errorf("shell failed to start: %w", err))
	}
	t := m.newTerminal(r)
	m.mu.Lock()
	m.items[r.ID] = t
	if requestKey != "" {
		m.keys[requestKey] = r.ID
	}
	m.mu.Unlock()
	t.follow(host)
	return t, nil
}

// Get returns a terminal this Runner knows.
func (m *Manager) Get(id string) (*Terminal, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	t, ok := m.items[id]
	if !ok {
		return nil, os.ErrNotExist
	}
	return t, nil
}

// List returns the terminals of a workspace (all if empty), oldest first.
func (m *Manager) List(collectionID string) []Status {
	m.mu.Lock()
	items := make([]*Terminal, 0, len(m.items))
	for _, t := range m.items {
		items = append(items, t)
	}
	m.mu.Unlock()
	out := []Status{}
	for _, t := range items {
		if s := t.Status(); collectionID == "" || s.Spec.CollectionID == collectionID {
			out = append(out, s)
		}
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].CreatedAt.Equal(out[j].CreatedAt) {
			return out[i].ID < out[j].ID
		}
		return out[i].CreatedAt.Before(out[j].CreatedAt)
	})
	return out
}

// Discard hangs a terminal up, waits for its shell to exit, and forgets it.
func (m *Manager) Discard(ctx context.Context, id string) (Status, error) {
	t, err := m.Get(id)
	if err != nil {
		return Status{}, err
	}
	if err = t.Close(); err != nil {
		return t.Status(), err
	}
	state, err := t.Wait(ctx)
	if err != nil {
		return state, err
	}
	m.mu.Lock()
	delete(m.items, id)
	for key, value := range m.keys {
		if value == id {
			delete(m.keys, key)
		}
	}
	m.mu.Unlock()
	return state, nil
}

// DiscardCollection discards every terminal of a workspace.
func (m *Manager) DiscardCollection(ctx context.Context, collectionID string) error {
	var errs []error
	for _, s := range m.List(collectionID) {
		if _, err := m.Discard(ctx, s.ID); err != nil {
			errs = append(errs, fmt.Errorf("terminal %s: %w", s.ID, err))
		}
	}
	return errors.Join(errs...)
}

// Close hangs up every terminal and waits for their shells.
func (m *Manager) Close() error { return m.shutdown(false) }

// Detach leaves every running shell in its host for the next Runner.
func (m *Manager) Detach() error { return m.shutdown(true) }

func (m *Manager) shutdown(detach bool) error {
	m.mu.Lock()
	m.closed = true
	m.mu.Unlock()
	m.starting.Wait()
	m.mu.Lock()
	items := make([]*Terminal, 0, len(m.items))
	for _, t := range m.items {
		items = append(items, t)
	}
	m.mu.Unlock()
	var errs []error
	for _, t := range items {
		if t.Status().Done {
			continue
		}
		if detach {
			t.detach()
			continue
		}
		if err := t.Close(); err != nil {
			errs = append(errs, err)
		}
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		if _, err := t.Wait(ctx); err != nil {
			errs = append(errs, fmt.Errorf("terminal %s did not stop: %w", t.Status().ID, err))
		}
		cancel()
	}
	return errors.Join(errs...)
}

// Status returns a copy of the terminal's state.
func (t *Terminal) Status() Status {
	t.mu.Lock()
	defer t.mu.Unlock()
	return t.status
}

func (t *Terminal) broadcastLocked() {
	close(t.changed)
	t.changed = make(chan struct{})
}

// append adds output that ends at offset end, keeping at most the scrollback.
func (t *Terminal) append(data []byte, end uint64) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if start := end - uint64(len(data)); start != t.status.Offset {
		// Only the first snapshot after adoption starts later than zero.
		t.buf = nil
	}
	t.buf = append(t.buf, data...)
	t.markTail = countCommandEnds(&t.status, append(t.markTail, data...))
	if limit := t.m.opts.Scrollback; len(t.buf) > 2*limit {
		t.buf = append([]byte(nil), t.buf[len(t.buf)-limit:]...)
	}
	t.status.Offset = end
	t.status.Retained = uint64(min(len(t.buf), t.m.opts.Scrollback))
	t.broadcastLocked()
}

// follow relays the host's output into the scrollback until the shell exits,
// another Runner adopts the terminal, or this one detaches.
func (t *Terminal) follow(host *sessionhost.Client) {
	t.mu.Lock()
	t.host = host
	t.following = make(chan struct{})
	done := t.following
	t.mu.Unlock()
	go func() {
		defer close(done)
		for {
			msg, exit, err := host.Next(context.Background())
			if err != nil {
				t.mu.Lock()
				detaching := t.detaching
				t.mu.Unlock()
				switch {
				case errors.Is(err, sessionhost.ErrSuperseded):
					// The other Runner owns its record now.
					t.finish(nil, "", errors.New("terminal is owned by another Runner"), false)
				case !detaching:
					t.finish(nil, "", errors.New("terminal host exited unexpectedly"), true)
				}
				return
			}
			if exit != nil {
				var cause error
				if exit.Outcome.Error != "" {
					cause = errors.New(exit.Outcome.Error)
				}
				code := exit.Outcome.ExitCode
				// The host exits on its own once released, a moment later. Its
				// record goes first, so that once the terminal is done no
				// inventory, stop or orphan sweep finds the host still listed.
				_ = host.Release()
				_ = host.Detach()
				_ = t.m.backend.Forget(t.m.hostRoot, host.ID())
				t.finish(&code, exit.Outcome.Signal, cause, true)
				return
			}
			var chunk sessionhost.Chunk
			if json.Unmarshal(msg.Line, &chunk) != nil || chunk.Channel != sessionhost.TerminalChannel {
				continue
			}
			t.append(chunk.Data, t.Status().Offset+uint64(len(chunk.Data)))
			_ = host.Ack(msg.Seq)
		}
	}()
}

func (t *Terminal) finish(exit *int, signal string, cause error, forget bool) {
	t.once.Do(func() {
		t.mu.Lock()
		t.status.Done = true
		t.status.ExitCode = exit
		t.status.Signal = signal
		if cause != nil {
			t.status.Error = cause.Error()
		}
		t.broadcastLocked()
		t.mu.Unlock()
		if forget {
			_ = os.Remove(t.m.recordPath(t.status.ID))
		}
		close(t.finished)
	})
}

func (t *Terminal) detach() {
	t.mu.Lock()
	host, done := t.host, t.following
	t.detaching = true
	t.mu.Unlock()
	if host == nil {
		return
	}
	_ = host.Detach()
	<-done
}

func (t *Terminal) live() (*sessionhost.Client, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	if t.status.Done || t.host == nil || t.detaching {
		return nil, errors.New("terminal has exited")
	}
	return t.host, nil
}

// Write sends keyboard input to the shell.
func (t *Terminal) Write(data []byte) error {
	if len(data) > sessionhost.InputLimit {
		return errors.New("terminal input exceeds 64 KiB")
	}
	host, err := t.live()
	if err != nil {
		return err
	}
	return host.Input(data)
}

// Resize changes the terminal's size.
func (t *Terminal) Resize(cols, rows uint16) error {
	if !ValidSize(cols, rows) {
		return errors.New("terminal size must be 1-1000 columns and 1-500 rows")
	}
	host, err := t.live()
	if err != nil {
		return err
	}
	if err = host.Resize(sessionhost.Size{Cols: cols, Rows: rows}); err != nil {
		return err
	}
	t.mu.Lock()
	t.status.Spec.Cols, t.status.Spec.Rows = cols, rows
	t.mu.Unlock()
	return nil
}

// Close hangs the terminal up; Wait observes the shell's exit.
func (t *Terminal) Close() error {
	t.mu.Lock()
	done, host := t.status.Done, t.host
	t.mu.Unlock()
	if done || host == nil {
		return nil
	}
	return host.Terminate()
}

// Wait blocks until the shell exits.
func (t *Terminal) Wait(ctx context.Context) (Status, error) {
	select {
	case <-ctx.Done():
		return t.Status(), ctx.Err()
	case <-t.finished:
		return t.Status(), nil
	}
}

// Next returns retained output after the byte offset, waiting for some if
// there is none yet. Output older than the scrollback is skipped: the returned
// offset says where the data starts. io.EOF means the shell exited and
// everything was read.
func (t *Terminal) Next(ctx context.Context, after uint64) ([]byte, uint64, Status, error) {
	for {
		t.mu.Lock()
		status, changed := t.status, t.changed
		if after < status.Offset {
			start := status.Offset - uint64(len(t.buf))
			from := max(after, status.Offset-status.Retained)
			data := t.buf[from-start:]
			if len(data) > ReadLimit {
				data = data[:ReadLimit]
			}
			data = append([]byte(nil), data...)
			t.mu.Unlock()
			return data, from, status, nil
		}
		t.mu.Unlock()
		if after > status.Offset {
			return nil, after, status, errors.New("terminal offset is beyond its output")
		}
		if status.Done {
			return nil, after, status, io.EOF
		}
		select {
		case <-ctx.Done():
			return nil, after, status, ctx.Err()
		case <-changed:
		}
	}
}
