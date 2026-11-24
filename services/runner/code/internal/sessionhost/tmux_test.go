package sessionhost

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// tmuxRoot is a host root whose state directory (its parent) is private to the
// test, so the test gets a tmux server of its own and stops only that one.
func tmuxRoot(t *testing.T) (string, Tmux) {
	t.Helper()
	backend := Tmux{}
	if _, err := backend.Version(context.Background()); err != nil {
		t.Skipf("tmux unavailable: %v", err)
	}
	state, err := os.MkdirTemp("", "lbx-tmux-")
	if err != nil {
		t.Fatal(err)
	}
	dir := filepath.Join(state, "hosts")
	t.Cleanup(func() {
		ids, _ := Hosts(dir)
		for _, id := range ids {
			_ = Stop(context.Background(), dir, id)
		}
		_ = exec.Command("tmux", "-L", ServerName(dir), "kill-server").Run()
		// tmux leaves its socket file behind; this one was the test's.
		tmp := os.Getenv("TMUX_TMPDIR")
		if tmp == "" {
			tmp = "/tmp"
		}
		_ = os.Remove(filepath.Join(tmp, fmt.Sprintf("tmux-%d", os.Getuid()), ServerName(dir)))
		_ = os.RemoveAll(state)
	})
	return dir, backend
}

func tmuxRun(t *testing.T, d Description, args ...string) string {
	t.Helper()
	out, err := exec.Command(d.Tmux.Path, append([]string{"-S", d.Tmux.Socket}, args...)...).CombinedOutput()
	if err != nil {
		t.Fatalf("tmux %v: %v: %s", args, err, out)
	}
	return strings.TrimSpace(string(out))
}

func TestTmuxHostKeepsTheDeliveryContract(t *testing.T) {
	dir, backend := tmuxRoot(t)
	ctx := deadline(t)
	c, err := backend.Launch(ctx, dir, Options{Kind: "agent", Owner: "execution-1", Label: "claude · fixture"})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, agentSpec()); err != nil {
		t.Fatal(err)
	}
	if name, _ := method(t, next(t, c)); name != "ready" {
		t.Fatalf("agent did not start: %s", name)
	}
	_ = c.Ack(1)

	d, err := backend.Describe(dir, c.ID())
	if err != nil {
		t.Fatal(err)
	}
	if d.Backend != BackendTmux || d.Kind != "agent" || d.Owner != "execution-1" || d.Label != "claude · fixture" || d.Tmux == nil || d.Tmux.Socket == "" || d.Tmux.Session != c.ID() {
		t.Fatalf("unexpected description %+v", d)
	}
	if want := "tmux -S " + shellQuote(d.Tmux.Socket) + " attach -t " + c.ID(); d.AttachCommand() != want {
		t.Fatalf("attach command %q, want %q", d.AttachCommand(), want)
	}
	if !strings.HasPrefix(filepath.Base(d.Tmux.Socket), "lazybox-") {
		t.Fatalf("server is not the private one: %s", d.Tmux.Socket)
	}
	if got := tmuxRun(t, d, "display-message", "-p", "-t", c.ID(), "#{window_name}"); got != "claude · fixture" {
		t.Fatalf("window name %q", got)
	}

	// An unacknowledged line is replayed to the next Runner, as from any host.
	if err = c.Send(json.RawMessage(`{"method":"echo","params":"kept"}`)); err != nil {
		t.Fatal(err)
	}
	if name, _ := method(t, next(t, c)); name != "echoed" {
		t.Fatal("no echo")
	}
	if err = c.Detach(); err != nil {
		t.Fatal(err)
	}
	again, err := Attach(ctx, dir, c.ID(), 1)
	if err != nil {
		t.Fatal(err)
	}
	m := next(t, again)
	if name, params := method(t, m); name != "echoed" || string(params) != `"kept"` || !m.Redelivered {
		t.Fatalf("replay %s redelivered=%t", m.Line, m.Redelivered)
	}
	_ = again.Ack(m.Seq)

	// The window shows the traffic, both ways, for a person who attaches.
	eventually(t, "window does not show the agent's lines", func() bool {
		pane := tmuxRun(t, d, "capture-pane", "-p", "-t", c.ID())
		return strings.Contains(pane, `> {"method":"echo","params":"kept"}`) && strings.Contains(pane, `< {"method":"echoed","params":"kept"}`)
	})

	if err = Stop(ctx, dir, c.ID()); err != nil {
		t.Fatal(err)
	}
	eventually(t, "the session outlived its host", func() bool {
		return exec.Command(d.Tmux.Path, "-S", d.Tmux.Socket, "has-session", "-t", c.ID()).Run() != nil
	})
}

func TestClosingATmuxWindowStopsItsProgram(t *testing.T) {
	dir, backend := tmuxRoot(t)
	ctx := deadline(t)
	c, err := backend.Launch(ctx, dir, Options{Kind: "agent"})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, agentSpec()); err != nil {
		t.Fatal(err)
	}
	next(t, c)
	pid := c.AgentPID()
	d, _ := backend.Describe(dir, c.ID())
	tmuxRun(t, d, "kill-window", "-t", c.ID())
	// The Runner still gets the exit, after the window is gone.
	for {
		_, exit, err := c.Next(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if exit != nil {
			break
		}
	}
	eventually(t, "the agent outlived its window", func() bool { return !alive(pid) })
	_ = c.Release()
	eventually(t, "the host outlived its program", func() bool {
		_, err := os.Stat(filepath.Join(dir, c.ID()))
		return os.IsNotExist(err)
	})
}

func TestTmuxTerminalTakesKeysFromItsWindow(t *testing.T) {
	dir, backend := tmuxRoot(t)
	ctx := deadline(t)
	c, err := backend.Launch(ctx, dir, Options{Kind: "terminal", Label: "shell"})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, terminalSpec()); err != nil {
		t.Fatal(err)
	}
	var out strings.Builder
	readTerminal(t, c, &out, "ready tty=true size=24x80", true)
	d, _ := backend.Describe(dir, c.ID())
	// Keys typed in the window reach the shell; its output reaches the Runner.
	tmuxRun(t, d, "send-keys", "-t", c.ID(), "-l", "from-tmux")
	tmuxRun(t, d, "send-keys", "-t", c.ID(), "Enter")
	readTerminal(t, c, &out, "got:from-tmux", true)
	// The window follows the size the app sets.
	if err = c.Resize(Size{Cols: 120, Rows: 40}); err != nil {
		t.Fatal(err)
	}
	readTerminal(t, c, &out, "resized=40x120", true)
	eventually(t, "window did not follow the terminal's size", func() bool {
		return tmuxRun(t, d, "display-message", "-p", "-t", c.ID(), "#{window_width}x#{window_height}") == "120x40"
	})
	eventually(t, "window does not show the shell", func() bool {
		return strings.Contains(tmuxRun(t, d, "capture-pane", "-p", "-t", c.ID()), "got:from-tmux")
	})
}

func TestTmuxLaunchWithoutTmuxSaysSoAndLeavesNothing(t *testing.T) {
	dir := root(t)
	_, err := Tmux{Path: filepath.Join(t.TempDir(), "no-tmux")}.Launch(deadline(t), dir, Options{})
	if err == nil || !strings.Contains(err.Error(), "tmux not found") {
		t.Fatalf("expected a clear error, got %v", err)
	}
	if ids, _ := Hosts(dir); len(ids) != 0 {
		t.Fatalf("left hosts behind: %v", ids)
	}
}

func TestAHostBeingLaunchedIsHeld(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "id-being-launched")
	done := launching(dir)
	if !Held(filepath.Dir(dir), filepath.Base(dir)) {
		t.Fatal("a sweep would take a host on its way up for an orphan")
	}
	done()
	done()
	if Held(filepath.Dir(dir), filepath.Base(dir)) {
		t.Fatal("still held after its launch ended")
	}
}

func TestLocalBackendRecordsWhatItHosts(t *testing.T) {
	dir := root(t)
	c, err := Local{}.Launch(deadline(t), dir, Options{Kind: "command", Owner: "cmd-1", Label: "go test ./..."})
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = Stop(context.Background(), dir, c.ID()) }()
	d, err := Local{}.Describe(dir, c.ID())
	if err != nil {
		t.Fatal(err)
	}
	if d.Backend != BackendSessionHost || d.Kind != "command" || d.Owner != "cmd-1" || d.Label != "go test ./..." || d.AttachCommand() != "" || d.PID == 0 {
		t.Fatalf("unexpected description %+v", d)
	}
}

// A tmux server a person can open windows in never holds the Runner's
// credentials or Codefly wiring.
func TestTmuxEnvironmentDropsTheRunnersWiring(t *testing.T) {
	t.Setenv("LAZYBOX_V2_ENDPOINT", "/owner/endpoint.json")
	t.Setenv("CODEFLY_AGENT_TOKEN", "runner-codefly-secret")
	t.Setenv("CODEFLY_CONTAINER_RECOVERY_SCOPE", "4242:v2:runner:ns")
	t.Setenv("TMUX", "/tmp/other,1,0")
	t.Setenv("KEEP_ME", "yes")
	kept := false
	for _, entry := range tmuxEnv() {
		if strings.HasPrefix(entry, "LAZYBOX_V2_") || strings.HasPrefix(entry, "CODEFLY") || strings.HasPrefix(entry, "TMUX=") {
			t.Fatalf("tmux inherits %q", entry)
		}
		kept = kept || entry == "KEEP_ME=yes"
	}
	if !kept {
		t.Fatal("the person's own environment was dropped")
	}
}
