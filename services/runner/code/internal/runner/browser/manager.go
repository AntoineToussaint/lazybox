// Package browser is the Runner's side of the agent browser (BROWSER.md): a
// Chromium the browser service (workers/browser) runs under the Runner, which
// an agent drives through its browser_* tools and the person watches live.
//
// The Runner owns everything that is policy or record: which sites an agent
// may open without asking, the person's answers, who holds the browser (the
// agent, or the person after Take over), Stop, and the action log the person
// watches. The service owns Chromium and refuses blocked addresses on every
// request a page makes. One conversation has one browser (a page); one
// workspace has one profile (a persistent browser context), so sign-ins made
// in a workspace stay in it and nowhere else — never in the person's own
// browser.
package browser

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

// Config locates the browser service and bounds its waits.
type Config struct {
	// Node runs Script, the service's built main.js. An empty Script means
	// this Runner has no agent browser.
	Node, Script string
	// Root holds profiles, downloads and the Runner's browser records; owner-only.
	Root string
	// BlockedHosts are host:port pairs no page may reach: the Runner and its gateway.
	BlockedHosts []string
	// LoopbackPorts returns the ports of a workspace's running previews, the
	// only addresses on this computer its agent browser may reach.
	LoopbackPorts func(ctx context.Context, workspace string) []int
	// Env is the service's environment; nil selects a minimal one (no credentials).
	Env []string
	// AskTimeout bounds a wait for the person's answer; zero selects 10 minutes.
	AskTimeout time.Duration
	// HoldTimeout bounds an agent's wait while the person holds the browser;
	// zero selects 10 minutes.
	HoldTimeout time.Duration
	Now         func() time.Time
}

// Caller is the execution calling a browser tool, as the Runner knows it.
type Caller struct {
	Session, Conversation, Workspace string
	// Permission is the conversation's mode: ask, read-only, yolo or "".
	Permission string
	// Agent names who browses for the person: Claude, Codex.
	Agent string
}

// ErrNotFound: no such browser request.
var ErrNotFound = errors.New("not found")

// ErrUnavailable: this Runner cannot run the agent browser. An installed
// release never can for now (it bundles neither the service nor Chromium);
// the message says so to the agent, which tells the person.
var ErrUnavailable = errors.New("the agent browser is not available on this Runner: for now it is a developer-build feature, not part of the installed Lazybox app. Tell the person you cannot open a browser here, and carry on without it")

const (
	maxLog      = 300
	maxPending  = 8
	viewportW   = 1280
	viewportH   = 800
	actTimeout  = 2 * time.Minute
	openTimeout = 60 * time.Second
)

// Manager runs the browser service and every conversation's browser.
type Manager struct {
	cfg    Config
	ctx    context.Context
	cancel context.CancelFunc

	mu       sync.Mutex
	settings *settingsStore
	browsers map[string]*conv
	requests map[string]*pending
	saved    map[string]savedConv
	proc     *jsonl.Process
	readers  sync.WaitGroup
	// install is the latest Chromium install command (InstallID).
	install string
}

// conv is one conversation's browser.
type conv struct {
	id, session, workspace, agent, permission string
	status, controller, problem               string
	page                                      PageState
	freely, stopped, used, opened             bool
	granted                                   map[string]bool
	// once is the origin allowed for the current visit ("Allow once").
	once string
	// lastURL reopens the page after the browser closed underneath the agent.
	lastURL string
	log     []Action
	seq     uint64
	current *Action
	frame   *Frame
	frames  uint64
	// heldByPerson: the person took the browser since the agent's last call.
	heldByPerson bool
	// notes are things that happened since the agent's last call, told with its next result.
	notes    []string
	watchers map[*Watcher]struct{}
	// changed is closed and replaced when the controller or Stop changes.
	changed chan struct{}
	updated time.Time
	// busy serializes the agent's calls on this browser.
	busy sync.Mutex
}

// savedConv is what a conversation's browser keeps across Runner restarts.
type savedConv struct {
	Freely  bool     `json:"freely,omitempty"`
	Stopped bool     `json:"stopped,omitempty"`
	Granted []string `json:"granted,omitempty"`
	LastURL string   `json:"lastUrl,omitempty"`
}

type pending struct {
	Request
	conv   string
	answer chan answer
}

type answer struct {
	allow  bool
	reason string
}

// New opens the browser records under cfg.Root. The service starts on the
// first browser an agent opens.
func New(cfg Config) (*Manager, error) {
	if cfg.Root == "" || !filepath.IsAbs(cfg.Root) {
		return nil, errors.New("browser root must be an absolute directory")
	}
	if cfg.Node == "" {
		cfg.Node = "node"
	}
	if cfg.Now == nil {
		cfg.Now = time.Now
	}
	if cfg.AskTimeout == 0 {
		cfg.AskTimeout = 10 * time.Minute
	}
	if cfg.HoldTimeout == 0 {
		cfg.HoldTimeout = 10 * time.Minute
	}
	if cfg.LoopbackPorts == nil {
		cfg.LoopbackPorts = func(context.Context, string) []int { return nil }
	}
	if err := os.MkdirAll(cfg.Root, 0o700); err != nil {
		return nil, err
	}
	settings, err := openSettings(filepath.Join(cfg.Root, "settings.json"))
	if err != nil {
		return nil, err
	}
	saved := map[string]savedConv{}
	if data, err := os.ReadFile(filepath.Join(cfg.Root, "conversations.json")); err == nil {
		if err = json.Unmarshal(data, &saved); err != nil {
			return nil, fmt.Errorf("browser conversations: %w", err)
		}
	} else if !errors.Is(err, fs.ErrNotExist) {
		return nil, err
	}
	ctx, cancel := context.WithCancel(context.Background())
	return &Manager{cfg: cfg, ctx: ctx, cancel: cancel, settings: settings, browsers: map[string]*conv{}, requests: map[string]*pending{}, saved: saved}, nil
}

// Available reports whether agents can be given the browser tools.
func (m *Manager) Available() bool { return m != nil && m.cfg.Script != "" }

// Enabled reports whether agents opened now should get the browser tools.
func (m *Manager) Enabled() bool {
	if !m.Available() {
		return false
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.settings.current.DefaultPermission != PermissionOff
}

// Close stops the service and every browser.
func (m *Manager) Close() error {
	m.cancel()
	m.mu.Lock()
	p := m.proc
	m.proc = nil
	for _, r := range m.requests {
		select {
		case r.answer <- answer{reason: "the Runner is stopping"}:
		default:
		}
	}
	m.mu.Unlock()
	var err error
	if p != nil {
		err = p.Close()
	}
	m.readers.Wait()
	return err
}

// ───────────────────────── conversations ─────────────────────────

// browserLocked returns c's browser, creating it, and records who drives it now.
func (m *Manager) browserLocked(c Caller) *conv {
	b := m.browsers[c.Conversation]
	if b == nil {
		s := m.saved[c.Conversation]
		b = &conv{id: c.Conversation, status: "closed", controller: "agent", freely: s.Freely, stopped: s.Stopped, lastURL: s.LastURL, granted: map[string]bool{}, watchers: map[*Watcher]struct{}{}, changed: make(chan struct{})}
		for _, site := range s.Granted {
			b.granted[site] = true
		}
		if b.stopped {
			b.status, b.problem = "stopped", "You stopped this browser."
		}
		m.browsers[c.Conversation] = b
	}
	if c.Session != "" {
		b.session = c.Session
	}
	if c.Workspace != "" {
		b.workspace = c.Workspace
	}
	if c.Agent != "" {
		b.agent = c.Agent
	}
	b.permission = c.Permission
	return b
}

// persistLocked records b's lasting choices.
func (m *Manager) persistLocked(b *conv) error {
	s := savedConv{Freely: b.freely, Stopped: b.stopped, LastURL: b.lastURL}
	for site := range b.granted {
		s.Granted = append(s.Granted, site)
	}
	slices.Sort(s.Granted)
	next := maps.Clone(m.saved)
	if s.Freely || s.Stopped || s.LastURL != "" || len(s.Granted) > 0 {
		next[b.id] = s
	} else {
		delete(next, b.id)
	}
	if err := writePrivate(filepath.Join(m.cfg.Root, "conversations.json"), next); err != nil {
		return err
	}
	m.saved = next
	return nil
}

// Forget closes a deleted conversation's browser and drops its records.
func (m *Manager) Forget(conversation string) {
	m.mu.Lock()
	b := m.browsers[conversation]
	delete(m.browsers, conversation)
	if _, ok := m.saved[conversation]; ok {
		delete(m.saved, conversation)
		_ = writePrivate(filepath.Join(m.cfg.Root, "conversations.json"), m.saved)
	}
	if safeName(conversation) {
		// What the agent downloaded in it goes with the conversation.
		_ = os.RemoveAll(filepath.Join(m.cfg.Root, "downloads", conversation))
	}
	var opened bool
	if b != nil {
		opened = b.opened
		m.denyLocked(b.id, "the conversation was deleted")
		for w := range b.watchers {
			w.end()
		}
	}
	p := m.proc
	m.mu.Unlock()
	if opened && p != nil {
		ctx, cancel := context.WithTimeout(m.ctx, 10*time.Second)
		defer cancel()
		_, _ = p.Call(ctx, "close", map[string]string{"browser": conversation})
	}
}

// stateLocked is b's state as the person sees it.
func (m *Manager) stateLocked(b *conv) State {
	s := State{Conversation: b.id, Session: b.session, Workspace: b.workspace, Status: b.status, Controller: b.controller, URL: b.page.URL, Title: b.page.Title, Loading: b.page.Loading,
		CanGoBack: b.page.CanGoBack, CanGoForward: b.page.CanGoForward, BrowseFreely: b.freely, Problem: b.problem, ViewportWidth: b.page.ViewportWidth, ViewportHeight: b.page.ViewportHeight,
		Updated: b.updated, Agent: b.agent}
	if s.URL == "" && !b.opened {
		s.URL = b.lastURL
	}
	if b.current != nil {
		current := *b.current
		s.Current = &current
	}
	for _, r := range m.requests {
		if r.conv == b.id {
			s.Waiting = true
		}
	}
	return s
}

// State is the conversation's browser, when it has been used.
func (m *Manager) State(conversation string) (State, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	b := m.browsers[conversation]
	if b == nil || (!b.used && !b.stopped) {
		return State{}, false
	}
	return m.stateLocked(b), true
}

// Pending are the requests waiting for the person in a conversation.
func (m *Manager) Pending(conversation string) []Request {
	m.mu.Lock()
	defer m.mu.Unlock()
	out := []Request{}
	for _, r := range m.requests {
		if r.conv == conversation {
			out = append(out, r.Request)
		}
	}
	slices.SortFunc(out, func(a, b Request) int { return a.Created.Compare(b.Created) })
	return out
}

// RequestConversation names the conversation a request belongs to.
func (m *Manager) RequestConversation(id string) (string, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	r := m.requests[id]
	if r == nil {
		return "", fmt.Errorf("%w: browser request %s", ErrNotFound, id)
	}
	return r.conv, nil
}

// ───────────────────────── watching ─────────────────────────

// Watcher follows one conversation's browser. Frames are latest-wins; a
// watcher that cannot keep up with state and actions is ended (Done) and
// reconnects.
type Watcher struct {
	events chan Event
	frames chan Frame
	done   chan struct{}
	once   sync.Once
}

func (w *Watcher) Events() <-chan Event  { return w.events }
func (w *Watcher) Frames() <-chan Frame  { return w.frames }
func (w *Watcher) Done() <-chan struct{} { return w.done }
func (w *Watcher) end()                  { w.once.Do(func() { close(w.done) }) }
func (w *Watcher) send(e Event) {
	select {
	case w.events <- e:
	default:
		w.end()
	}
}
func (w *Watcher) frame(f Frame) {
	for {
		select {
		case w.frames <- f:
			return
		default:
		}
		select {
		case <-w.frames:
		default:
		}
	}
}

// Watch follows c's browser: the first event carries its state, its recent
// log and its last frame. Stop with the returned function.
func (m *Manager) Watch(c Caller) (*Watcher, Event, func()) {
	m.mu.Lock()
	b := m.browserLocked(c)
	w := &Watcher{events: make(chan Event, 256), frames: make(chan Frame, 1), done: make(chan struct{})}
	b.watchers[w] = struct{}{}
	state := m.stateLocked(b)
	first := Event{State: &state, History: slices.Clone(b.log)}
	if b.frame != nil {
		f := *b.frame
		first.Frame = &f
	}
	start := len(b.watchers) == 1 && b.opened
	m.mu.Unlock()
	if start {
		m.screencast(b.id, true)
	}
	return w, first, func() {
		m.mu.Lock()
		delete(b.watchers, w)
		stop := len(b.watchers) == 0 && b.opened
		m.mu.Unlock()
		w.end()
		if stop {
			m.screencast(b.id, false)
		}
	}
}

func (m *Manager) screencast(browser string, on bool) {
	m.mu.Lock()
	p := m.proc
	m.mu.Unlock()
	if p == nil {
		return
	}
	go func() {
		ctx, cancel := context.WithTimeout(m.ctx, 10*time.Second)
		defer cancel()
		_, _ = p.Call(ctx, "screencast", map[string]any{"browser": browser, "on": on})
	}()
}

// changedLocked tells b's watchers its state changed.
func (m *Manager) changedLocked(b *conv) {
	b.updated = m.cfg.Now()
	state := m.stateLocked(b)
	for w := range b.watchers {
		w.send(Event{State: &state})
	}
}

// logLocked appends (or, with the same seq, updates) an action and tells the watchers.
func (m *Manager) logLocked(b *conv, a Action) Action {
	if a.Seq == 0 {
		b.seq++
		a.Seq = b.seq
		if a.At.IsZero() {
			a.At = m.cfg.Now()
		}
		b.log = append(b.log, a)
		if len(b.log) > maxLog {
			b.log = slices.Delete(b.log, 0, len(b.log)-maxLog)
		}
	} else {
		for i := len(b.log) - 1; i >= 0; i-- {
			if b.log[i].Seq == a.Seq {
				b.log[i] = a
				break
			}
		}
	}
	b.used = true
	if a.Status == "running" {
		current := a
		b.current = &current
	} else if b.current != nil && b.current.Seq == a.Seq {
		b.current = nil
	}
	b.updated = m.cfg.Now()
	state := m.stateLocked(b)
	for w := range b.watchers {
		action := a
		w.send(Event{Action: &action, State: &state})
	}
	return a
}

// ───────────────────────── the service ─────────────────────────

// process returns the running service, starting it.
func (m *Manager) process() (*jsonl.Process, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.proc != nil {
		return m.proc, nil
	}
	if m.cfg.Script == "" {
		return nil, ErrUnavailable
	}
	if err := m.ctx.Err(); err != nil {
		return nil, err
	}
	env := m.cfg.Env
	if env == nil {
		env = serviceEnv(os.Environ())
	}
	p, err := jsonl.Start(m.ctx, m.cfg.Node, []string{m.cfg.Script}, env, m.cfg.Root)
	if err != nil {
		return nil, fmt.Errorf("start the browser service: %w", err)
	}
	m.proc = p
	m.readers.Add(1)
	go m.read(p)
	return p, nil
}

// serviceEnv passes what Node and Playwright need to find themselves and
// Chromium, and none of the person's credentials.
func serviceEnv(environ []string) []string {
	keep := []string{"PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "USER", "PLAYWRIGHT_BROWSERS_PATH", "XDG_CACHE_HOME"}
	out := []string{}
	for _, kv := range environ {
		name, _, _ := strings.Cut(kv, "=")
		if slices.Contains(keep, name) {
			out = append(out, kv)
		}
	}
	return out
}

// read handles the service's events and requests until it exits.
func (m *Manager) read(p *jsonl.Process) {
	defer m.readers.Done()
	for {
		msg, err := p.Receive(m.ctx)
		if err != nil {
			m.exited(p, err)
			return
		}
		if len(msg.ID) > 0 && msg.Method != "" {
			go m.serve(p, msg)
			continue
		}
		m.event(msg.Method, msg.Params)
	}
}

// exited marks every browser closed when the service went away.
func (m *Manager) exited(p *jsonl.Process, err error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if m.proc == p {
		m.proc = nil
	}
	for _, b := range m.browsers {
		if !b.opened {
			continue
		}
		b.opened, b.frame = false, nil
		if b.status == "open" || b.status == "starting" {
			b.status = "closed"
			if m.ctx.Err() == nil {
				b.problem = "The browser stopped unexpectedly; it reopens on the agent's next step."
				_ = err
			}
		}
		m.changedLocked(b)
	}
	_ = p.Close()
}

func (m *Manager) event(method string, params json.RawMessage) {
	var e struct {
		Browser string    `json:"browser"`
		Data    string    `json:"data"`
		Width   int       `json:"width"`
		Height  int       `json:"height"`
		State   PageState `json:"state"`
		Kind    string    `json:"kind"`
		Summary string    `json:"summary"`
		URL     string    `json:"url"`
		Reason  string    `json:"reason"`
	}
	if json.Unmarshal(params, &e) != nil {
		return
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	b := m.browsers[e.Browser]
	if b == nil {
		return
	}
	switch method {
	case "frame":
		data, err := base64.StdEncoding.DecodeString(e.Data)
		if err != nil {
			return
		}
		b.frames++
		f := Frame{JPEG: data, Width: e.Width, Height: e.Height, Seq: b.frames}
		b.frame = &f
		for w := range b.watchers {
			w.frame(f)
		}
	case "state":
		m.pageLocked(b, e.State)
		m.changedLocked(b)
	case "log":
		actor := "lazybox"
		if b.controller == "person" {
			actor = "person"
		}
		status := "done"
		if e.Kind == "blocked" {
			status = "blocked"
		}
		m.logLocked(b, Action{Kind: e.Kind, Actor: actor, Summary: e.Summary, URL: e.URL, Status: status})
		if actor != "person" {
			b.notes = append(b.notes, e.Summary)
			if len(b.notes) > 20 {
				b.notes = b.notes[len(b.notes)-20:]
			}
		}
	case "closed":
		b.opened, b.frame = false, nil
		if b.status == "open" || b.status == "starting" {
			b.status = "closed"
			b.problem = e.Reason
		}
		m.changedLocked(b)
	}
}

// pageLocked records what the page shows. A site the person went to
// themselves counts as their choice, so the agent may go on there once given
// back. Only the service tells which navigation the person started
// (PageState.Origin): a page that moved itself while they held the browser (a
// script, a meta refresh) is no choice of theirs, whatever its address.
func (m *Manager) pageLocked(b *conv, page PageState) {
	b.page = page
	if page.URL != "" && page.URL != "about:blank" {
		b.lastURL = page.URL
	}
	if b.controller == "person" && page.Origin != "" {
		if u, err := url.Parse(page.Origin); err == nil && (u.Scheme == "http" || u.Scheme == "https") && u.Host != "" {
			b.once = Origin(u)
		}
	}
}

// serve answers a request the service made: a site, a download or a sign-in.
func (m *Manager) serve(p *jsonl.Process, msg jsonl.Message) {
	var r struct {
		Browser  string `json:"browser"`
		URL      string `json:"url"`
		Origin   string `json:"origin"`
		FileName string `json:"fileName"`
	}
	if err := json.Unmarshal(msg.Params, &r); err != nil {
		_ = p.Reject(msg.ID, "invalid request")
		return
	}
	m.mu.Lock()
	b := m.browsers[r.Browser]
	var workspace string
	if b != nil {
		workspace = b.workspace
	}
	m.mu.Unlock()
	if b == nil {
		_ = p.Reply(msg.ID, map[string]any{"allow": false, "reason": "no such browser"})
		return
	}
	ctx := m.ctx
	switch msg.Method {
	case "site":
		u, err := m.parseURL(ctx, workspace, r.URL)
		if err != nil {
			_ = p.Reply(msg.ID, map[string]any{"allow": false, "reason": err.Error()})
			return
		}
		// The service asks only about navigations the person did not make: one
		// the page makes on its own while they hold the browser is asked about
		// like the agent's, never granted for holding it.
		m.mu.Lock()
		allowed := m.allowedLocked(b, u)
		m.mu.Unlock()
		if allowed {
			_ = p.Reply(msg.ID, map[string]any{"allow": true})
			return
		}
		a := m.ask(ctx, b, Request{Kind: "site", URL: u.String(), Origin: Origin(u)})
		_ = p.Reply(msg.ID, map[string]any{"allow": a.allow, "reason": a.reason})
	case "download":
		m.mu.Lock()
		block := m.settings.current.Downloads == DownloadsBlock
		m.mu.Unlock()
		name := safeFileName(r.FileName)
		if block {
			_ = p.Reply(msg.ID, map[string]any{"allow": false, "reason": "downloads are blocked in Settings → Browser"})
			return
		}
		a := m.ask(ctx, b, Request{Kind: "download", URL: r.URL, Origin: originOf(r.URL), FileName: name})
		if !a.allow {
			_ = p.Reply(msg.ID, map[string]any{"allow": false, "reason": a.reason})
			return
		}
		path, err := m.downloadPath(b.id, name)
		if err != nil {
			_ = p.Reply(msg.ID, map[string]any{"allow": false, "reason": err.Error()})
			return
		}
		m.mu.Lock()
		b.notes = append(b.notes, fmt.Sprintf("Downloaded %s to %s", name, path))
		m.mu.Unlock()
		_ = p.Reply(msg.ID, map[string]any{"allow": true, "path": path})
	case "sign_in":
		a := m.ask(ctx, b, Request{Kind: "sign_in", URL: r.URL, Origin: r.Origin})
		_ = p.Reply(msg.ID, map[string]any{"allow": a.allow, "reason": a.reason})
	default:
		_ = p.Reject(msg.ID, "unsupported request")
	}
}

func originOf(raw string) string {
	if u, err := url.Parse(raw); err == nil && u.Host != "" {
		return Origin(u)
	}
	return ""
}

var unsafeFile = regexp.MustCompile(`[^A-Za-z0-9._ -]+`)

func safeFileName(name string) string {
	name = strings.TrimSpace(unsafeFile.ReplaceAllString(filepath.Base(name), "_"))
	name = strings.TrimLeft(name, ".")
	if name == "" {
		name = "download"
	}
	if len(name) > 120 {
		name = name[:120]
	}
	return name
}

var safeID = regexp.MustCompile(`^[A-Za-z0-9._-]{1,128}$`)

func safeName(id string) bool { return safeID.MatchString(id) && id != "." && id != ".." }

// downloadPath is a new file under the conversation's downloads folder.
func (m *Manager) downloadPath(conversation, name string) (string, error) {
	if !safeName(conversation) {
		return "", errors.New("invalid conversation")
	}
	dir := filepath.Join(m.cfg.Root, "downloads", conversation)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	ext := filepath.Ext(name)
	base := strings.TrimSuffix(name, ext)
	for i := 0; i < 1000; i++ {
		candidate := name
		if i > 0 {
			candidate = fmt.Sprintf("%s (%d)%s", base, i, ext)
		}
		path := filepath.Join(dir, candidate)
		if _, err := os.Lstat(path); errors.Is(err, fs.ErrNotExist) {
			return path, nil
		}
	}
	return "", errors.New("too many downloads with that name")
}

// profileDir is the workspace's browser profile; a conversation outside any
// workspace has its own.
func (m *Manager) profileDir(b *conv) (string, error) {
	name := b.workspace
	if name == "" {
		name = "conversation-" + b.id
	}
	if !safeName(name) {
		return "", errors.New("invalid browser profile")
	}
	dir := filepath.Join(m.cfg.Root, "profiles", name)
	return dir, os.MkdirAll(dir, 0o700)
}

// ───────────────────────── asking the person ─────────────────────────

// allowedLocked: the agent may open u without asking.
func (m *Manager) allowedLocked(b *conv, u *url.URL) bool {
	return b.once == Origin(u) || m.siteGrantedLocked(b, u)
}

func (m *Manager) siteGrantedLocked(b *conv, u *url.URL) bool {
	s := &m.settings.current
	if s.DefaultPermission == PermissionAllow || b.freely || b.granted[Site(u)] {
		return true
	}
	if strings.ToLower(u.Hostname()) == "localhost" && slices.Contains(s.WorkspaceSites[b.workspace], localhostPreviews) {
		return true
	}
	for _, entry := range append(slices.Clone(s.AllowedSites), s.WorkspaceSites[b.workspace]...) {
		if SiteMatches(entry, u) {
			return true
		}
	}
	return false
}

// ask puts a request to the person and waits for the answer, the timeout,
// Stop, or ctx.
func (m *Manager) ask(ctx context.Context, b *conv, r Request) answer {
	id := make([]byte, 12)
	_, _ = rand.Read(id)
	r.ID = "browser-" + hex.EncodeToString(id)
	r.Created = m.cfg.Now()
	m.mu.Lock()
	r.Session, r.Conversation, r.Workspace, r.Permission, r.Agent = b.session, b.id, b.workspace, b.permission, b.agent
	if r.Kind == "site" {
		if u, err := url.Parse(r.URL); err == nil && m.siteGrantedLocked(b, u) {
			m.mu.Unlock()
			return answer{allow: true}
		}
	}
	count := 0
	for _, other := range m.requests {
		if other.conv == b.id {
			count++
		}
	}
	if count >= maxPending {
		m.mu.Unlock()
		return answer{reason: "too many browser requests are already waiting for the person"}
	}
	p := &pending{Request: r, conv: b.id, answer: make(chan answer, 1)}
	m.requests[r.ID] = p
	m.changedLocked(b)
	m.mu.Unlock()
	timer := time.NewTimer(m.cfg.AskTimeout)
	defer timer.Stop()
	var a answer
	select {
	case a = <-p.answer:
	case <-timer.C:
		a = answer{reason: fmt.Sprintf("the person did not answer within %s", m.cfg.AskTimeout.Round(time.Minute))}
	case <-ctx.Done():
		a = answer{reason: "the request was canceled"}
	}
	m.mu.Lock()
	delete(m.requests, r.ID)
	m.changedLocked(b)
	m.mu.Unlock()
	return a
}

// denyLocked answers every request of a conversation with a refusal.
func (m *Manager) denyLocked(conversation, reason string) {
	for id, r := range m.requests {
		if r.conv != conversation {
			continue
		}
		delete(m.requests, id)
		select {
		case r.answer <- answer{reason: reason}:
		default:
		}
	}
}

func (m *Manager) setFreelyLocked(b *conv, freely bool) error {
	previous := b.freely
	b.freely = freely
	if err := m.persistLocked(b); err != nil {
		b.freely = previous
		return err
	}
	return nil
}

// resolveSiteGrantsLocked wakes visits already waiting when the person expands a grant.
func (m *Manager) resolveSiteGrantsLocked() {
	for id, request := range m.requests {
		if request.Kind != "site" {
			continue
		}
		b := m.browsers[request.conv]
		u, err := url.Parse(request.URL)
		if b != nil && err == nil && m.siteGrantedLocked(b, u) {
			delete(m.requests, id)
			request.answer <- answer{allow: true}
			m.changedLocked(b)
		}
	}
}

const localhostPreviews = "localhost:*"

// Decide answers a request: a site once, for the conversation, always for the
// workspace, or deny; a download or a sign-in once or deny.
func (m *Manager) Decide(id, decision, reason string) (State, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	r := m.requests[id]
	if r == nil {
		return State{}, fmt.Errorf("%w: browser request %s was answered or expired", ErrNotFound, id)
	}
	b := m.browsers[r.conv]
	valid := []string{"once", "deny"}
	u, _ := url.Parse(r.URL)
	if r.Kind == "site" {
		if b != nil && b.workspace != "" && u != nil && strings.ToLower(u.Hostname()) == "localhost" {
			valid = append(valid, "workspace_previews")
		}
		valid = append(valid, "conversation", "workspace", "browse_freely")
	}
	if !slices.Contains(valid, decision) {
		return State{}, fmt.Errorf("decision must be one of %s for a %s request", strings.Join(valid, ", "), r.Kind)
	}
	if len([]rune(reason)) > 1000 {
		return State{}, errors.New("reason is too long")
	}
	if r.Kind == "site" && u != nil && b != nil {
		if decision == "workspace" && b.workspace == "" {
			decision = "conversation"
		}
		switch decision {
		case "browse_freely":
			if err := m.setFreelyLocked(b, true); err != nil {
				return State{}, err
			}
		case "once":
			b.once = r.Origin
		case "conversation":
			site := Site(u)
			if !b.granted[site] {
				b.granted[site] = true
				if err := m.persistLocked(b); err != nil {
					delete(b.granted, site)
					return State{}, err
				}
			}
		case "workspace", "workspace_previews":
			if _, err := m.settings.update(func(s *Settings) {
				if s.WorkspaceSites == nil {
					s.WorkspaceSites = map[string][]string{}
				}
				site := Site(u)
				if decision == "workspace_previews" {
					site = localhostPreviews
				}
				s.WorkspaceSites[b.workspace] = append(s.WorkspaceSites[b.workspace], site)
			}); err != nil {
				return State{}, err
			}
		}
	}
	delete(m.requests, id)
	r.answer <- answer{allow: decision != "deny", reason: strings.TrimSpace(reason)}
	if r.Kind == "site" && decision != "once" && decision != "deny" {
		m.resolveSiteGrantsLocked()
	}
	if b == nil {
		return State{Conversation: r.conv}, nil
	}
	if decision != "deny" || r.Kind != "site" {
		action := map[string]string{"site": "Allowed " + r.Origin, "download": "Allowed the download of " + r.FileName, "sign_in": "Allowed signing in on " + r.Origin}[r.Kind]
		if decision == "deny" {
			action = map[string]string{"download": "Declined the download of " + r.FileName, "sign_in": "Declined signing in on " + r.Origin}[r.Kind]
		}
		m.logLocked(b, Action{Kind: "site", Actor: "person", Summary: action, URL: r.URL, Status: "done"})
	} else {
		m.logLocked(b, Action{Kind: "site", Actor: "person", Summary: "Declined " + r.Origin, URL: r.URL, Status: "blocked"})
	}
	return m.stateLocked(b), nil
}

// ───────────────────────── settings ─────────────────────────

// Settings returns Settings → Browser.
func (m *Manager) Settings() Settings {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.settings.get()
}

// SaveSettings saves the person's settings against the revision they read.
func (m *Manager) SaveSettings(next Settings) (Settings, error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.settings.save(next)
}

// ProfileData reports each workspace profile's size on disk.
func (m *Manager) ProfileData() map[string]int64 {
	out := map[string]int64{}
	entries, err := os.ReadDir(filepath.Join(m.cfg.Root, "profiles"))
	if err != nil {
		return out
	}
	for _, e := range entries {
		if !e.IsDir() || strings.HasPrefix(e.Name(), "conversation-") {
			continue
		}
		var size int64
		_ = filepath.WalkDir(filepath.Join(m.cfg.Root, "profiles", e.Name()), func(_ string, d fs.DirEntry, err error) error {
			if err == nil && !d.IsDir() {
				if info, err := d.Info(); err == nil {
					size += info.Size()
				}
			}
			return nil
		})
		out[e.Name()] = size
	}
	return out
}

// ClearData closes a workspace's browsers and deletes its profile and its
// always-allowed sites.
func (m *Manager) ClearData(ctx context.Context, workspace string) (Settings, error) {
	if !safeName(workspace) {
		return Settings{}, errors.New("invalid workspace")
	}
	dir := filepath.Join(m.cfg.Root, "profiles", workspace)
	m.mu.Lock()
	p := m.proc
	for _, b := range m.browsers {
		if b.workspace == workspace && b.opened {
			b.opened, b.frame, b.status, b.problem = false, nil, "closed", "Browser data was cleared."
			m.changedLocked(b)
		}
	}
	m.mu.Unlock()
	if p != nil {
		call, cancel := context.WithTimeout(ctx, 20*time.Second)
		_, err := p.Call(call, "close_profile", map[string]string{"profile": dir})
		cancel()
		if err != nil {
			return m.Settings(), fmt.Errorf("close the workspace's browser: %w", err)
		}
	}
	if err := os.RemoveAll(dir); err != nil {
		return m.Settings(), err
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	if _, ok := m.settings.current.WorkspaceSites[workspace]; ok {
		return m.settings.update(func(s *Settings) { delete(s.WorkspaceSites, workspace) })
	}
	return m.settings.get(), nil
}
