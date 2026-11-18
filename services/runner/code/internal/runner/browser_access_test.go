package runner

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/browser"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/sessionstore"
)

func recordWith(tools *bool) sessionstore.Record {
	return sessionstore.Record{ID: "NOJOURNAL", BrowserTools: tools}
}

// browserBench is a Runner with the agent browser service configured. The
// service itself starts only when an agent opens its browser, so a path is
// enough here.
func browserBench(t *testing.T) (*Bench, *agenttest.Adapter) {
	t.Helper()
	worker := filepath.Join(t.TempDir(), "main.js")
	if err := os.WriteFile(worker, []byte("// the browser service\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	b, err := New(Config{State: t.TempDir(), BrowserWorker: worker})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { b.Close() })
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	return b, fake
}

func setBrowserPermission(t *testing.T, b *Bench, permission string) {
	t.Helper()
	s := b.Browser.Settings()
	s.DefaultPermission = permission
	if _, err := b.Browser.SaveSettings(s); err != nil {
		t.Fatal(err)
	}
}

func TestNewConversationGetsTheBrowserWhenTheRunnerHasIt(t *testing.T) {
	b, fake := browserBench(t)
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if !fake.Session.Open.Browser {
		t.Fatal("a coding agent opened while the browser is available must get its tools")
	}
	if got := b.BrowserAccess(e); got != BrowserHas {
		t.Fatalf("access %q, want %q", got, BrowserHas)
	}
	saved, err := b.History.Get(context.Background(), e.ID)
	if err != nil || saved.BrowserTools == nil || !*saved.BrowserTools {
		t.Fatalf("what the agent was started with must be kept for a re-attach: %v %v", err, saved.BrowserTools)
	}
	// The briefing tells it, and how the person sees it.
	if !strings.Contains(saved.Context, "You have a browser") || !strings.Contains(saved.Context, "Agent browser panel") {
		t.Fatalf("briefing does not say the agent has the browser:\n%s", saved.Context)
	}
	if update := b.BriefingUpdate(e).Text; update != "" {
		t.Fatalf("a new conversation already has its browser section: %q", update)
	}
}

func TestNoBrowserServiceMeansNoToolsAndNoBriefing(t *testing.T) {
	b, err := New(Config{State: t.TempDir()})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if fake.Session.Open.Browser || b.BrowserAccess(e) != "" || strings.Contains(e.Saved.Context, "## Your browser") {
		t.Fatalf("a Runner without the service offers no browser: open=%v access=%q", fake.Session.Open.Browser, b.BrowserAccess(e))
	}
	if _, err = b.GiveBrowser(context.Background(), e); err == nil {
		t.Fatal("giving a browser this Runner does not have must fail")
	}
}

func TestGiveTheBrowserRestartsAnAgentStartedWithoutItAndKeepsTheConversation(t *testing.T) {
	b, fake := browserBench(t)
	setBrowserPermission(t, b, browser.PermissionOff)
	e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if fake.Session.Open.Browser {
		t.Fatal("the browser is off: the agent must not get its tools")
	}
	if got := b.BrowserAccess(e); got != BrowserOff {
		t.Fatalf("access %q, want %q", got, BrowserOff)
	}
	if !strings.Contains(e.Saved.Context, "agent browser is off (Settings → Browser)") {
		t.Fatalf("briefing must say the browser is off and where to turn it on:\n%s", e.Saved.Context)
	}
	if _, err = b.GiveBrowser(context.Background(), e); err == nil || !strings.Contains(err.Error(), "off in Settings → Browser") {
		t.Fatalf("off everywhere is the person's choice in Settings: %v", err)
	}
	// Turned on: this agent still has no tools; the person can give it them.
	setBrowserPermission(t, b, browser.PermissionAsk)
	if got := b.BrowserAccess(e); got != BrowserCanGive {
		t.Fatalf("access %q, want %q", got, BrowserCanGive)
	}
	update := b.BriefingUpdate(e).Text
	if !strings.Contains(update, "You do not have Lazybox's agent browser") || !strings.Contains(update, "Give Codex the browser") {
		t.Fatalf("the agent must hear how the person gives it the browser: %q", update)
	}
	// Mid-turn it waits.
	fake.Session.Events <- agent.Event{Type: "turn_started", TurnID: "t"}
	waitHistory(t, func() bool { return e.Engine.Snapshot().Busy })
	if _, err = b.GiveBrowser(context.Background(), e); err == nil || !strings.Contains(err.Error(), "finish its turn") {
		t.Fatalf("a busy agent must not be restarted: %v", err)
	}
	fake.Session.Events <- agent.Event{Type: "turn_ended", TurnID: "t", Status: "completed", SessionID: "native-reference"}
	waitHistory(t, func() bool { return !e.Engine.Snapshot().Busy })
	next, err := b.GiveBrowser(context.Background(), e)
	if err != nil {
		t.Fatal(err)
	}
	if next.ID == e.ID || next.Previous != e.ID {
		t.Fatalf("the conversation must continue in a successor: %q after %q", next.ID, e.ID)
	}
	if got := fake.Session.Open; !got.Browser || got.Resume != "native-reference" {
		t.Fatalf("the new agent must have the browser, on the same provider conversation: %+v", got)
	}
	if got := b.BrowserAccess(next); got != BrowserHas {
		t.Fatalf("access %q, want %q", got, BrowserHas)
	}
	if update := b.BriefingUpdate(next).Text; !strings.Contains(update, "You have a browser") {
		t.Fatalf("the restarted agent must hear it has the browser now: %q", update)
	}
	journaled(t, next, `"browser_given"`)
	// Given again: nothing to restart.
	if again, err := b.GiveBrowser(context.Background(), next); err != nil || again != next {
		t.Fatalf("an agent that has the browser is not restarted: %v", err)
	}
}

func TestBrowserOffForAWorkspaceWithholdsItThereOnly(t *testing.T) {
	b, _ := browserBench(t)
	if _, err := b.Browser.SetWorkspaceOff("ws-a", true); err != nil {
		t.Fatal(err)
	}
	if b.browserFor("claude", "coding", "ws-a") || !b.browserFor("claude", "coding", "ws-b") {
		t.Fatal("off for one workspace must withhold the browser there and only there")
	}
	for _, c := range []struct{ provider, purpose string }{{"claude", "help"}, {"gemini-acp", "coding"}, {"", "coding"}} {
		if b.browserFor(c.provider, c.purpose, "ws-b") {
			t.Fatalf("%s/%s must not get Lazybox's browser", c.provider, c.purpose)
		}
	}
	if got := b.browserBriefing("claude", "coding", "ws-a", false); !strings.Contains(got, "turned it off for this workspace") || !strings.Contains(got, "Give Claude the browser") {
		t.Fatalf("briefing must say why and how: %q", got)
	}
	if got := b.browserBriefing("claude", "coding", "ws-a", true); strings.Contains(got, "You have a browser") {
		t.Fatalf("an agent whose workspace has the browser off must not be told it has one: %q", got)
	}
	if got := b.browserBriefing("gemini-acp", "coding", "ws-b", false); got != "" {
		t.Fatalf("an ACP agent gets no browser section: %q", got)
	}
	// A conversation of that workspace whose agent has the tools: Give the
	// browser turns it back on there, without restarting the agent.
	b.Adapters["claude"] = &agenttest.Adapter{}
	e, err := b.OpenEmptyWorkspace("claude-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	e.CollectionID = "ws-a" // the empty-workspace conversation has none of its own
	if got := b.BrowserAccess(e); got != BrowserCanGive {
		t.Fatalf("access %q, want %q", got, BrowserCanGive)
	}
	if next, err := b.GiveBrowser(context.Background(), e); err != nil || next != e {
		t.Fatalf("no restart for an agent that has the tools: %v", err)
	}
	if b.Browser.WorkspaceOff("ws-a") || b.BrowserAccess(e) != BrowserHas {
		t.Fatal("giving the browser must turn it back on for the workspace")
	}
}

func TestInstallChromiumRunsAsARunnerCommandTheSettingsShow(t *testing.T) {
	root, cache := t.TempDir(), t.TempDir()
	t.Setenv("PLAYWRIGHT_BROWSERS_PATH", cache)
	script := filepath.Join(root, "workers", "browser", "dist", "main.js")
	core := filepath.Join(root, "node_modules", "playwright-core")
	for path, text := range map[string]string{script: "//\n", filepath.Join(core, "cli.js"): "//\n", filepath.Join(core, "browsers.json"): `{"browsers":[{"name":"chromium-headless-shell","revision":"7"}]}`} {
		if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	// /bin/echo stands in for Node: the command prints what it would run.
	b, err := New(Config{State: t.TempDir(), BrowserWorker: script, Node: "/bin/echo"})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	if got := b.Browser.Availability().State; got != browser.AvailabilityNotInstalled {
		t.Fatalf("availability %q", got)
	}
	started, err := b.InstallBrowser()
	if err != nil {
		t.Fatal(err)
	}
	if started.Spec.Kind != commands.KindRunner || started.Spec.SessionID != "" || started.Spec.CollectionID != "" || b.Browser.InstallID() != started.ID {
		t.Fatalf("a Runner command of no conversation, remembered for Settings: %+v", started.Spec)
	}
	c, err := b.Commands.Get(started.ID)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if done, err := c.Wait(ctx); err != nil || done.ExitCode == nil || *done.ExitCode != 0 {
		t.Fatalf("install: %+v %v", done, err)
	}
	out, _, err := c.Read(ctx, 0, 1<<16)
	if err != nil || !strings.Contains(string(out), "install --only-shell --no-remove chromium") {
		t.Fatalf("output %q %v", out, err)
	}
	if err := os.MkdirAll(filepath.Join(cache, "chromium_headless_shell-7"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(cache, "chromium_headless_shell-7", "INSTALLATION_COMPLETE"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err = b.InstallBrowser(); err == nil || !strings.Contains(err.Error(), "already installed") {
		t.Fatalf("nothing left to install: %v", err)
	}
}

func TestReattachedAgentKeepsTheToolsItReported(t *testing.T) {
	dir := t.TempDir()
	journal := filepath.Join(dir, "X.jsonl")
	write := func(name, text string) {
		t.Helper()
		if err := os.WriteFile(filepath.Join(dir, name), []byte(text), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if _, known := reportedBrowserTools(journal); known {
		t.Fatal("no journal: unknown")
	}
	write("X.jsonl", `{"value":{"type":"tool_catalog","data":{"tools":["Bash","mcp__lazybox__command"]}}}`+"\n"+`{"value":{"type":"turn_started"}}`+"\n")
	if has, known := reportedBrowserTools(journal); !known || has {
		t.Fatalf("a tool list without the browser: has=%v known=%v", has, known)
	}
	write("X.jsonl", `{"value":{"type":"tool_catalog","data":{"tools":["Bash"]}}}`+"\n"+`{"value":{"type":"tool_catalog","data":{"tools":["Bash","mcp__lazybox_browser__browser_open"]}}}`+"\n")
	if has, known := reportedBrowserTools(journal); !known || !has {
		t.Fatalf("the last tool list has the browser: has=%v known=%v", has, known)
	}
	b, _ := browserBench(t)
	yes, no := true, false
	if !b.browserToolsOf(recordWith(&yes), false) || b.browserToolsOf(recordWith(&no), true) {
		t.Fatal("a recorded answer wins over what the agent would get now")
	}
}
