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

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
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

// A finished command's host exits on its own a moment after it is released.
// It must not be listed in between: an inventory would show it, stopping it
// would "succeed" and an orphan sweep would count it as stopped.
func TestAFinishedCommandsHostIsNotListed(t *testing.T) {
	f := newHosted(t)
	m := f.open(t)
	defer m.Close()
	for i := 0; i < 5; i++ {
		c, err := m.Start(Spec{SessionID: "s", Title: "quick", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", "echo done"}, Env: os.Environ()})
		if err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		state, err := c.Wait(ctx)
		cancel()
		if err != nil || state.ExitCode == nil || *state.ExitCode != 0 {
			t.Fatalf("command did not finish: %+v %v", state, err)
		}
		if ids, _ := sessionhost.Hosts(f.hosts); len(ids) != 0 {
			t.Fatalf("run %d: a finished command's host is still listed: %v", i, ids)
		}
	}
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

// When another Runner adopts a command, this Runner stops tracking it: waiting
// on it returns at once, it frees its capacity, and closing this Runner does
// not stop what the other one now owns.
func TestCommandAdoptedElsewhereEndsHereWithoutStopping(t *testing.T) {
	f := newHosted(t)
	first := f.open(t)
	c, err := first.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", f.script()}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	id := c.Status().ID
	waitFor(t, "first output never recorded", func() bool { return c.Status().Offset > 0 })
	second := f.open(t) // Adopts the command, superseding the first Runner.
	defer second.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	state, err := c.Wait(ctx)
	if err != nil || !strings.Contains(state.Error, "another Runner") {
		t.Fatalf("the superseded Runner kept waiting on a command it no longer owns: %+v %v", state, err)
	}
	if err = first.Close(); err != nil {
		t.Fatalf("closing the superseded Runner: %v", err)
	}
	if err = os.WriteFile(f.gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	adopted, err := second.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	final := finished(t, adopted)
	if final.ExitCode == nil || *final.ExitCode != 4 || final.Error != "" {
		t.Fatalf("the superseded Runner stopped a command it no longer owns: %+v", final)
	}
}

// A command whose output fills its journal is stopped, and how it ended is
// still recorded: the completion has room kept for it. Before, the completion
// did not fit, yet the host was released, so the next Runner found neither the
// outcome nor the host and called it unknown.
func TestACommandThatFillsItsJournalKeepsItsCompletion(t *testing.T) {
	f := newHosted(t)
	const budget = 8 << 10
	m, err := NewHosted(f.dir, budget, 0, f.hosts)
	if err != nil {
		t.Fatal(err)
	}
	c, err := m.Start(Spec{SessionID: "s", Title: "flood", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", "while :; do echo x; sleep 0.01; done"}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	state := finished(t, c)
	if !strings.Contains(state.Error, "capture failed") || state.Signal == "" {
		t.Fatalf("a command that filled its journal: %+v", state)
	}
	waitFor(t, "the finished command's host stayed", func() bool { ids, _ := sessionhost.Hosts(f.hosts); return len(ids) == 0 })
	if err = m.Close(); err != nil {
		t.Fatal(err)
	}
	next, err := NewHosted(f.dir, budget, 0, f.hosts)
	if err != nil {
		t.Fatal(err)
	}
	defer next.Close()
	reopened, err := next.Get(state.ID)
	if err != nil {
		t.Fatal(err)
	}
	if got := reopened.Status(); !got.Done || got.Error != state.Error || got.Signal != state.Signal {
		t.Fatalf("the completion was lost: recorded %+v, was %+v", got, state)
	}
}

// A host is released only once its command's completion is recorded. One
// whose completion could not be written keeps the exit, and the next Runner
// records it.
func TestAnUnrecordedCompletionLeavesTheExitWithTheHost(t *testing.T) {
	f := newHosted(t)
	m := f.open(t)
	quiet := `printf before; while [ ! -e "` + f.gate + `" ]; do sleep 0.02; done; exit 4`
	c, err := m.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", quiet}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "first output never recorded", func() bool { return c.Status().Offset > 0 })
	// The journal takes nothing more (a failing disk, say).
	_ = c.spool.Close()
	if err = os.WriteFile(f.gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	if state := finished(t, c); !strings.Contains(state.Error, "capture") {
		t.Fatalf("an unrecorded completion: %+v", state)
	}
	if ids, _ := sessionhost.Hosts(f.hosts); len(ids) != 1 {
		t.Fatal("the host was released although the exit was not recorded")
	}
	_ = m.Close()
	next := f.open(t)
	defer next.Close()
	adopted, err := next.Get(c.Status().ID)
	if err != nil {
		t.Fatal(err)
	}
	if state := finished(t, adopted); state.ExitCode == nil || *state.ExitCode != 4 {
		t.Fatalf("the next Runner did not collect the exit: %+v", state)
	}
}

// A connection to a live host that drops (here closed under the Runner) is
// not the command's end: the Runner re-attaches and records the rest of its
// output and its own exit. Before, the command was marked done while it ran,
// and the next Runner's sweep stopped it as an orphan.
func TestADroppedHostConnectionIsReattached(t *testing.T) {
	f := newHosted(t)
	m := f.open(t)
	defer m.Close()
	c, err := m.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", f.script()}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "first output never recorded", func() bool { return c.Status().Offset > 0 })
	c.mu.Lock()
	lost := c.host
	c.mu.Unlock()
	_ = lost.Detach() // The connection ends; this Runner did not detach.
	waitFor(t, "the command was not re-attached", func() bool { c.mu.Lock(); defer c.mu.Unlock(); return c.host != lost })
	if state := c.Status(); state.Done {
		t.Fatalf("a running command was marked done when its connection dropped: %+v", state)
	}
	if err = os.WriteFile(f.gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	state := finished(t, c)
	if state.ExitCode == nil || *state.ExitCode != 4 || state.Error != "" {
		t.Fatalf("the command's own exit was not recorded: %+v", state)
	}
	if got := output(t, c); got != "beforeafterwarn" && got != "beforewarnafter" {
		t.Fatalf("output across the dropped connection is %q: nothing may be lost or repeated", got)
	}
}

// staleAttached is a backend whose hosts all report a client attached, as a
// host does for a connection that died until it reads its close.
type staleAttached struct{ sessionhost.Local }

func (b staleAttached) Probe(ctx context.Context, root, id string) (sessionhost.Status, error) {
	status, err := b.Local.Probe(ctx, root, id)
	status.Attached = true
	return status, err
}

// A host that still counts the dropped connection as attached is no reason
// to stand down: re-attaching supersedes the stale client. Before, the
// command was left shown done, supervised by no one, and Cancel could not
// reach it.
func TestADroppedConnectionTheHostStillCountsIsReattached(t *testing.T) {
	f := newHosted(t)
	m, err := NewHostedOn(f.dir, 1<<20, 0, f.hosts, staleAttached{})
	if err != nil {
		t.Fatal(err)
	}
	defer m.Close()
	c, err := m.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", f.script()}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "first output never recorded", func() bool { return c.Status().Offset > 0 })
	c.mu.Lock()
	lost := c.host
	c.mu.Unlock()
	_ = lost.Detach()
	waitFor(t, "the command was not re-attached", func() bool {
		c.mu.Lock()
		defer c.mu.Unlock()
		return c.host != lost || c.status.Done
	})
	if state := c.Status(); state.Done {
		t.Fatalf("a running command was given up: %+v", state)
	}
	if err = os.WriteFile(f.gate, nil, 0600); err != nil {
		t.Fatal(err)
	}
	if state := finished(t, c); state.ExitCode == nil || *state.ExitCode != 4 || state.Error != "" {
		t.Fatalf("the command's own exit was not recorded: %+v", state)
	}
}

// A host that dies while attached took its command with it (processguard):
// its outcome is unknown, and nothing is left running to confirm.
func TestACommandWhoseHostDiesWhileAttachedIsReportedUnknown(t *testing.T) {
	f := newHosted(t)
	m := f.open(t)
	defer m.Close()
	c, err := m.Start(Spec{SessionID: "s", Title: "build", Cwd: t.TempDir(), Argv: []string{"/bin/sh", "-c", f.script()}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "no output", func() bool { return c.Status().Offset > 0 })
	record, err := sessionhost.ReadRecord(filepath.Join(f.hosts, c.hostID))
	if err != nil {
		t.Fatal(err)
	}
	_ = syscall.Kill(record.PID, syscall.SIGKILL)
	state := finished(t, c)
	if state.ExitCode != nil || !strings.Contains(state.Error, "outcome unknown") {
		t.Fatalf("dead host: %+v", state)
	}
}

func TestUpgradeStopsUnisolatedVerification(t *testing.T) {
	f := newHosted(t)
	m := f.open(t)
	// Seed exactly the old launch path, which did not apply isolation.
	c, err := m.startHosted(Spec{SessionID: "reviewer", Kind: "verification", Action: "review/1/check/base", Cwd: t.TempDir(), Argv: []string{"sh", "-c", "echo ready; sleep 30"}, Env: os.Environ()})
	if err != nil {
		t.Fatal(err)
	}
	waitFor(t, "legacy command started", func() bool { return c.Status().Offset > 0 })
	id := c.Status().ID
	if err = m.Detach(); err != nil {
		t.Fatal(err)
	}
	upgraded := f.open(t)
	defer upgraded.Close()
	adopted, err := upgraded.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	state, err := adopted.Wait(ctx)
	if err != nil || !state.Done || (state.ExitCode != nil && *state.ExitCode == 0) {
		t.Fatalf("unisolated command survived upgrade: %+v %v", state, err)
	}
}
