package browser

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// The fake browser service: this test binary re-executed. It answers the
// protocol the way the real service does, without Chromium, so the Runner's
// policy can be tested deterministically. The real service has its own tests
// (workers/browser) and an end-to-end test (service_test.go).
func TestMain(m *testing.M) {
	if os.Getenv("LAZYBOX_FAKE_BROWSER") == "1" {
		fakeService()
		return
	}
	os.Exit(m.Run())
}

func fakeService() {
	in := bufio.NewScanner(os.Stdin)
	in.Buffer(make([]byte, 1<<20), 16<<20)
	var mu sync.Mutex
	out := json.NewEncoder(os.Stdout)
	write := func(v any) { mu.Lock(); defer mu.Unlock(); _ = out.Encode(v) }
	replies := map[string]chan json.RawMessage{}
	var rmu sync.Mutex
	next := 0
	ask := func(method string, params any) json.RawMessage {
		rmu.Lock()
		next++
		id := fmt.Sprintf("fake-%d", next)
		ch := make(chan json.RawMessage, 1)
		replies[id] = ch
		rmu.Unlock()
		write(map[string]any{"id": id, "method": method, "params": params})
		return <-ch
	}
	page := map[string]string{}
	// origin is the site the page may stay on without asking, as the real
	// service reports it: where the agent was allowed to go, or the person went.
	origin := map[string]string{}
	originOf := func(raw string) string {
		u, err := url.Parse(raw)
		if err != nil {
			return ""
		}
		return u.Scheme + "://" + u.Host
	}
	// One request at a time, like a page does; asks wait on their own replies.
	var pmu sync.Mutex
	for in.Scan() {
		var m struct {
			ID     json.RawMessage `json:"id"`
			Method string          `json:"method"`
			Params map[string]any  `json:"params"`
			Result json.RawMessage `json:"result"`
		}
		if json.Unmarshal(in.Bytes(), &m) != nil {
			continue
		}
		if m.Method == "" {
			var id string
			_ = json.Unmarshal(m.ID, &id)
			rmu.Lock()
			if ch := replies[id]; ch != nil {
				ch <- m.Result
				delete(replies, id)
			}
			rmu.Unlock()
			continue
		}
		go func() {
			pmu.Lock()
			defer pmu.Unlock()
			browser, _ := m.Params["browser"].(string)
			state := func() map[string]any {
				return map[string]any{"url": page[browser], "title": "Fake " + page[browser], "viewportWidth": 1280, "viewportHeight": 800, "origin": origin[browser]}
			}
			moved := func(to string) {
				page[browser] = to
				write(map[string]any{"method": "state", "params": map[string]any{"browser": browser, "state": state()}})
			}
			reply := func(v any) { write(map[string]any{"id": m.ID, "result": v}) }
			switch m.Method {
			case "open", "input":
				if page[browser] == "" {
					page[browser] = "about:blank"
				}
				switch m.Params["text"] {
				case "page-navigates-itself":
					// A script on the page moves it while the person holds the browser.
					moved("https://moved.example/")
				case "page-asks-site":
					// The page navigates itself before the person's first input: the service asks.
					var r struct{ Allow bool }
					_ = json.Unmarshal(ask("site", map[string]any{"browser": browser, "url": "https://asked.example/x", "origin": "https://asked.example"}), &r)
					if r.Allow {
						page[browser], origin[browser] = "https://asked.example/x", "https://asked.example"
					}
				}
				reply(state())
			case "screencast":
				if on, _ := m.Params["on"].(bool); on {
					write(map[string]any{"method": "frame", "params": map[string]any{"browser": browser, "data": base64.StdEncoding.EncodeToString([]byte("jpeg")), "width": 1280, "height": 800}})
				}
				reply(map[string]any{})
			case "act":
				action, _ := m.Params["action"].(string)
				switch action {
				case "navigate":
					page[browser], _ = m.Params["url"].(string)
					origin[browser] = originOf(page[browser])
					write(map[string]any{"method": "state", "params": map[string]any{"browser": browser, "state": state()}})
					reply(map[string]any{"text": "Page: " + page[browser], "summary": "Opened " + page[browser], "state": state()})
				case "click":
					ref, _ := m.Params["ref"].(string)
					if ref == "cross" {
						// A link to another site: the service asks before following it.
						var r struct{ Allow bool }
						_ = json.Unmarshal(ask("site", map[string]any{"browser": browser, "url": "https://other.example/x", "origin": "https://other.example"}), &r)
						if r.Allow {
							page[browser], origin[browser] = "https://other.example/x", "https://other.example"
						}
					}

					if ref == "download" {
						var r struct {
							Allow bool
							Path  string
						}
						_ = json.Unmarshal(ask("download", map[string]any{"browser": browser, "url": "https://docs.example/r.pdf", "fileName": "../r.pdf"}), &r)
						write(map[string]any{"method": "log", "params": map[string]any{"browser": browser, "kind": "download", "summary": fmt.Sprintf("download allowed=%v path=%s", r.Allow, r.Path)}})
					}
					reply(map[string]any{"text": "Clicked " + ref, "summary": "Clicked " + ref, "state": state(), "box": map[string]any{"x": 10, "y": 20, "width": 30, "height": 40}})
				case "screenshot":
					reply(map[string]any{"text": "Screenshot", "summary": "Took a screenshot", "state": state(), "image": map[string]any{"mediaType": "image/jpeg", "data": "AAAA"}})
				default:
					reply(map[string]any{"text": action + " done", "summary": action, "state": state()})
				}
			case "close":
				// The page is gone: reopened, the browser starts blank.
				delete(page, browser)
				delete(origin, browser)
				reply(map[string]any{})
			default:
				reply(map[string]any{})
			}
		}()
	}
}

func newManager(t *testing.T, change ...func(*Config)) *Manager {
	t.Helper()
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	cfg := Config{Node: exe, Script: "-test.run=^$", Root: t.TempDir(), BlockedHosts: []string{"127.0.0.1:7777"}, Env: append(os.Environ(), "LAZYBOX_FAKE_BROWSER=1"),
		LoopbackPorts: func(context.Context, string) []int { return []int{5173} }, AskTimeout: 5 * time.Second, HoldTimeout: 5 * time.Second}
	for _, c := range change {
		c(&cfg)
	}
	m, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = m.Close() })
	return m
}

var alice = Caller{Session: "exec-a", Conversation: "conv-a", Workspace: "ws-1", Permission: "ask", Agent: "Claude"}

func call(t *testing.T, m *Manager, c Caller, name string, args any) (agent.BrowserResult, error) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	raw, _ := json.Marshal(args)
	out, err := m.Tool(ctx, c, name, raw)
	if err != nil {
		return agent.BrowserResult{}, err
	}
	return agent.ParseBrowserResult(out), nil
}

// waitRequest returns the first request waiting in conversation.
func waitRequest(t *testing.T, m *Manager, conversation string) Request {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if p := m.Pending(conversation); len(p) > 0 {
			return p[0]
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("no request waited for the person")
	return Request{}
}

func TestFirstVisitToASiteAsksThePersonWithTheAgentsReason(t *testing.T) {
	m := newManager(t)
	done := make(chan error, 1)
	go func() {
		_, err := call(t, m, alice, "browser_navigate", map[string]string{"url": "https://docs.example/guide", "reason": "Read the API guide"})
		done <- err
	}()
	r := waitRequest(t, m, "conv-a")
	if r.Kind != "site" || r.Origin != "https://docs.example" || r.Reason != "Read the API guide" || r.Session != "exec-a" || r.Agent != "Claude" || r.Permission != "ask" {
		t.Fatalf("request %+v", r)
	}
	if s, _ := m.State("conv-a"); !s.Waiting {
		t.Fatal("the browser does not say it waits for the person")
	}
	if _, err := m.Decide(r.ID, "once", ""); err != nil {
		t.Fatal(err)
	}
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	// Same origin: no second ask. Another origin: asked again; "once" covered one visit.
	if _, err := call(t, m, alice, "browser_navigate", map[string]string{"url": "https://docs.example/other"}); err != nil {
		t.Fatal(err)
	}
	go func() {
		_, err := call(t, m, alice, "browser_navigate", map[string]string{"url": "https://news.example/"})
		done <- err
	}()
	r = waitRequest(t, m, "conv-a")
	if _, err := m.Decide(r.ID, "deny", "Stay on the docs"); err != nil {
		t.Fatal(err)
	}
	if err := <-done; err == nil || !strings.Contains(err.Error(), "did not allow news.example: Stay on the docs") {
		t.Fatalf("denial reached the agent as %v", err)
	}
	// The denied page never loaded: the agent is still on docs.example.
	if _, err := call(t, m, alice, "browser_navigate", map[string]string{"url": "https://docs.example/still"}); err != nil {
		t.Fatal(err)
	}
	// Leaving for another site allowed once, then coming back: asked again.
	for _, next := range []string{"https://blog.example/", "https://docs.example/again"} {
		go func() {
			_, err := call(t, m, alice, "browser_navigate", map[string]string{"url": next})
			done <- err
		}()
		r = waitRequest(t, m, "conv-a")
		if _, err := m.Decide(r.ID, "once", ""); err != nil {
			t.Fatal(err)
		}
		if err := <-done; err != nil {
			t.Fatal(err)
		}
	}
}

func TestGrantsForTheConversationAndTheWorkspace(t *testing.T) {
	m := newManager(t)
	bob := Caller{Session: "exec-b", Conversation: "conv-b", Workspace: "ws-1", Agent: "Codex"}
	carol := Caller{Session: "exec-c", Conversation: "conv-c", Workspace: "ws-2", Agent: "Claude"}
	allow := func(c Caller, raw, decision string) {
		t.Helper()
		done := make(chan error, 1)
		go func() { _, err := call(t, m, c, "browser_navigate", map[string]string{"url": raw}); done <- err }()
		r := waitRequest(t, m, c.Conversation)
		if _, err := m.Decide(r.ID, decision, ""); err != nil {
			t.Fatal(err)
		}
		if err := <-done; err != nil {
			t.Fatal(err)
		}
	}
	allow(alice, "https://a.example/", "conversation")
	allow(alice, "https://w.example/", "workspace")
	// Bob shares the workspace: w.example is allowed for him, a.example is not.
	if _, err := call(t, m, bob, "browser_navigate", map[string]string{"url": "https://w.example/page"}); err != nil {
		t.Fatal(err)
	}
	allow(bob, "https://a.example/", "once")
	// Carol's workspace allows neither.
	allow(carol, "https://w.example/", "once")
	if got := m.Settings().WorkspaceSites["ws-1"]; len(got) != 1 || got[0] != "w.example" {
		t.Fatalf("workspace sites %v", got)
	}
	// A conversation's grants outlive the Runner.
	m2, err := New(m.cfg)
	if err != nil {
		t.Fatal(err)
	}
	defer m2.Close()
	m2.mu.Lock()
	b := m2.browserLocked(alice)
	ok := m2.allowedLocked(b, mustURL("https://a.example/x"))
	m2.mu.Unlock()
	if !ok {
		t.Fatal("conversation grant lost on restart")
	}
}

func TestBrowseFreelyAndAllowListSkipTheAskEvenInYOLO(t *testing.T) {
	m := newManager(t)
	yolo := alice
	yolo.Permission = "yolo"
	// YOLO alone still asks.
	done := make(chan error, 1)
	go func() {
		_, err := call(t, m, yolo, "browser_open", map[string]string{"url": "https://free.example/"})
		done <- err
	}()
	r := waitRequest(t, m, "conv-a")
	if r.Permission != "yolo" {
		t.Fatalf("request does not say the conversation is in full access: %+v", r)
	}
	if _, err := m.Decide(r.ID, "deny", ""); err != nil {
		t.Fatal(err)
	}
	<-done
	s := m.Settings()
	s.AllowedSites = []string{"*.trusted.example"}
	if _, err := m.SaveSettings(s); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, m, yolo, "browser_navigate", map[string]string{"url": "https://docs.trusted.example/"}); err != nil {
		t.Fatal("allow-listed site asked", err)
	}
	if _, err := m.Control(context.Background(), alice, "browse_freely", ""); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, m, yolo, "browser_navigate", map[string]string{"url": "https://anything.example/"}); err != nil {
		t.Fatal("browse freely asked", err)
	}
	if st, _ := m.State("conv-a"); !st.BrowseFreely {
		t.Fatal(st)
	}
}

func TestBlockedAddressesAreRefusedWithoutAsking(t *testing.T) {
	m := newManager(t)
	for raw, want := range map[string]string{
		"file:///etc/passwd":        "only http and https",
		"javascript:alert(1)":       "only http and https",
		"http://127.0.0.1:7777/":    "Lazybox itself",
		"http://localhost:7777/x":   "Lazybox itself",
		"http://localhost:9999/":    "only this workspace's running previews",
		"http://192.168.1.10/":      "private network",
		"http://169.254.169.254/":   "private network",
		"http://100.100.1.1/":       "private network",
		"https://user:pw@example.c": "credentials",
		"docs.example":              "absolute URL",
	} {
		_, err := call(t, m, alice, "browser_navigate", map[string]string{"url": raw})
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%s: got %v, want %q", raw, err, want)
		}
	}
	if len(m.Pending("conv-a")) != 0 {
		t.Fatal("a blocked address asked the person")
	}
	// A workspace preview is reachable, and still asked about once like any site.
	done := make(chan error, 1)
	go func() {
		_, err := call(t, m, alice, "browser_navigate", map[string]string{"url": "http://localhost:5173/"})
		done <- err
	}()
	r := waitRequest(t, m, "conv-a")
	_, _ = m.Decide(r.ID, "once", "")
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func TestNavigationTheServiceAsksAboutFollowsTheSamePolicy(t *testing.T) {
	m := newManager(t)
	m.Control(context.Background(), alice, "browse_freely", "")
	if _, err := call(t, m, alice, "browser_navigate", map[string]string{"url": "https://docs.example/"}); err != nil {
		t.Fatal(err)
	}
	m.Control(context.Background(), alice, "ask_sites", "")
	done := make(chan error, 1)
	go func() {
		_, err := call(t, m, alice, "browser_click", map[string]string{"ref": "cross", "element": "Other site link"})
		done <- err
	}()
	r := waitRequest(t, m, "conv-a")
	if r.Origin != "https://other.example" || r.Reason != "" {
		t.Fatalf("%+v", r)
	}
	_, _ = m.Decide(r.ID, "once", "")
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if st, _ := m.State("conv-a"); st.URL != "https://other.example/x" {
		t.Fatalf("followed link: %+v", st)
	}
}

func TestReadOnlyConversationsReadButDoNotAct(t *testing.T) {
	m := newManager(t, func(c *Config) {})
	reader := alice
	reader.Permission = "read-only"
	s := m.Settings()
	s.DefaultPermission = PermissionAllow
	if _, err := m.SaveSettings(s); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, m, reader, "browser_open", map[string]string{"url": "https://docs.example/"}); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, m, reader, "browser_snapshot", nil); err != nil {
		t.Fatal(err)
	}
	for _, tool := range []string{"browser_click", "browser_type", "browser_select", "browser_press"} {
		if _, err := call(t, m, reader, tool, map[string]any{"ref": "e1", "text": "x", "values": []string{"a"}, "key": "Enter"}); err == nil || !strings.Contains(err.Error(), "read-only") {
			t.Errorf("%s in a read-only conversation: %v", tool, err)
		}
	}
}

func TestTakeOverPausesTheAgentAndGiveBackResumesIt(t *testing.T) {
	m := newManager(t)
	m.Control(context.Background(), alice, "browse_freely", "")
	if _, err := call(t, m, alice, "browser_open", map[string]string{"url": "https://docs.example/"}); err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	if _, err := m.Control(ctx, alice, "take_over", ""); err != nil {
		t.Fatal(err)
	}
	// The person browses: no ask, even to a new site.
	if _, err := m.Control(ctx, alice, "navigate", "https://person.example/"); err != nil {
		t.Fatal(err)
	}
	if _, err := m.Input(ctx, "conv-a", map[string]any{"type": "mouse_down", "x": 3.0, "y": 4.0}); err != nil {
		t.Fatal(err)
	}
	result := make(chan agent.BrowserResult, 1)
	go func() { r, _ := call(t, m, alice, "browser_snapshot", nil); result <- r }()
	select {
	case <-result:
		t.Fatal("the agent acted while the person held the browser")
	case <-time.After(200 * time.Millisecond):
	}
	if _, err := m.Control(ctx, alice, "give_back", ""); err != nil {
		t.Fatal(err)
	}
	r := <-result
	if !strings.Contains(r.Text, "The person used the browser and gave it back") {
		t.Fatalf("the agent was not told: %q", r.Text)
	}
	// The site the person chose is the agent's to stay on, once.
	m.Control(ctx, alice, "ask_sites", "")
	if _, err := call(t, m, alice, "browser_navigate", map[string]string{"url": "https://person.example/more"}); err != nil {
		t.Fatal(err)
	}
	// Input is the person's only while they hold the browser.
	if _, err := m.Input(ctx, "conv-a", map[string]any{"type": "text", "text": "x"}); err == nil {
		t.Fatal("input accepted while the agent holds the browser")
	}
}

func TestStopEndsBrowsingUntilThePersonResumesIt(t *testing.T) {
	m := newManager(t)
	ctx := context.Background()
	done := make(chan error, 1)
	go func() {
		_, err := call(t, m, alice, "browser_open", map[string]string{"url": "https://docs.example/"})
		done <- err
	}()
	waitRequest(t, m, "conv-a")
	st, err := m.Control(ctx, alice, "stop", "")
	if err != nil || st.Status != "stopped" {
		t.Fatal(st, err)
	}
	if err := <-done; err == nil || !strings.Contains(err.Error(), "stopped") {
		t.Fatalf("the waiting call was not ended by Stop: %v", err)
	}
	if _, err := call(t, m, alice, "browser_snapshot", nil); !errors.Is(err, errStopped) {
		t.Fatalf("browsing after Stop: %v", err)
	}
	if _, err := m.Control(ctx, alice, "resume", ""); err != nil {
		t.Fatal(err)
	}
	m.Control(ctx, alice, "browse_freely", "")
	if _, err := call(t, m, alice, "browser_open", map[string]string{"url": "https://docs.example/"}); err != nil {
		t.Fatal(err)
	}
}

func TestWatchersGetHistoryFramesAndActionsWithBoxes(t *testing.T) {
	m := newManager(t)
	m.Control(context.Background(), alice, "browse_freely", "")
	if _, err := call(t, m, alice, "browser_open", map[string]string{"url": "https://docs.example/"}); err != nil {
		t.Fatal(err)
	}
	w, first, stop := m.Watch(alice)
	defer stop()
	if first.State == nil || first.State.Status != "open" || len(first.History) == 0 {
		t.Fatalf("first event %+v", first)
	}
	select {
	case f := <-w.Frames():
		if string(f.JPEG) != "jpeg" || f.Width != 1280 {
			t.Fatal(f)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("no frame while watched")
	}
	if _, err := call(t, m, alice, "browser_click", map[string]string{"ref": "e3", "element": "Sign in button"}); err != nil {
		t.Fatal(err)
	}
	var running, done bool
	deadline := time.After(5 * time.Second)
	for !(running && done) {
		select {
		case e := <-w.Events():
			if e.Action == nil || e.Action.Kind != "click" {
				continue
			}
			if e.Action.Status == "running" && e.Action.Summary == "Clicking Sign in button" {
				running = true
			}
			if e.Action.Status == "done" && e.Action.Box != nil && e.Action.Box.Width == 30 {
				done = true
			}
		case <-deadline:
			t.Fatalf("running %v done %v", running, done)
		}
	}
}

func TestScreenshotsComeBackAsImages(t *testing.T) {
	m := newManager(t)
	m.Control(context.Background(), alice, "browse_freely", "")
	r, err := call(t, m, alice, "browser_screenshot", nil)
	if err != nil {
		t.Fatal(err)
	}
	if r.Image == nil || r.Image.MediaType != "image/jpeg" || r.Image.Data != "AAAA" {
		t.Fatalf("%+v", r)
	}
}

func TestDownloadsAskAndSaveUnderTheConversationOrAreBlocked(t *testing.T) {
	m := newManager(t)
	m.Control(context.Background(), alice, "browse_freely", "")
	done := make(chan agent.BrowserResult, 1)
	go func() { r, _ := call(t, m, alice, "browser_click", map[string]string{"ref": "download"}); done <- r }()
	r := waitRequest(t, m, "conv-a")
	if r.Kind != "download" || r.FileName != "r.pdf" {
		t.Fatalf("%+v", r)
	}
	if _, err := m.Decide(r.ID, "workspace", ""); err == nil {
		t.Fatal("a download accepted a site decision")
	}
	_, _ = m.Decide(r.ID, "once", "")
	// What happened besides the step itself is told with its result.
	clicked := <-done
	want := filepath.Join(m.cfg.Root, "downloads", "conv-a", "r.pdf")
	if !strings.Contains(clicked.Text, "Also: Downloaded r.pdf to "+want) {
		t.Fatalf("%q", clicked.Text)
	}
	s := m.Settings()
	s.Downloads = DownloadsBlock
	if _, err := m.SaveSettings(s); err != nil {
		t.Fatal(err)
	}
	if _, err := call(t, m, alice, "browser_click", map[string]string{"ref": "download"}); err != nil {
		t.Fatal(err)
	}
	if len(m.Pending("conv-a")) != 0 {
		t.Fatal("a blocked download asked")
	}
}

func TestOffTurnsTheToolsOffAndSettingsValidate(t *testing.T) {
	m := newManager(t)
	s := m.Settings()
	s.DefaultPermission = PermissionOff
	saved, err := m.SaveSettings(s)
	if err != nil {
		t.Fatal(err)
	}
	if m.Enabled() {
		t.Fatal("off still offers the tools")
	}
	if _, err := call(t, m, alice, "browser_open", nil); err == nil || !strings.Contains(err.Error(), "Settings → Browser") {
		t.Fatal(err)
	}
	if _, err := m.SaveSettings(s); !errors.Is(err, ErrConflict) {
		t.Fatal("stale save accepted", err)
	}
	bad := saved
	bad.AllowedSites = []string{"not a site"}
	if _, err := m.SaveSettings(bad); err == nil {
		t.Fatal("invalid site accepted")
	}
	good := saved
	good.AllowedSites = []string{"https://Docs.Example/path", "*.trusted.example", "docs.example"}
	got, err := m.SaveSettings(good)
	if err != nil || strings.Join(got.AllowedSites, ",") != "*.trusted.example,docs.example" {
		t.Fatal(got.AllowedSites, err)
	}
	info, err := os.Stat(filepath.Join(m.cfg.Root, "settings.json"))
	if err != nil || info.Mode().Perm() != 0o600 {
		t.Fatal("settings are not private", err)
	}
}

func TestUnavailableWithoutTheService(t *testing.T) {
	m := newManager(t, func(c *Config) { c.Script = "" })
	if m.Available() || m.Enabled() {
		t.Fatal("offered without a service")
	}
	if _, err := call(t, m, alice, "browser_open", nil); !errors.Is(err, ErrUnavailable) {
		t.Fatal(err)
	}
}

func TestClearDataDeletesTheProfileAndWorkspaceSites(t *testing.T) {
	m := newManager(t)
	m.Control(context.Background(), alice, "browse_freely", "")
	if _, err := call(t, m, alice, "browser_open", map[string]string{"url": "https://docs.example/"}); err != nil {
		t.Fatal(err)
	}
	s := m.Settings()
	s.WorkspaceSites = map[string][]string{"ws-1": {"docs.example"}}
	if _, err := m.SaveSettings(s); err != nil {
		t.Fatal(err)
	}
	profile := filepath.Join(m.cfg.Root, "profiles", "ws-1")
	if _, err := os.Stat(profile); err != nil {
		t.Fatal("no profile", err)
	}
	after, err := m.ClearData(context.Background(), "ws-1")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(profile); !os.IsNotExist(err) || len(after.WorkspaceSites["ws-1"]) != 0 {
		t.Fatal("profile or sites survived", err, after.WorkspaceSites)
	}
	if st, _ := m.State("conv-a"); st.Status != "closed" {
		t.Fatal(st)
	}
	if _, err := m.ClearData(context.Background(), "../escape"); err == nil {
		t.Fatal("unsafe workspace accepted")
	}
}

func TestPolicyHelpers(t *testing.T) {
	for raw, want := range map[string]string{"https://Example.com:443/a": "https://example.com", "http://example.com:8080/": "http://example.com:8080", "http://[::1]:3000/": "http://[::1]:3000"} {
		if got := Origin(mustURL(raw)); got != want {
			t.Errorf("Origin(%s) = %s, want %s", raw, got, want)
		}
	}
	for entry, cases := range map[string]map[string]bool{
		"example.com":    {"https://example.com/": true, "https://www.example.com/": false, "https://example.com:8443/": false},
		"*.example.com":  {"https://example.com/": true, "https://a.b.example.com/": true, "https://badexample.com/": false},
		"localhost:5173": {"http://localhost:5173/": true, "http://localhost:5174/": false},
	} {
		for raw, want := range cases {
			if got := SiteMatches(entry, mustURL(raw)); got != want {
				t.Errorf("SiteMatches(%s, %s) = %v", entry, raw, got)
			}
		}
	}
	for _, bad := range []string{"", "*", "a b.com", "*.example.com:80", "nohost"} {
		if _, err := NormalizeSite(bad); err == nil {
			t.Errorf("NormalizeSite(%q) accepted", bad)
		}
	}
	if got := safeFileName("../../etc/pass wd?.pdf"); got != "pass wd_.pdf" {
		t.Errorf("safeFileName = %q", got)
	}
}

func mustURL(raw string) *url.URL {
	u, err := url.Parse(raw)
	if err != nil {
		panic(err)
	}
	return u
}
