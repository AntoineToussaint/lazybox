package commands

import (
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestVerificationIsolation(t *testing.T) {
	outside := filepath.Join(t.TempDir(), "secret")
	if err := os.WriteFile(outside, []byte("private"), 0600); err != nil {
		t.Fatal(err)
	}
	var requests atomic.Int32
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { requests.Add(1) }))
	defer server.Close()
	m := manager(t, 1<<20)
	tests := []struct {
		name, script string
		pass         bool
	}{
		{"inside", "printf ok > result; test \"$(cat result)\" = ok", true},
		{"outside write", "printf changed > \"$1\"", false},
		{"outside read", "cat \"$1\"", false},
		{"environment", "test -z \"$LAZYBOX_SECRET\"", true},
		{"symlink", "ln -s \"$1\" link; cat link", false},
		// A test's caches, dependencies' caches and temporary files.
		{"write a cache", "mkdir -p .cache/tool node_modules/.cache && printf built > .cache/tool/receipt && printf x > node_modules/.cache/x && printf t > \"$TMPDIR/t\" && test \"$(cat .cache/tool/receipt)\" = built", true},
	}
	type netTest = struct {
		name, script string
		pass         bool
	}
	if runtime.GOOS == "linux" {
		// The command has its own network namespace: the host's servers,
		// loopback included, are out of reach.
		tests = append(tests, netTest{"host loopback", "curl --max-time 1 --silent " + server.URL, false})
	} else {
		// curl cannot run in the macOS sandbox at all (it reads
		// /private/etc/ssl): nc, which needs nothing outside it, can.
		address := strings.TrimPrefix(server.URL, "http://")
		host, port, _ := strings.Cut(address, ":")
		tests = append(tests,
			netTest{"loopback", "printf 'GET / HTTP/1.0\\r\\n\\r\\n' | nc -w 3 " + host + " " + port + " | grep -q '200 OK'", true},
			// Another machine is refused before a packet leaves, not timed out.
			netTest{"other machine", "nc -v -z -w 3 192.0.2.1 80 2>&1 | grep -q 'not permitted'", true})
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			c, err := m.Start(Spec{SessionID: "test", Kind: "verification", Action: "review/1/check/base", Cwd: t.TempDir(), Argv: []string{"sh", "-c", test.script, "sh", outside}, Env: []string{"LAZYBOX_SECRET=private"}})
			if err != nil {
				t.Fatal(err)
			}
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			state, err := c.Wait(ctx)
			output := c.Tail(ctx, 10000)
			if err != nil || state.ExitCode == nil || state.Signal != "" || (*state.ExitCode == 0) != test.pass {
				t.Fatalf("state=%+v err=%v output=%s", state, err, output)
			}
		})
	}
	if runtime.GOOS == "linux" && requests.Load() != 0 {
		t.Fatal("verification reached the host's loopback")
	}
	data, err := os.ReadFile(outside)
	if err != nil || string(data) != "private" {
		t.Fatal("external file changed")
	}
}

// requireSandbox skips where the platform's isolation runtime is absent:
// verification then refuses to run anything, which TestVerificationIsolation
// covers, and there is no sandbox whose rules to pin.
func requireSandbox(t *testing.T) {
	t.Helper()
	binary := map[string]string{"darwin": "sandbox-exec", "linux": "bwrap"}[runtime.GOOS]
	if binary == "" {
		t.Skipf("no verification sandbox on %s", runtime.GOOS)
	}
	if _, err := exec.LookPath(binary); err != nil {
		t.Skipf("%s is not installed", binary)
	}
}

// A test's own server, as a UI gate's dev server is: it listens on loopback
// inside the sandbox and the test reaches it there, on macOS (Seatbelt's
// localhost rule) and on Linux (bubblewrap's private namespace brings its
// own loopback up). The helper is this test binary, copied into the
// checkout: the one program sure to be there on both.
func TestVerificationServesOnLoopback(t *testing.T) {
	requireSandbox(t)
	self, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	binary, err := os.ReadFile(self)
	if err != nil {
		t.Fatal(err)
	}
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "helper"), binary, 0o755); err != nil {
		t.Fatal(err)
	}
	m := manager(t, 1<<20)
	c, err := m.Start(Spec{SessionID: "test", Kind: "verification", Action: "review/1/serve/candidate", Cwd: root, Argv: []string{filepath.Join(root, "helper"), "-test.run=^TestLoopbackHelper$", "-test.v", loopbackHelperMarker}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = c.Cancel() })
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	state, err := c.Wait(ctx)
	output := string(c.Tail(ctx, 10000))
	if err != nil || state.ExitCode == nil || *state.ExitCode != 0 || !strings.Contains(output, "echoed: served") {
		t.Fatalf("state=%+v err=%v output=%s", state, err, output)
	}
}

const loopbackHelperMarker = "loopback-helper"

// TestLoopbackHelper is TestVerificationServesOnLoopback's program inside
// the sandbox: a server on 127.0.0.1 and a client that reaches it. Run
// directly, it does nothing.
func TestLoopbackHelper(t *testing.T) {
	if os.Args[len(os.Args)-1] != loopbackHelperMarker {
		t.Skip("the helper of TestVerificationServesOnLoopback")
	}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	defer listener.Close()
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		_, _ = io.Copy(conn, conn)
	}()
	conn, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	defer conn.Close()
	if _, err := conn.Write([]byte("served")); err != nil {
		t.Fatalf("write: %v", err)
	}
	_ = conn.(*net.TCPConn).CloseWrite()
	echoed, err := io.ReadAll(conn)
	if err != nil || string(echoed) != "served" {
		t.Fatalf("echoed %q: %v", echoed, err)
	}
	fmt.Printf("echoed: %s\n", echoed)
}

func TestCommandDeadlineIndependentOfReviewPreparation(t *testing.T) {
	m := manager(t, 1<<20)
	c, err := m.Start(Spec{SessionID: "test", Cwd: t.TempDir(), Argv: []string{"sh", "-c", "sleep 30"}, MaxDurationMs: 100})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	state, err := c.Wait(ctx)
	if err != nil || !state.Done || (state.ExitCode != nil && *state.ExitCode == 0) {
		t.Fatalf("deadline did not cancel: %+v %v", state, err)
	}
}

func TestVerificationCannotReadOtherProcessCredentials(t *testing.T) {
	victim := exec.Command("/bin/sleep", "30")
	victim.Env = []string{"VERIFICATION_PRIVATE_TOKEN=never-readable-by-reviewer"}
	if err := victim.Start(); err != nil {
		t.Fatal(err)
	}
	defer func() { _ = victim.Process.Kill(); _ = victim.Wait() }()
	m := manager(t, 1<<20)
	c, err := m.Start(Spec{SessionID: "test", Kind: "verification", Action: "review/1/process/base", Cwd: t.TempDir(), Argv: []string{"sh", "-c", `ps eww -p "$1"; cat /proc/"$1"/environ`, "sh", strconv.Itoa(victim.Process.Pid)}})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	if _, err = c.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(c.Tail(ctx, 10000)), "never-readable-by-reviewer") {
		t.Fatal("sandbox exposed another process credential")
	}
}
