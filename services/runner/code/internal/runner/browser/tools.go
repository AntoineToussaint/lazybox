package browser

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"slices"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/adapters/jsonl"
)

// The agent's tools and the person's controls. An agent's call runs only on
// its own conversation's browser (the Runner derives the conversation from
// the calling execution, never from the arguments), one call at a time, and
// waits while the person holds the browser.

type toolArgs struct {
	URL            string   `json:"url"`
	Reason         string   `json:"reason"`
	Ref            string   `json:"ref"`
	Element        string   `json:"element"`
	Text           string   `json:"text"`
	Submit         bool     `json:"submit"`
	Values         []string `json:"values"`
	Key            string   `json:"key"`
	Direction      string   `json:"direction"`
	Amount         float64  `json:"amount"`
	FullPage       bool     `json:"fullPage"`
	TextGone       string   `json:"textGone"`
	TimeoutSeconds float64  `json:"timeoutSeconds"`
	MaxChars       int      `json:"maxChars"`
}

// actResult is the service's answer to an act.
type actResult struct {
	Text    string              `json:"text"`
	Summary string              `json:"summary"`
	State   PageState           `json:"state"`
	Box     *Box                `json:"box"`
	Image   *agent.BrowserImage `json:"image"`
}

// acting are the tools that change a page rather than read it.
var acting = map[string]bool{"click": true, "type": true, "select": true, "press": true}

var errStopped = errors.New("the person stopped your browser. Do not use it again unless they ask you to")

// Tool runs one of the agent's browser tools on its conversation's browser.
// The result is an agent.BrowserResult as JSON.
func (m *Manager) Tool(ctx context.Context, c Caller, name string, raw json.RawMessage) (string, error) {
	if !m.Available() {
		return "", fmt.Errorf("%w on this Runner", ErrUnavailable)
	}
	action, ok := strings.CutPrefix(name, agent.BrowserToolPrefix)
	if !ok || !agent.BrowserTool(name) {
		return "", agent.ErrUnsupported
	}
	if c.Conversation == "" {
		return "", errors.New("no conversation")
	}
	var a toolArgs
	if len(raw) > 0 && string(raw) != "null" {
		if err := json.Unmarshal(raw, &a); err != nil {
			return "", fmt.Errorf("invalid arguments: %w", err)
		}
	}
	m.mu.Lock()
	off := m.settings.current.DefaultPermission == PermissionOff
	workspaceOff := c.Workspace != "" && slices.Contains(m.settings.current.OffWorkspaces, c.Workspace)
	b := m.browserLocked(c)
	stopped := b.stopped
	if !off && !workspaceOff && !stopped {
		// From the agent's first call the person sees its browser, even
		// while the first site still waits for them.
		b.used = true
	}
	m.mu.Unlock()
	switch {
	case off:
		return "", errors.New("the person turned the agent browser off in Settings → Browser")
	case workspaceOff:
		return "", fmt.Errorf("the person turned the agent browser off for this workspace. If you need it, ask them to choose “Give %s the browser” in this conversation's menu, or to turn it on in Settings → Browser", agentName(c.Agent))
	case stopped:
		return "", errStopped
	case acting[action] && c.Permission == "read-only":
		return "", errors.New("this conversation is read-only: you can open, read, scroll and screenshot pages, but not click, type or press keys")
	}
	b.busy.Lock()
	defer b.busy.Unlock()
	held, err := m.awaitAgent(ctx, b)
	if err != nil {
		return "", err
	}
	text, image, err := m.run(ctx, b, action, a)
	if err != nil {
		if strings.Contains(err.Error(), "is not installed") {
			// The person installs it with one click; an agent running an
			// install in its workspace would only fail or install it elsewhere.
			return "", errors.New("Chromium for the agent browser is not installed on this computer. Do not install it yourself: tell the person they can install it with Install Chromium in Settings → Browser, then use the browser again (no restart needed)")
		}
		return "", err
	}
	m.mu.Lock()
	notes := b.notes
	b.notes = nil
	m.mu.Unlock()
	var lead []string
	if held {
		lead = append(lead, "The person used the browser and gave it back. Take a fresh browser_snapshot before using refs.")
	}
	if len(notes) > 0 {
		lead = append(lead, "Also: "+strings.Join(notes, "; ")+".")
	}
	if len(lead) > 0 {
		text = strings.Join(lead, "\n") + "\n\n" + text
	}
	out, err := json.Marshal(agent.BrowserResult{Text: text, Image: image})
	return string(out), err
}

// agentName is who browses, for a sentence: "Claude", else "the agent".
func agentName(name string) string {
	if name == "" {
		return "the agent"
	}
	return name
}

// awaitAgent returns once the agent holds the browser: at once, or when the
// person gives it back. held says the person had it meanwhile.
func (m *Manager) awaitAgent(ctx context.Context, b *conv) (bool, error) {
	timer := time.NewTimer(m.cfg.HoldTimeout)
	defer timer.Stop()
	for {
		m.mu.Lock()
		if b.stopped {
			m.mu.Unlock()
			return false, errStopped
		}
		if b.controller != "person" {
			held := b.heldByPerson
			b.heldByPerson = false
			m.mu.Unlock()
			return held, nil
		}
		changed := b.changed
		m.mu.Unlock()
		select {
		case <-changed:
		case <-ctx.Done():
			return false, ctx.Err()
		case <-timer.C:
			return false, errors.New("the person still has the browser (they took it over). Wait for them to give it back, or ask them in the chat")
		}
	}
}

// run performs one tool call and logs it.
func (m *Manager) run(ctx context.Context, b *conv, action string, a toolArgs) (string, *agent.BrowserImage, error) {
	if action == "close" {
		return m.closeBrowser(ctx, b, "agent"), nil, nil
	}
	params := map[string]any{"browser": b.id, "actor": "agent", "action": action}
	target := a.Element
	if target == "" && a.Ref != "" {
		target = "element " + a.Ref
	}
	if a.MaxChars > 0 {
		params["maxChars"] = min(max(a.MaxChars, 500), 40000)
	}
	running := ""
	switch action {
	case "open", "navigate":
		if action == "navigate" && strings.TrimSpace(a.URL) == "" {
			return "", nil, errors.New("browser_navigate needs a url")
		}
		target := a.URL
		if target == "" {
			if err := m.ensureOpen(ctx, b); err != nil {
				return "", nil, err
			}
			m.mu.Lock()
			current, last := b.page.URL, b.lastURL
			m.mu.Unlock()
			if current != "" && current != "about:blank" {
				params["action"], running = "snapshot", "Reading the page"
				break
			}
			if last == "" {
				return "Your browser is open on a blank page. Go somewhere with browser_navigate.", nil, nil
			}
			// Reopened after it closed underneath the agent: back where it was.
			target = last
			if u, err := url.Parse(last); err == nil {
				m.mu.Lock()
				b.once = Origin(u)
				m.mu.Unlock()
			}
		}
		u, err := m.permitted(ctx, b, target, a.Reason)
		if err != nil {
			return "", nil, err
		}
		if err := m.ensureOpen(ctx, b); err != nil {
			return "", nil, err
		}
		params["action"], params["url"], params["withSnapshot"] = "navigate", u.String(), true
		running = "Opening " + Site(u)
	case "snapshot":
		running = "Reading the page"
	case "click":
		params["ref"], params["withSnapshot"] = a.Ref, true
		running = "Clicking " + target
	case "type":
		params["ref"], params["text"], params["submit"], params["withSnapshot"] = a.Ref, a.Text, a.Submit, true
		running = "Typing into " + target
	case "select":
		params["ref"], params["values"], params["withSnapshot"] = a.Ref, a.Values, true
		running = "Choosing in " + target
	case "press":
		params["key"], params["withSnapshot"] = a.Key, true
		running = "Pressing " + a.Key
	case "scroll":
		if a.Direction == "" && a.Ref == "" {
			a.Direction = "down"
		}
		params["direction"], params["ref"] = a.Direction, a.Ref
		if a.Amount > 0 {
			params["amount"] = a.Amount
		}
		running = "Scrolling " + a.Direction
	case "screenshot":
		params["fullPage"] = a.FullPage
		running = "Taking a screenshot"
	case "back", "forward":
		params["withSnapshot"] = true
		running = map[string]string{"back": "Going back", "forward": "Going forward"}[action]
	case "wait_for":
		params["text"], params["textGone"], params["ref"] = a.Text, a.TextGone, a.Ref
		if a.TimeoutSeconds > 0 {
			params["timeoutMs"] = int(min(a.TimeoutSeconds, 60) * 1000)
		}
		running = "Waiting for the page"
	default:
		return "", nil, agent.ErrUnsupported
	}
	if err := m.ensureOpen(ctx, b); err != nil {
		return "", nil, err
	}
	kind := params["action"].(string)
	if kind == "wait_for" {
		kind = "wait"
	}
	m.mu.Lock()
	entry := m.logLocked(b, Action{Kind: kind, Actor: "agent", Summary: running, URL: b.page.URL, Status: "running"})
	m.mu.Unlock()
	r, err := m.act(ctx, b, params)
	m.mu.Lock()
	entry.Duration = m.cfg.Now().Sub(entry.At)
	if err != nil {
		entry.Status, entry.Error = "failed", err.Error()
	} else {
		entry.Status, entry.Box, entry.URL = "done", r.Box, r.State.URL
		if r.Summary != "" {
			entry.Summary = r.Summary
		}
	}
	m.logLocked(b, entry)
	m.mu.Unlock()
	if err != nil {
		return "", nil, err
	}
	return r.Text, r.Image, nil
}

// permitted checks a URL the agent asked for and, for a site not allowed
// yet, asks the person.
func (m *Manager) permitted(ctx context.Context, b *conv, raw, reason string) (*url.URL, error) {
	m.mu.Lock()
	workspace := b.workspace
	m.mu.Unlock()
	u, err := ParseURL(raw, m.cfg.BlockedHosts, m.cfg.LoopbackPorts(ctx, workspace))
	if err != nil {
		if errors.Is(err, ErrBlocked) {
			m.mu.Lock()
			m.logLocked(b, Action{Kind: "blocked", Actor: "lazybox", Summary: "Refused " + raw, URL: raw, Status: "blocked", Error: err.Error()})
			m.mu.Unlock()
		}
		return nil, err
	}
	m.mu.Lock()
	allowed := m.allowedLocked(b, u)
	m.mu.Unlock()
	if allowed {
		return u, nil
	}
	reason = strings.TrimSpace(reason)
	if len([]rune(reason)) > 500 {
		reason = string([]rune(reason)[:500])
	}
	answer := m.ask(ctx, b, Request{Kind: "site", URL: u.String(), Origin: Origin(u), Reason: reason})
	if !answer.allow {
		msg := "the person did not allow " + Site(u)
		if answer.reason != "" {
			msg += ": " + answer.reason
		}
		return nil, errors.New(msg)
	}
	return u, nil
}

// ensureOpen opens b's page in the service, starting the service.
func (m *Manager) ensureOpen(ctx context.Context, b *conv) error {
	m.mu.Lock()
	if b.opened {
		m.mu.Unlock()
		return nil
	}
	b.status, b.problem, b.used = "starting", "", true
	m.changedLocked(b)
	workspace := b.workspace
	m.mu.Unlock()
	fail := func(err error) error {
		if service := serviceError(err); service != err {
			err = service
		} else {
			// The process itself failed: one line for the person, the rest is in its stderr tail.
			first, _, _ := strings.Cut(err.Error(), "\n")
			err = fmt.Errorf("the browser service did not start: %s", first)
		}
		m.mu.Lock()
		b.status, b.problem = "failed", err.Error()
		m.changedLocked(b)
		m.mu.Unlock()
		return err
	}
	p, err := m.process()
	if err != nil {
		return fail(err)
	}
	profile, err := m.profileDir(b)
	if err != nil {
		return fail(err)
	}
	ports := m.cfg.LoopbackPorts(ctx, workspace)
	if ports == nil {
		ports = []int{}
	}
	call, cancel := context.WithTimeout(ctx, openTimeout)
	defer cancel()
	raw, err := p.Call(call, "open", map[string]any{"browser": b.id, "profile": profile, "viewport": map[string]int{"width": viewportW, "height": viewportH},
		"policy": map[string]any{"loopbackPorts": ports, "blockedHosts": append([]string{}, m.cfg.BlockedHosts...)}})
	if err != nil {
		return fail(err)
	}
	var page PageState
	_ = json.Unmarshal(raw, &page)
	m.mu.Lock()
	b.opened, b.status, b.problem = true, "open", ""
	m.pageLocked(b, page)
	watched := len(b.watchers) > 0
	m.changedLocked(b)
	m.mu.Unlock()
	if watched {
		m.screencast(b.id, true)
	}
	return nil
}

// act sends one action to the service and records the page it left.
func (m *Manager) act(ctx context.Context, b *conv, params map[string]any) (actResult, error) {
	m.mu.Lock()
	p := m.proc
	m.mu.Unlock()
	if p == nil {
		return actResult{}, errors.New("the browser closed; open it again with browser_open")
	}
	call, cancel := context.WithTimeout(ctx, actTimeout)
	defer cancel()
	raw, err := p.Call(call, "act", params)
	if err != nil {
		return actResult{}, serviceError(err)
	}
	var r actResult
	if err = json.Unmarshal(raw, &r); err != nil {
		return actResult{}, err
	}
	m.mu.Lock()
	m.pageLocked(b, r.State)
	m.changedLocked(b)
	m.mu.Unlock()
	return r, nil
}

// serviceError keeps the service's own words: a call it refused reads as its message.
func serviceError(err error) error {
	var call *jsonl.CallError
	if errors.As(err, &call) && call.Message != "" {
		return errors.New(call.Message)
	}
	return err
}

// closeBrowser closes b's page; who is agent or person.
func (m *Manager) closeBrowser(ctx context.Context, b *conv, who string) string {
	m.mu.Lock()
	opened := b.opened
	p := m.proc
	b.opened, b.frame, b.status, b.problem = false, nil, "closed", ""
	m.persistLocked(b)
	m.logLocked(b, Action{Kind: "close", Actor: who, Summary: "Closed the browser", Status: "done"})
	m.mu.Unlock()
	if opened && p != nil {
		call, cancel := context.WithTimeout(ctx, 10*time.Second)
		defer cancel()
		_, _ = p.Call(call, "close", map[string]string{"browser": b.id})
	}
	return "Closed the browser."
}

// signalLocked wakes the agent calls waiting on b's controller or Stop.
func signalLocked(b *conv) {
	close(b.changed)
	b.changed = make(chan struct{})
}

// Control is the person's: take_over, give_back, stop, resume, browse_freely,
// ask_sites; and while they hold it, navigate, back, forward, reload.
func (m *Manager) Control(ctx context.Context, c Caller, action, raw string) (State, error) {
	m.mu.Lock()
	b := m.browserLocked(Caller{Conversation: c.Conversation, Session: c.Session, Workspace: c.Workspace, Agent: c.Agent, Permission: c.Permission})
	agentName := b.agent
	if agentName == "" {
		agentName = "the agent"
	}
	switch action {
	case "take_over":
		if !b.opened {
			m.mu.Unlock()
			return State{}, errors.New("the browser is not open")
		}
		if b.controller != "person" {
			b.controller, b.heldByPerson = "person", true
			signalLocked(b)
			m.logLocked(b, Action{Kind: "take_over", Actor: "person", Summary: "You took over; " + agentName + " waits", Status: "done", URL: b.page.URL})
		}
	case "give_back":
		if b.controller == "person" {
			b.controller = "agent"
			signalLocked(b)
			m.logLocked(b, Action{Kind: "give_back", Actor: "person", Summary: "You gave the browser back to " + agentName, Status: "done", URL: b.page.URL})
		}
	case "stop":
		opened := b.opened
		b.stopped, b.controller, b.opened, b.frame = true, "agent", false, nil
		b.status, b.problem = "stopped", "You stopped this browser."
		m.denyLocked(b.id, "the person stopped the browser")
		signalLocked(b)
		m.persistLocked(b)
		m.logLocked(b, Action{Kind: "stop", Actor: "person", Summary: "You stopped the browser", Status: "done"})
		p := m.proc
		state := m.stateLocked(b)
		m.mu.Unlock()
		if opened && p != nil {
			call, cancel := context.WithTimeout(ctx, 10*time.Second)
			defer cancel()
			_, _ = p.Call(call, "close", map[string]string{"browser": b.id})
		}
		return state, nil
	case "resume":
		if b.stopped {
			b.stopped, b.status, b.problem = false, "closed", ""
			signalLocked(b)
			m.persistLocked(b)
			m.logLocked(b, Action{Kind: "give_back", Actor: "person", Summary: "You let " + agentName + " browse again", Status: "done"})
		}
	case "browse_freely", "ask_sites":
		freely := action == "browse_freely"
		if b.freely != freely {
			b.freely = freely
			m.persistLocked(b)
			summary := agentName + " opens any site in this conversation"
			if !freely {
				summary = agentName + " asks before each new site again"
			}
			m.logLocked(b, Action{Kind: "site", Actor: "person", Summary: summary, Status: "done"})
		}
	case "navigate", "back", "forward", "reload":
		if b.controller != "person" || !b.opened {
			m.mu.Unlock()
			return State{}, errors.New("take over the browser first")
		}
		workspace := b.workspace
		m.mu.Unlock()
		params := map[string]any{"browser": b.id, "actor": "person", "action": action}
		summary := map[string]string{"back": "You went back", "forward": "You went forward", "reload": "You reloaded the page"}[action]
		if action == "navigate" {
			u, err := ParseURL(raw, m.cfg.BlockedHosts, m.cfg.LoopbackPorts(ctx, workspace))
			if err != nil {
				return State{}, err
			}
			params["url"], summary = u.String(), "You went to "+Site(u)
		}
		r, err := m.act(ctx, b, params)
		if err != nil {
			return State{}, err
		}
		m.mu.Lock()
		kind := action
		if action == "reload" {
			kind = "navigate"
		}
		m.logLocked(b, Action{Kind: kind, Actor: "person", Summary: summary, URL: r.State.URL, Status: "done"})
		defer m.mu.Unlock()
		return m.stateLocked(b), nil
	default:
		m.mu.Unlock()
		return State{}, fmt.Errorf("unknown browser control %q", action)
	}
	defer m.mu.Unlock()
	m.changedLocked(b)
	return m.stateLocked(b), nil
}

// Input forwards the person's mouse and keyboard while they hold the browser.
func (m *Manager) Input(ctx context.Context, conversation string, input map[string]any) (State, error) {
	m.mu.Lock()
	b := m.browsers[conversation]
	if b == nil || !b.opened || b.controller != "person" {
		m.mu.Unlock()
		return State{}, errors.New("take over the browser first")
	}
	p := m.proc
	m.mu.Unlock()
	if p == nil {
		return State{}, errors.New("the browser closed")
	}
	input["browser"] = conversation
	call, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	raw, err := p.Call(call, "input", input)
	if err != nil {
		return State{}, serviceError(err)
	}
	var page PageState
	m.mu.Lock()
	defer m.mu.Unlock()
	if json.Unmarshal(raw, &page) == nil && page.URL != "" {
		m.pageLocked(b, page)
	}
	return m.stateLocked(b), nil
}
