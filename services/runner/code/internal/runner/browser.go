package runner

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/browser"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

// The agent browser (BROWSER.md). The Runner gives each coding conversation a
// browser it drives through its browser_* tools; the browser package owns the
// policy and the service, and this file connects it to executions and
// workspaces: who calls, which conversation that is, and which of the
// workspace's previews its browser may reach.

// openBrowser creates the browser manager. The service itself starts with the
// first browser an agent opens.
func (b *Bench) openBrowser() error {
	blocked := []string{}
	if u, err := url.Parse(b.Config.Address); err == nil && u.Host != "" {
		blocked = append(blocked, u.Host)
	} else if _, _, err := net.SplitHostPort(b.Config.Address); err == nil {
		blocked = append(blocked, b.Config.Address)
	}
	m, err := browser.New(browser.Config{Node: b.Config.Node, Script: b.Config.BrowserWorker, Root: filepath.Join(b.Config.State, "browser"), BlockedHosts: blocked, LoopbackPorts: b.previewPorts})
	if err != nil {
		return err
	}
	b.Browser = m
	return nil
}

// previewPorts are the ports a workspace's running actions serve on: the only
// addresses on this computer its agent browser may reach.
func (b *Bench) previewPorts(ctx context.Context, workspace string) []int {
	if workspace == "" || b.runtime == nil {
		return nil
	}
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	view, err := b.WorkspaceRuntime(ctx, workspace)
	if err != nil {
		return nil
	}
	ports := []int{}
	for _, a := range view.Actions {
		if a.Port > 0 {
			ports = append(ports, a.Port)
		}
	}
	return append(ports, b.commandPorts(workspace)...)
}

// commandPorts are the ports the workspace's running commands listen on: a
// dev server an agent started with `lazybox run`, or the person from a
// workspace terminal. Its agents' browsers may open them, as they may an
// action's preview, so an agent can check the app it runs without anyone
// defining an action.
func (b *Bench) commandPorts(workspace string) []int {
	if b.Commands == nil {
		return nil
	}
	// A conversation's command is its workspace's; only live conversations run commands.
	sessions := map[string]string{}
	b.mu.Lock()
	for id, e := range b.items {
		sessions[id] = e.CollectionID
	}
	b.mu.Unlock()
	ports := []int{}
	for _, c := range b.Commands.List("") {
		owner := c.Spec.CollectionID
		if c.Spec.SessionID != "" {
			owner = sessions[c.Spec.SessionID]
		}
		if c.Done || len(c.Ports) == 0 || owner != workspace {
			continue
		}
		for _, p := range c.Ports {
			ports = append(ports, int(p))
		}
	}
	return ports
}

// browserProvider reports whether provider's adapter offers the browser
// tools: Claude (an MCP server in its worker) and Codex (dynamic tools). ACP
// agents bring their own tools and get none of Lazybox's.
func browserProvider(provider string) bool { return provider == "claude" || provider == "codex" }

// browserFor reports whether a coding agent of provider opened now in
// workspace gets the browser tools: whenever this Runner has the service,
// unless the person turned it off (everywhere, or for that workspace).
func (b *Bench) browserFor(provider, purpose, workspace string) bool {
	return b.Browser != nil && !agent.IsAssistant(purpose) && browserProvider(provider) && b.Browser.EnabledFor(workspace)
}

// browserOffered: the execution's agent gets the browser tools when it opens.
func (b *Bench) browserOffered(e *Execution) bool {
	return b.browserFor(e.Profile.Provider, e.Purpose, e.CollectionID)
}

// Browser access of a conversation's agent (Session.browser_access).
const (
	// BrowserHas: its agent has the browser tools.
	BrowserHas = "has"
	// BrowserCanGive: it has not (it started before the browser was there,
	// or while it was off for its workspace), and Give <agent> the browser
	// gives it: turning it on for the workspace and restarting the agent,
	// the conversation kept.
	BrowserCanGive = "can_give"
	// BrowserOff: the person chose Off in Settings → Browser.
	BrowserOff = "off"
)

// BrowserAccess says whether e's agent has the browser, can be given it, or
// cannot because it is off everywhere; "" when the browser does not apply
// (no service, an assistant, an ACP agent, an ended conversation).
func (b *Bench) BrowserAccess(e *Execution) string {
	if b.Browser == nil || !b.Browser.Available() || agent.IsAssistant(e.Purpose) || !browserProvider(e.Profile.Provider) || e.Archived || e.Engine == nil {
		return ""
	}
	if e.Engine.Snapshot().Closed {
		return ""
	}
	switch {
	case !b.Browser.Enabled():
		return BrowserOff
	case e.browserTools && !b.Browser.WorkspaceOff(e.CollectionID):
		return BrowserHas
	}
	return BrowserCanGive
}

// GiveBrowser gives e's agent the browser: it turns the browser back on for
// e's workspace if the person turned it off there, and restarts an agent
// that started without the tools, between turns, continuing the same
// provider conversation (as a model change does). It returns the execution
// the conversation continues in: e itself when no restart was needed.
func (b *Bench) GiveBrowser(ctx context.Context, e *Execution) (*Execution, error) {
	switch {
	case b.Browser == nil || !b.Browser.Available():
		return nil, browser.ErrUnavailable
	case agent.IsAssistant(e.Purpose):
		return nil, errors.New("assistants do not get the agent browser")
	case !browserProvider(e.Profile.Provider):
		return nil, errors.New("this agent brings its own tools: Lazybox's browser is for Claude and Codex conversations")
	case !b.Browser.Enabled():
		return nil, errors.New("the agent browser is off in Settings → Browser: choose Ask before each new site there first")
	}
	if !e.browserTools {
		if err := restartable(e); err != nil {
			return nil, err
		}
		if state := e.Engine.Snapshot(); state.Reference == "" || !state.Capabilities.Resume {
			return nil, errors.New("this conversation cannot be resumed by its provider; start a new conversation: it gets the browser")
		}
	}
	if b.Browser.WorkspaceOff(e.CollectionID) {
		if _, err := b.Browser.SetWorkspaceOff(e.CollectionID, false); err != nil {
			return nil, err
		}
	}
	if e.browserTools {
		return e, nil
	}
	next, err := b.restart(ctx, e)
	if err != nil {
		return nil, err
	}
	_ = b.Record(next, agent.Event{Type: "browser_given", Status: "restart"})
	return next, nil
}

// installTitle names the Chromium install in its console.
const installTitle = "Install Chromium for the agent browser"

// InstallBrowser installs the Chromium the agent browser runs, in a
// developer build where it is missing: the command `make browser` runs, as a
// Runner command the person watches in Settings → Browser. One at a time:
// while one runs, it is returned again.
func (b *Bench) InstallBrowser() (commands.Status, error) {
	if b.Browser == nil || !b.Browser.Available() {
		return commands.Status{}, browser.ErrUnavailable
	}
	if id := b.Browser.InstallID(); id != "" {
		if c, err := b.Commands.Get(id); err == nil && !c.Status().Done {
			return c.Status(), nil
		}
	}
	a := b.Browser.Availability()
	switch {
	case a.State == browser.AvailabilityReady:
		return commands.Status{}, errors.New("Chromium for the agent browser is already installed")
	case a.State != browser.AvailabilityNotInstalled:
		return commands.Status{}, errors.New(a.Problem)
	case len(a.Install) == 0:
		return commands.Status{}, errors.New("this Lazybox cannot install Chromium itself: run make browser in its source checkout")
	}
	c, err := b.Commands.Start(commands.Spec{Kind: commands.KindRunner, Title: installTitle, Cwd: filepath.Dir(a.Install[1]), Argv: a.Install, Env: b.Browser.InstallEnv()})
	if err != nil {
		return commands.Status{}, err
	}
	b.Browser.SetInstallID(c.Status().ID)
	return c.Status(), nil
}

// browserToolsOf is what an agent re-attached after a Runner restart was
// started with. A record from before Lazybox kept it says nothing; then the
// agent's own tool list, the last one it reported (Claude's tool_catalog),
// tells, and failing that the agent is taken to have what it would get now.
func (b *Bench) browserToolsOf(r sessionstore.Record, now bool) bool {
	if r.BrowserTools != nil {
		return *r.BrowserTools
	}
	if has, known := reportedBrowserTools(b.journalPath(r.ID)); known {
		return has
	}
	return now
}

// reportedBrowserTools reads the last tool list an agent reported in its
// journal (and its rolled-over segments): known when there is one, has when
// it lists the browser's tools.
func reportedBrowserTools(path string) (has, known bool) {
	files, _ := filepath.Glob(path + "*")
	var newest time.Time
	for _, file := range files {
		info, err := os.Stat(file)
		if err != nil || !info.Mode().IsRegular() || (known && !info.ModTime().After(newest)) {
			continue
		}
		data, err := os.ReadFile(file)
		if err != nil {
			continue
		}
		at := bytes.LastIndex(data, []byte(`"type":"tool_catalog"`))
		if at < 0 {
			continue
		}
		line := data[at:]
		if end := bytes.IndexByte(line, '\n'); end >= 0 {
			line = line[:end]
		}
		has, known, newest = bytes.Contains(line, []byte(`"`+browserMCPPrefix)), true, info.ModTime()
	}
	return has, known
}

// browserMCPPrefix starts the browser tools' names as Claude lists them
// (workers/claude-sdk/src/browser.ts names the server lazybox_browser).
const browserMCPPrefix = "mcp__lazybox_browser__"

// BrowserCaller is the execution as the browser knows it: its conversation,
// workspace, permission and agent.
func (b *Bench) BrowserCaller(e *Execution) browser.Caller {
	name := e.Profile.Provider
	if name != "" {
		name = strings.ToUpper(name[:1]) + name[1:]
	}
	return browser.Caller{Session: e.ID, Conversation: conversationOf(e), Workspace: e.CollectionID, Permission: e.Options.Permission, Agent: name}
}

// browserTool runs one browser tool for e, on e's own conversation's browser.
func (b *Bench) browserTool(ctx context.Context, e *Execution, name string, args json.RawMessage) (string, error) {
	if b.Browser == nil || agent.IsAssistant(e.Purpose) {
		return "", agent.ErrUnsupported
	}
	caller := b.BrowserCaller(e)
	if e.Options.Workflow == "plan" {
		caller.Permission = "read-only"
	}
	return b.Browser.Tool(ctx, caller, name, args)
}
