package sessionhost

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"testing"

	"github.com/creack/pty"
)

// fakeTerminalProgram reports what an interactive program sees on its terminal,
// echoes each input line, reports size changes, and exits on "exit".
func fakeTerminalProgram() {
	rows, cols, err := pty.Getsize(os.Stdin)
	fmt.Printf("ready tty=%t size=%dx%d sigint-ignored=%t sighup-ignored=%t\r\n", err == nil, rows, cols, signal.Ignored(syscall.SIGINT), signal.Ignored(syscall.SIGHUP))
	winch := make(chan os.Signal, 1)
	signal.Notify(winch, syscall.SIGWINCH)
	go func() {
		for range winch {
			rows, cols, _ := pty.Getsize(os.Stdin)
			fmt.Printf("resized=%dx%d\r\n", rows, cols)
		}
	}()
	s := bufio.NewScanner(os.Stdin)
	for s.Scan() {
		if line := s.Text(); line == "exit" {
			os.Exit(3)
		} else {
			fmt.Printf("got:%s\r\n", line)
		}
	}
}

func terminalSpec() Spec {
	self, _ := os.Executable()
	return Spec{Path: self, Env: append(os.Environ(), "SESSIONHOST_FIXTURE=terminal"), Cwd: os.TempDir(), Terminal: &Size{Cols: 80, Rows: 24}}
}

func startTerminal(t *testing.T, dir string) *Client {
	t.Helper()
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, terminalSpec()); err != nil {
		t.Fatal(err)
	}
	return c
}

// readTerminal reads terminal output until it contains want, acknowledging it or not.
func readTerminal(t *testing.T, c *Client, seen *strings.Builder, want string, ack bool) {
	t.Helper()
	ctx := deadline(t)
	for !strings.Contains(seen.String(), want) {
		m, exit, err := c.Next(ctx)
		if err != nil || exit != nil {
			t.Fatalf("waiting for %q: exit=%+v err=%v; output so far %q", want, exit, err, seen.String())
		}
		var chunk Chunk
		if err = json.Unmarshal(m.Line, &chunk); err != nil || chunk.Channel != TerminalChannel {
			t.Fatalf("unexpected terminal line %s", m.Line)
		}
		seen.Write(chunk.Data)
		if ack {
			_ = c.Ack(m.Seq)
		}
	}
}

// exitOf reads until the program's exit.
func exitOf(t *testing.T, c *Client) Outcome {
	t.Helper()
	ctx := deadline(t)
	for {
		m, exit, err := c.Next(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if exit != nil {
			return exit.Outcome
		}
		_ = c.Ack(m.Seq)
	}
}

func TestTerminalHostRunsAnInteractiveProgram(t *testing.T) {
	dir := root(t)
	c := startTerminal(t, dir)
	var out strings.Builder
	// A controlling terminal of the requested size, and default signal
	// dispositions: the host's own ignored SIGINT and SIGHUP must not leak in.
	readTerminal(t, c, &out, "ready tty=true size=24x80 sigint-ignored=false sighup-ignored=false", true)
	if err := c.Input([]byte("hello\n")); err != nil {
		t.Fatal(err)
	}
	readTerminal(t, c, &out, "got:hello", true)
	if err := c.Resize(Size{Cols: 120, Rows: 40}); err != nil {
		t.Fatal(err)
	}
	readTerminal(t, c, &out, "resized=40x120", true)
	if err := c.Input([]byte("exit\n")); err != nil {
		t.Fatal(err)
	}
	if outcome := exitOf(t, c); !outcome.Started || outcome.ExitCode != 3 {
		t.Fatalf("unexpected exit %+v", outcome)
	}
	_ = c.Release()
	eventually(t, "host did not exit after release", func() bool {
		_, err := os.Stat(filepath.Join(dir, c.ID()))
		return os.IsNotExist(err)
	})
}

func TestTerminalScrollbackIsHandedToTheNextRunnerOnce(t *testing.T) {
	dir := root(t)
	c := startTerminal(t, dir)
	var out strings.Builder
	readTerminal(t, c, &out, "ready", true)
	_ = c.Input([]byte("before\n"))
	readTerminal(t, c, &out, "got:before", true)
	// Output the old Runner received but never acknowledged is in the
	// snapshot, not replayed after it.
	_ = c.Input([]byte("unacknowledged\n"))
	readTerminal(t, c, &out, "got:unacknowledged", false)
	_ = c.Detach()

	again, err := Attach(deadline(t), dir, c.ID(), 0)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Detach()
	scrollback, produced := again.Scrollback()
	if !strings.Contains(string(scrollback), "got:before") || !strings.Contains(string(scrollback), "got:unacknowledged") || produced != uint64(len(scrollback)) {
		t.Fatalf("scrollback %q, produced %d", scrollback, produced)
	}
	_ = again.Input([]byte("after\n"))
	var live strings.Builder
	readTerminal(t, again, &live, "got:after", true)
	if strings.Contains(live.String(), "before") || strings.Contains(live.String(), "unacknowledged") {
		t.Fatalf("snapshot output was replayed: %q", live.String())
	}
}

func TestTerminalScrollbackIsBounded(t *testing.T) {
	dir := root(t)
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{Scrollback: 64})
	if err != nil {
		t.Fatal(err)
	}
	if err = c.Spawn(ctx, terminalSpec()); err != nil {
		t.Fatal(err)
	}
	var out strings.Builder
	readTerminal(t, c, &out, "ready", true)
	_ = c.Input([]byte(strings.Repeat("x", 200) + "\n"))
	readTerminal(t, c, &out, "got:"+strings.Repeat("x", 200), true)
	// A PTY read can split the last line from its terminator. Include every
	// byte of this reply before comparing the observed and retained tails.
	if !strings.HasSuffix(out.String(), "\n") {
		var ending strings.Builder
		readTerminal(t, c, &ending, "\n", true)
		out.WriteString(ending.String())
	}
	_ = c.Detach()
	again, err := Attach(ctx, dir, c.ID(), 0)
	if err != nil {
		t.Fatal(err)
	}
	defer again.Detach()
	scrollback, produced := again.Scrollback()
	if len(scrollback) != 64 || produced != uint64(out.Len()) || !strings.HasSuffix(out.String(), string(scrollback)) {
		t.Fatalf("scrollback %q (%d bytes) of %d produced", scrollback, len(scrollback), produced)
	}
}

func TestTerminateHangsUpTheTerminal(t *testing.T) {
	dir := root(t)
	c := startTerminal(t, dir)
	var out strings.Builder
	readTerminal(t, c, &out, "ready", true)
	if err := c.Terminate(); err != nil {
		t.Fatal(err)
	}
	// The program does not handle SIGHUP, so the hang-up ends it.
	if outcome := exitOf(t, c); outcome.Signal != syscall.SIGHUP.String() {
		t.Fatalf("expected a hang-up, got %+v", outcome)
	}
}

func TestTerminalRejectsAnInvalidSize(t *testing.T) {
	dir := root(t)
	ctx := deadline(t)
	c, err := Launch(ctx, dir, Options{})
	if err != nil {
		t.Fatal(err)
	}
	spec := terminalSpec()
	spec.Terminal = &Size{}
	if err = c.Spawn(ctx, spec); err == nil || !strings.Contains(err.Error(), "terminal size") {
		t.Fatalf("expected a size error, got %v", err)
	}
}
