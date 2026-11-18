package installation

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"
)

// The test binary also plays the Runner binary: `supervise` is the
// supervisor, and `fake-runner <file>` a Runner that notes its PID and
// environment and runs until SIGTERM.
func TestMain(m *testing.M) {
	if len(os.Args) == 4 && os.Args[1] == SuperviseArgument {
		if err := Supervise(os.Args[2], os.Args[3]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		os.Exit(0)
	}
	if len(os.Args) == 3 && os.Args[1] == "fake-runner" {
		terms := make(chan os.Signal, 1)
		signal.Notify(terms, syscall.SIGTERM)
		note := strconv.Itoa(os.Getpid()) + " FAKE=" + os.Getenv("FAKE") + " TOKEN=" + os.Getenv("LAZYBOX_V2_AGENT_TOKEN") + "\n"
		f, _ := os.OpenFile(os.Args[2], os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
		_, _ = f.WriteString(note)
		_ = f.Close()
		<-terms
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func runnersSeen(t *testing.T, path string) []string {
	t.Helper()
	data, _ := os.ReadFile(path)
	return strings.Split(strings.TrimSpace(string(data)), "\n")
}

func waitFor(t *testing.T, what string, ok func() bool) {
	t.Helper()
	for end := time.Now().Add(15 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
		if ok() {
			return
		}
	}
	t.Fatalf("timed out waiting for %s", what)
}

// Without systemd (a container), Lazybox's own supervisor keeps the Runner
// running: it restarts it on request, stops it on Deactivate, and survives
// the process that started it.
func TestSupervisorKeepsTheRunnerRunning(t *testing.T) {
	home := t.TempDir()
	paths, err := PathsFor("linux", env(map[string]string{"HOME": home}))
	if err != nil {
		t.Fatal(err)
	}
	self, _ := os.Executable()
	seen := filepath.Join(t.TempDir(), "runners")
	m := Supervisor{}
	s := Service{Label: DefaultLabel, Program: self, Arguments: []string{"fake-runner", seen}, Environment: map[string]string{"FAKE": "1"}, WorkingDirectory: home, Version: "0.1.0"}
	data, err := m.Render(s)
	if err != nil {
		t.Fatal(err)
	}
	for _, dir := range []string{paths.Support, paths.Logs} {
		_ = os.MkdirAll(dir, 0o700)
	}
	if err = os.WriteFile(m.DefinitionPath(paths), data, 0o600); err != nil {
		t.Fatal(err)
	}
	if installed, err := m.ReadDefinition(context.Background(), m.DefinitionPath(paths)); err != nil || installed.Program != self || installed.Version != "0.1.0" {
		t.Fatalf("read back %+v %v", installed, err)
	}
	// An agent's credential in the environment it is started from never reaches the Runner.
	t.Setenv("LAZYBOX_V2_AGENT_TOKEN", "agent-secret")
	ctx := context.Background()
	t.Cleanup(func() { _ = m.Remove(ctx, paths) })

	if job, err := m.Status(ctx, paths); err != nil || job.Loaded {
		t.Fatalf("before: %+v %v", job, err)
	}
	if err = m.Activate(ctx, paths); err != nil {
		t.Fatal(err)
	}
	var job Job
	waitFor(t, "the Runner", func() bool { job, _ = m.Status(ctx, paths); return job.Running })
	if !job.Loaded || job.Detail != NotAtBoot {
		t.Fatalf("%+v", job)
	}
	first := job.PID
	if got := runnersSeen(t, seen); len(got) != 1 || got[0] != strconv.Itoa(first)+" FAKE=1 TOKEN=" {
		t.Fatalf("the Runner ran with %q", got)
	}
	// Activating again is a no-op: one supervisor.
	if err = m.Activate(ctx, paths); err != nil {
		t.Fatal(err)
	}
	if err = m.Restart(ctx, paths); err != nil {
		t.Fatal(err)
	}
	waitFor(t, "a new Runner", func() bool { job, _ = m.Status(ctx, paths); return job.Running && job.PID != first })
	if syscall.Kill(first, 0) == nil {
		t.Fatal("the restart left the first Runner running")
	}
	// The new Runner notes itself once it runs, a moment after the supervisor records it.
	waitFor(t, "the new Runner to start", func() bool { return len(runnersSeen(t, seen)) == 2 })
	second := job.PID
	if err = m.Deactivate(ctx, paths); err != nil {
		t.Fatal(err)
	}
	if job, _ = m.Status(ctx, paths); job.Loaded || job.Running {
		t.Fatalf("after stop: %+v", job)
	}
	waitFor(t, "the Runner to stop", func() bool { return syscall.Kill(second, 0) != nil })
	if log, _ := os.ReadFile(paths.Log()); strings.Contains(string(log), "panic") {
		t.Fatalf("supervisor log:\n%s", log)
	}
}

// A record naming a process that is not the supervisor it recorded (the PID
// was reused) is never signalled.
func TestASupervisorRecordWithAReusedPIDIsNotSignalled(t *testing.T) {
	paths, _ := PathsFor("linux", env(map[string]string{"HOME": t.TempDir()}))
	_ = os.MkdirAll(paths.Support, 0o700)
	data, _ := json.Marshal(supervision{PID: os.Getpid(), Start: "not-this-process", Runner: os.Getpid(), RunnerStart: "nor-this"})
	if err := os.WriteFile(statusPath(paths), data, 0o600); err != nil {
		t.Fatal(err)
	}
	m := Supervisor{}
	if job, _ := m.Status(context.Background(), paths); job.Loaded || job.Running {
		t.Fatalf("a stale record counts as running: %+v", job)
	}
	// Were it signalled, this test process would get SIGTERM.
	if err := m.Deactivate(context.Background(), paths); err != nil {
		t.Fatal(err)
	}
	if start, err := processStart(os.Getpid()); err != nil || start == "" {
		t.Fatalf("this process's start time: %q %v", start, err)
	}
}
