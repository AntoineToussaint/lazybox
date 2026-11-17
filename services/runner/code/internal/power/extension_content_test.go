package power

import (
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func openBoard(t *testing.T, origin string) (*Extensions, ExtensionSession, string) {
	t.Helper()
	dir := t.TempDir()
	folder := writeExtension(t, dir, "board", boardManifest)
	if err := os.WriteFile(filepath.Join(folder, "app.js"), []byte("lazybox.ready"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(folder, ".secret.js"), []byte("hidden"), 0o600); err != nil {
		t.Fatal(err)
	}
	e := NewExtensions(dir, filepath.Join(t.TempDir(), "extensions.json"))
	t.Cleanup(e.Close)
	info, _ := e.Snapshot().Find("board")
	if _, err := e.Approve("board", info.Manifest.Grants, info.Manifest.Hosts); err != nil {
		t.Fatal(err)
	}
	s, err := e.Open("board", "", origin)
	if err != nil {
		t.Fatal(err)
	}
	return e, s, dir
}

func get(t *testing.T, address string) (*http.Response, string) {
	t.Helper()
	resp, err := http.Get(address)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	return resp, string(body)
}

// The shell is sandboxed and may frame only this session's own pages; an
// extension's page runs scripts and styles from its own files alone and
// reaches only the approved hosts. Nothing loosens either policy.
func TestExtensionContentPolicies(t *testing.T) {
	_, s, _ := openBoard(t, "http://127.0.0.1:5173")
	if !strings.HasPrefix(s.Base, "http://127.0.0.1:") || !strings.HasPrefix(s.URL, s.Base) {
		t.Fatalf("%+v", s)
	}
	resp, body := get(t, s.URL)
	shell := resp.Header.Get("Content-Security-Policy")
	if resp.StatusCode != 200 || !strings.Contains(body, "shell.js") {
		t.Fatalf("shell: %d %s", resp.StatusCode, body)
	}
	for _, want := range []string{"default-src 'none'", "script-src " + s.Base + "_lazybox/shell.js;", "frame-src " + s.Base + ";", "sandbox allow-scripts", "frame-ancestors http://127.0.0.1:5173", "form-action 'none'", "webrtc 'block'"} {
		if !strings.Contains(shell, want) {
			t.Errorf("shell policy lacks %q: %s", want, shell)
		}
	}
	resp, _ = get(t, s.Base+"index.html")
	page := resp.Header.Get("Content-Security-Policy")
	for _, want := range []string{"default-src 'none'", "script-src " + s.Base + ";", "style-src " + s.Base + ";", "connect-src https://api.example.com;", "frame-src 'none'", "worker-src 'none'", "base-uri 'none'", "form-action 'none'", "webrtc 'block'", "sandbox allow-scripts"} {
		if !strings.Contains(page, want) {
			t.Errorf("page policy lacks %q: %s", want, page)
		}
	}
	for _, never := range []string{"unsafe-inline", "unsafe-eval", "'self'", "allow-same-origin", "allow-top-navigation", "allow-popups", "allow-forms", " *"} {
		if strings.Contains(page, never) || strings.Contains(shell, never) {
			t.Errorf("a policy holds %q", never)
		}
	}
	if resp.Header.Get("X-Content-Type-Options") != "nosniff" || resp.Header.Get("Referrer-Policy") != "no-referrer" || resp.Header.Get("Content-Type") != "text/html; charset=utf-8" {
		t.Errorf("headers: %v", resp.Header)
	}
	// Lazybox's own files, under _lazybox/.
	for _, name := range []string{"lazybox.js", "lazybox.css", "shell.js", "shell.css"} {
		if resp, _ := get(t, s.Base+"_lazybox/"+name); resp.StatusCode != 200 {
			t.Errorf("%s: %d", name, resp.StatusCode)
		}
	}
	// No hosts: no network at all.
	if got := ExtensionPolicy(s.Base, nil); !strings.Contains(got, "connect-src 'none'") {
		t.Errorf("%s", got)
	}
	// A desktop origin is no http(s) origin: no frame-ancestors rather than a wrong one.
	if got := ShellPolicy(s.Base, "tauri://localhost"); strings.Contains(got, "frame-ancestors") {
		t.Errorf("%s", got)
	}
}

// Only this session's extension's own files, while the session lives.
func TestExtensionContentServesOnlyItsOwnFiles(t *testing.T) {
	e, s, dir := openBoard(t, "")
	if err := os.WriteFile(filepath.Join(dir, "secret.txt"), []byte("outside"), 0o600); err != nil {
		t.Fatal(err)
	}
	if resp, body := get(t, s.Base+"app.js"); resp.StatusCode != 200 || body != "lazybox.ready" || !strings.HasPrefix(resp.Header.Get("Content-Type"), "text/javascript") {
		t.Fatalf("own file: %d %q", resp.StatusCode, body)
	}
	for _, p := range []string{"../secret.txt", "%2e%2e/secret.txt", "..%2fsecret.txt", ".secret.js", "extension.yaml", "_lazybox/policy.json", "_lazybox/lazybox.d.ts", "missing.js", "", "./app.js"} {
		resp, body := get(t, s.Base+p)
		if resp.StatusCode == 200 {
			t.Errorf("%s served: %q", p, body)
		}
	}
	// A raw path the client would not normalize.
	conn, err := net.Dial("tcp", strings.TrimPrefix(strings.Split(s.Base, "/x/")[0], "http://"))
	if err != nil {
		t.Fatal(err)
	}
	u, _ := url.Parse(s.Base)
	_, _ = io.WriteString(conn, "GET "+u.Path+"../../secret.txt HTTP/1.1\r\nHost: "+u.Host+"\r\nConnection: close\r\n\r\n")
	raw, _ := io.ReadAll(conn)
	conn.Close()
	if strings.Contains(string(raw), "outside") {
		t.Fatal("a raw traversal was served")
	}
	// Another Host (DNS rebinding) is refused; writes are refused.
	req, _ := http.NewRequest(http.MethodGet, s.Base+"app.js", nil)
	req.Host = "attacker.example:80"
	if resp, err := http.DefaultClient.Do(req); err != nil || resp.StatusCode != http.StatusMisdirectedRequest {
		t.Fatalf("rebinding: %v %v", resp, err)
	}
	if resp, err := http.Post(s.Base+"app.js", "text/plain", strings.NewReader("x")); err != nil || resp.StatusCode != http.StatusMethodNotAllowed {
		t.Fatalf("post: %v %v", resp, err)
	}
	// An unknown session, and one revoked by turning the extension off.
	if resp, _ := get(t, strings.Replace(s.Base, "/x/", "/x/z", 1)+"app.js"); resp.StatusCode != http.StatusGone {
		t.Fatalf("unknown session: %d", resp.StatusCode)
	}
	if _, err := e.SetEnabled("board", false); err != nil {
		t.Fatal(err)
	}
	if resp, _ := get(t, s.Base+"app.js"); resp.StatusCode != http.StatusGone {
		t.Fatalf("revoked session: %d", resp.StatusCode)
	}
}
