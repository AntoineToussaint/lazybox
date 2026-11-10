package commands

import (
	"bufio"
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/v2/internal/sessionhost"
)

// The test binary doubles as a Runner that starts a hosted command, reports
// its ID and then waits to be killed.
func TestMain(m *testing.M) {
	if os.Getenv("COMMANDS_FIXTURE") == "runner" {
		fixtureRunner()
		return
	}
	os.Exit(m.Run())
}

func fixtureRunner() {
	m, err := NewHosted(os.Getenv("FIXTURE_DIR"), 1<<20, 0, os.Getenv("FIXTURE_HOSTS"))
	if err != nil {
		os.Exit(2)
	}
	c, err := m.Start(Spec{SessionID: "fixture", Title: "build", Cwd: os.TempDir(), Argv: []string{"/bin/sh", "-c", os.Getenv("FIXTURE_SCRIPT")}, Env: os.Environ()})
	if err != nil {
		os.Exit(3)
	}
	for c.Status().Offset == 0 {
		time.Sleep(10 * time.Millisecond)
	}
	os.Stdout.WriteString(c.Status().ID + "\n")
	select {}
}

type hostedFixture struct {
	dir, hosts, gate string
}

func newHosted(t *testing.T) hostedFixture {
	t.Helper()
	hosts, err := os.MkdirTemp("", "lbx-cmd-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		ids, _ := sessionhost.Hosts(hosts)
		for _, id := range ids {
			_ = sessionhost.Stop(context.Background(), hosts, id)
		}
		_ = os.RemoveAll(hosts)
	})
	return hostedFixture{dir: t.TempDir(), hosts: hosts, gate: filepath.Join(t.TempDir(), "gate")}
}

func (f hostedFixture) open(t *testing.T) *Manager {
	t.Helper()
	m, err := NewHosted(f.dir, 1<<20, 0, f.hosts)
	if err != nil {
		t.Fatal(err)
	}
	return m
}

// script prints "before", waits for the gate file, then prints "after" on both
// streams and exits 4: the second half happens while no Runner is attached.
func (f hostedFixture) script() string {
	return `printf before; while [ ! -e "` + f.gate + `" ]; do sleep 0.02; done; printf after; printf warn >&2; exit 4`
}

func waitFor(t *testing.T, what string, check func() bool) {
	t.Helper()
	for end := time.Now().Add(8 * time.Second); time.Now().Before(end); time.Sleep(10 * time.Millisecond) {
		if check() {
			return
		}
	}
	t.Fatal(what)
}

func finished(t *testing.T, c *Command) Status {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	state, err := c.Wait(ctx)
	if err != nil {
		t.Fatalf("command never finished: %v (%+v)", err, state)
	}
	return state
}

func output(t *testing.T, c *Command) string {
	t.Helper()
	data, _, err := c.Read(context.Background(), 0, 0)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

func TestHostedCommandOutlivesARunnerThatDetaches(t *testing.T) {
	f := newHosted(t)
	first := f.open(t)
	c, err := first.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", f.script()}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	id := c.Status().ID
	waitFor(t, "first output never recorded", func() bool { return c.Status().Offset > 0 })
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	if c.Status().Done {
		t.Fatal("detaching finished the command")
	}
	if err = os.WriteFile(f.gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	second := f.open(t)
	defer second.Close()
	adopted, err := second.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	state := finished(t, adopted)
	if state.ExitCode == nil || *state.ExitCode != 4 || state.Error != "" {
		t.Fatalf("adopted command did not report its own exit: %+v", state)
	}
	got := output(t, adopted)
	if got != "beforeafterwarn" && got != "beforewarnafter" {
		t.Fatalf("output across the restart is %q: nothing may be lost or repeated", got)
	}
}

// A Runner whose registry fails to open (here an unreadable journal) leaves
// the commands it had already re-attached running for the next Runner.
func TestFailedOpenLeavesAdoptedCommandsRunning(t *testing.T) {
	f := newHosted(t)
	first := f.open(t)
	c, err := first.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", f.script()}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	id := c.Status().ID
	waitFor(t, "first output never recorded", func() bool { return c.Status().Offset > 0 })
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}
	// Sorted after the command's journal, so that command is adopted first.
	corrupt := filepath.Join(f.dir, "~corrupt.jsonl")
	if err = os.WriteFile(corrupt, []byte("not a journal\n"), 0600); err != nil {
		t.Fatal(err)
	}
	if _, err = NewHosted(f.dir, 1<<20, 0, f.hosts); err == nil {
		t.Fatal("an unreadable journal was accepted")
	}
	if err = os.Remove(corrupt); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(f.gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	next := f.open(t)
	defer next.Close()
	adopted, err := next.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	state := finished(t, adopted)
	if state.ExitCode == nil || *state.ExitCode != 4 || state.Error != "" {
		t.Fatalf("the failed open stopped an adopted command: %+v", state)
	}
}

// A Runner killed outright (SIGKILL, its whole process group) never detached.
// Its command keeps running and the next Runner still adopts it.
func TestHostedCommandOutlivesARunnerKilledWithItsGroup(t *testing.T) {
	f := newHosted(t)
	self, _ := os.Executable()
	runner := exec.Command(self)
	runner.Env = append(os.Environ(), "COMMANDS_FIXTURE=runner", "FIXTURE_DIR="+f.dir, "FIXTURE_HOSTS="+f.hosts, "FIXTURE_SCRIPT="+f.script())
	runner.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	stdout, _ := runner.StdoutPipe()
	if err := runner.Start(); err != nil {
		t.Fatal(err)
	}
	line, err := bufio.NewReader(stdout).ReadString('\n')
	if err != nil {
		_ = syscall.Kill(-runner.Process.Pid, syscall.SIGKILL)
		t.Fatal("fixture Runner did not start its command:", err)
	}
	id := strings.TrimSpace(line)
	_ = syscall.Kill(-runner.Process.Pid, syscall.SIGKILL)
	_ = runner.Wait()

	if err = os.WriteFile(f.gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	next := f.open(t)
	defer next.Close()
	adopted, err := next.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	state := finished(t, adopted)
	if state.ExitCode == nil || *state.ExitCode != 4 {
		t.Fatalf("command died with its Runner: %+v", state)
	}
	if got := output(t, adopted); !strings.HasPrefix(got, "before") || !strings.Contains(got, "after") || strings.Count(got, "before") != 1 {
		t.Fatalf("output across the crash: %q", got)
	}
}

func TestClosingTheRunnerStillStopsHostedCommands(t *testing.T) {
	f := newHosted(t)
	m := f.open(t)
	c, err := m.Start(Spec{SessionID: "s", Title: "serve", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", "echo $$; exec /bin/sleep 300"}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "no output", func() bool { return c.Status().Offset > 0 })
	if err = m.Close(); err != nil {
		t.Fatal(err)
	}
	state := c.Status()
	if !state.Done || state.CleanupUnconfirmed() {
		t.Fatalf("closing did not stop the command with evidence: %+v", state)
	}
	ids, _ := sessionhost.Hosts(f.hosts)
	waitFor(t, "the stopped command's host stayed", func() bool { ids, _ = sessionhost.Hosts(f.hosts); return len(ids) == 0 })
}

// A host that died while no Runner was attached took its command with it. The
// next Runner says so instead of waiting forever or claiming an exit status.
func TestCommandWhoseHostDiedIsReportedUnknown(t *testing.T) {
	f := newHosted(t)
	first := f.open(t)
	c, err := first.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", f.script()}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "no output", func() bool { return c.Status().Offset > 0 })
	_ = first.Detach()
	record, err := sessionhost.ReadRecord(filepath.Join(f.hosts, c.hostID))
	if err != nil {
		t.Fatal(err)
	}
	_ = syscall.Kill(record.PID, syscall.SIGKILL)
	time.Sleep(100 * time.Millisecond)

	second := f.open(t)
	defer second.Close()
	adopted, _ := second.Get(c.Status().ID)
	state := finished(t, adopted)
	if state.ExitCode != nil || !strings.Contains(state.Error, "outcome unknown") || state.CleanupUnconfirmed() {
		t.Fatalf("dead host: %+v", state)
	}
	if _, err = os.Stat(filepath.Join(f.hosts, c.hostID)); !os.IsNotExist(err) {
		t.Fatal("the dead host's directory was kept")
	}
}

// Cancelling an adopted command reaches the program the previous Runner started.
func TestAdoptedCommandCanBeCancelled(t *testing.T) {
	f := newHosted(t)
	first := f.open(t)
	c, err := first.Start(Spec{SessionID: "s", Title: "serve", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", "echo up; exec /bin/sleep 300"}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "no output", func() bool { return c.Status().Offset > 0 })
	_ = first.Detach()
	second := f.open(t)
	defer second.Close()
	adopted, _ := second.Get(c.Status().ID)
	if err = adopted.Cancel(); err != nil {
		t.Fatal(err)
	}
	if state := finished(t, adopted); state.Signal == "" || state.CleanupUnconfirmed() {
		t.Fatalf("cancelled adopted command: %+v", state)
	}
}

// Closing the registry while a hosted command is between registration and
// spawn still stops that command: shutdown waits for the start to finish.
func TestCloseDuringAHostedStartStopsTheCommand(t *testing.T) {
	f := newHosted(t)
	m := f.open(t)
	closed := make(chan error, 1)
	beforeSpawn = func() {
		go func() { closed <- m.Close() }()
		waitFor(t, "shutdown never began", func() bool { m.mu.Lock(); defer m.mu.Unlock(); return m.closed })
	}
	t.Cleanup(func() { beforeSpawn = func() {} })
	c, err := m.Start(Spec{SessionID: "s", Title: "serve", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", "echo $$; exec /bin/sleep 300"}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	var pid int
	waitFor(t, "command never reported its pid", func() bool {
		data, _, _ := c.Read(context.Background(), 0, 0)
		pid, _ = strconv.Atoi(strings.TrimSpace(string(data)))
		return pid > 0 || c.Status().Done
	})
	if err = <-closed; err != nil {
		t.Fatalf("closing during a start: %v", err)
	}
	if !c.Status().Done {
		t.Fatal("a command started during shutdown was left running")
	}
	if pid > 0 && syscall.Kill(pid, 0) == nil {
		t.Fatalf("command %d still running after Close", pid)
	}
}
