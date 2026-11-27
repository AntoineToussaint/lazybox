package browser

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"regexp"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent"
)

// End to end with the real browser service (workers/browser) and Chromium:
// `make test` builds it and names it in LAZYBOX_V2_BROWSER_WORKER. Without
// it the test is skipped; the service's own tests cover it in depth.
func TestRealServiceNavigatesReadsClicksAndShowsFrames(t *testing.T) {
	script := os.Getenv("LAZYBOX_V2_BROWSER_WORKER")
	if script == "" {
		t.Skip("LAZYBOX_V2_BROWSER_WORKER is not set; run through make test")
	}
	if _, err := os.Stat(script); err != nil {
		t.Skip("browser service not built: ", err)
	}
	site := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "text/html")
		switch r.URL.Path {
		case "/":
			fmt.Fprint(w, `<title>Fixture home</title><h1>Fixture home</h1><a href="/next">Next page</a>`)
		default:
			fmt.Fprint(w, `<title>Next</title><h1>The next page</h1>`)
		}
	}))
	defer site.Close()
	_, port, _ := net.SplitHostPort(strings.TrimPrefix(site.URL, "http://"))
	var p int
	fmt.Sscan(port, &p)
	var previewPort atomic.Int32
	previewPort.Store(int32(p))
	root, err := os.MkdirTemp("", "browser-e2e-")
	if err != nil {
		t.Fatal(err)
	}
	// Chromium can still be flushing its cache as the test ends; a cleanup that
	// races it must not fail the test.
	t.Cleanup(func() { _ = os.RemoveAll(root) })
	m, err := New(Config{Script: script, Root: root, LoopbackPorts: func(context.Context, string) []int { return []int{int(previewPort.Load())} }, AskTimeout: 30 * time.Second})
	if err != nil {
		t.Fatal(err)
	}
	defer m.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 90*time.Second)
	defer cancel()
	c := Caller{Session: "e2e", Conversation: "e2e", Workspace: "ws-e2e", Agent: "Claude"}
	w, _, stop := m.Watch(c)
	defer stop()
	done := make(chan error, 1)
	var opened string
	go func() {
		out, err := m.Tool(ctx, c, "browser_open", []byte(`{"url":"`+strings.Replace(site.URL, "127.0.0.1", "localhost", 1)+`/","reason":"Check the fixture"}`))
		opened = agent.ParseBrowserResult(out).Text
		done <- err
	}()
	r := waitRequest(t, m, "e2e")
	if _, err := m.Decide(r.ID, "workspace_previews", ""); err != nil {
		t.Fatal(err)
	}
	if err := <-done; err != nil {
		if strings.Contains(err.Error(), "not installed") {
			t.Skip(err)
		}
		t.Fatal(err)
	}
	if !strings.Contains(opened, "Fixture home") {
		t.Fatalf("open: %s", opened)
	}
	ref := regexp.MustCompile(`link "Next page" \[ref=(e\d+)\]`).FindStringSubmatch(opened)
	if ref == nil {
		t.Fatalf("no ref for the link in %s", opened)
	}
	clicked, err := m.Tool(ctx, c, "browser_click", []byte(`{"ref":"`+ref[1]+`","element":"Next page link"}`))
	if err != nil || !strings.Contains(agent.ParseBrowserResult(clicked).Text, "The next page") {
		t.Fatalf("click: %s %v", clicked, err)
	}
	select {
	case f := <-w.Frames():
		if len(f.JPEG) < 100 || f.JPEG[0] != 0xFF || f.JPEG[1] != 0xD8 {
			t.Fatalf("not a JPEG frame: %d bytes", len(f.JPEG))
		}
	case <-ctx.Done():
		t.Fatal("no frame while watched")
	}
	shot, err := m.Tool(ctx, c, "browser_screenshot", nil)
	if err != nil || !strings.Contains(shot, `"mediaType":"image/jpeg"`) {
		t.Fatalf("screenshot: %.200s %v", shot, err)
	}
	if st, _ := m.State("e2e"); st.Status != "open" || !strings.HasSuffix(st.URL, "/next") {
		t.Fatalf("state %+v", st)
	}
	next := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprint(w, "<h1>Restarted preview</h1>")
	}))
	defer next.Close()
	previewPort.Store(int32(next.Listener.Addr().(*net.TCPAddr).Port))
	target := strings.Replace(next.URL, "127.0.0.1", "localhost", 1)
	result, err := m.Tool(ctx, c, "browser_navigate", []byte(`{"url":"`+target+`"}`))
	if err != nil || !strings.Contains(agent.ParseBrowserResult(result).Text, "Restarted preview") {
		t.Fatalf("same browser after preview restart: %s %v", result, err)
	}

}
