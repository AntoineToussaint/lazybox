package runner

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/commands"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/runner/terminals"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

func startTerminal(t *testing.T, b *Bench, cwd string) *terminals.Terminal {
	t.Helper()
	term, err := b.Terminals.Start(terminals.Spec{CollectionID: "workspace-1", Cwd: cwd, Shell: "/bin/sh", Env: []string{"PATH=/usr/bin:/bin", "ENV="}, Cols: 80, Rows: 24}, "")
	if err != nil {
		t.Fatal(err)
	}
	return term
}

func inventory(t *testing.T, b *Bench) map[string]HostedProcess {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	got, err := b.Processes(ctx)
	if err != nil {
		t.Fatal(err)
	}
	byKind := map[string]HostedProcess{}
	for _, p := range got.Processes {
		key := p.Kind
		if !p.Attached {
			key = "orphan"
		}
		if _, dup := byKind[key]; dup {
			t.Fatalf("two %s processes: %+v", key, got.Processes)
		}
		byKind[key] = p
	}
	return byKind
}

// The Processes view: every hosted process, named by what it belongs to, with
// an orphan told apart; the sweep stops only the orphan; Stop goes through
// each owner.
func TestProcessesNamesEveryHostedProcessAndStopsThemSafely(t *testing.T) {
	state := t.TempDir()
	cleanupHosts(t, state)
	b := fixtureBench(t, state)
	defer b.Close()
	e, err := openAwaitingApproval(b)
	if err != nil {
		t.Fatal(err)
	}
	command, err := b.Commands.Start(commands.Spec{SessionID: e.ID, Title: "sleeping", Cwd: state, Argv: []string{"/bin/sleep", "300"}})
	if err != nil {
		t.Fatal(err)
	}
	term := startTerminal(t, b, state)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	orphan, err := sessionhost.Launch(ctx, filepath.Join(state, "command-hosts"), sessionhost.Options{Kind: "command", Label: "left behind"})
	if err != nil {
		t.Fatal(err)
	}
	if err = orphan.Spawn(ctx, sessionhost.Spec{Path: "/bin/sleep", Args: []string{"300"}, Cwd: state, Stream: true}); err != nil {
		t.Fatal(err)
	}
	orphanPID := orphan.AgentPID()
	_ = orphan.Detach()

	got := inventory(t, b)
	if len(got) != 4 {
		t.Fatalf("want an agent, a command, a terminal and an orphan, got %+v", got)
	}
	a := got["agent"]
	if a.ExecutionID != e.ID || a.Provider != "claude" || a.State != "running" || a.ProgramPID == 0 || a.Backend != sessionhost.BackendSessionHost || a.AttachCommand != "" {
		t.Fatalf("agent %+v", a)
	}
	c := got["command"]
	if c.CommandID != command.Status().ID || c.Label != "sleeping" || c.ExecutionID != e.ID || c.Provider != "claude" || c.State != "running" {
		t.Fatalf("command %+v", c)
	}
	tm := got["terminal"]
	if tm.TerminalID != term.Status().ID || tm.WorkspaceID != "workspace-1" || tm.State != "running" {
		t.Fatalf("terminal %+v", tm)
	}
	o := got["orphan"]
	if o.Kind != "command" || o.Label != "left behind" || o.ProgramPID != orphanPID {
		t.Fatalf("orphan %+v", o)
	}
	for kind, p := range got {
		if p.HostPID == 0 || p.Usage == nil || p.Usage.Memory == 0 || p.Usage.Processes < 2 {
			t.Fatalf("%s has no usage sample: %+v", kind, p)
		}
	}

	stopped, err := b.CleanUpOrphanProcesses(ctx)
	if err != nil || stopped != 1 {
		t.Fatalf("cleanup stopped %d: %v", stopped, err)
	}
	stoppedEventually(t, orphanPID)
	for _, p := range []HostedProcess{a, c, tm} {
		if syscall.Kill(p.ProgramPID, 0) != nil {
			t.Fatalf("the sweep stopped a process this Runner holds: %+v", p)
		}
	}

	for _, p := range []HostedProcess{c, tm, a} {
		if err = b.StopProcess(ctx, p.ID); err != nil {
			t.Fatalf("stop %s: %v", p.Kind, err)
		}
		stoppedEventually(t, p.ProgramPID)
	}
	if done, _ := command.Wait(ctx); !done.Done {
		t.Fatal("the command was not cancelled through its manager")
	}
	if !e.Engine.Snapshot().Closed {
		t.Fatal("the agent was stopped without closing its session")
	}
	if err = b.StopProcess(ctx, "no-such-host-id"); err == nil {
		t.Fatal("stopping an unknown process succeeded")
	}
}

func TestTmuxSelectedButUnavailableStillStartsAndSaysWhy(t *testing.T) {
	state := t.TempDir()
	b, err := New(Config{State: state, Processes: ProcessHosting{Backend: sessionhost.BackendTmux, Tmux: filepath.Join(t.TempDir(), "no-tmux")}})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	startTerminal(t, b, state)
	got, err := b.Processes(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Processes) != 1 || got.Processes[0].Backend != sessionhost.BackendSessionHost {
		t.Fatalf("the terminal did not start in a session host: %+v", got.Processes)
	}
	h := got.Hosting
	if h.Selected != sessionhost.BackendTmux || h.Configured != sessionhost.BackendTmux || !strings.Contains(h.TmuxProblem, "tmux not found") || !strings.Contains(h.Fallback, "runs in a session host") {
		t.Fatalf("hosting state does not explain the fallback: %+v", h)
	}
}

func TestUnknownProcessBackendIsRefused(t *testing.T) {
	if _, err := New(Config{State: t.TempDir(), Processes: ProcessHosting{Backend: "screen"}}); err == nil || !strings.Contains(err.Error(), "unknown process backend") {
		t.Fatalf("expected a configuration error, got %v", err)
	}
}

// Choosing tmux in Settings applies to the next process, without a restart;
// running processes keep the backend they started with.
func TestBackendChosenInSettingsHostsTheNextProcess(t *testing.T) {
	if _, err := (sessionhost.Tmux{}).Version(context.Background()); err != nil {
		t.Skipf("tmux unavailable: %v", err)
	}
	state := t.TempDir()
	b, err := New(Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	server := sessionhost.ServerName(b.Config.TerminalHostRoot)
	t.Cleanup(func() {
		_ = exec.Command("tmux", "-L", server, "kill-server").Run()
		tmp := os.Getenv("TMUX_TMPDIR")
		if tmp == "" {
			tmp = "/tmp"
		}
		_ = os.Remove(filepath.Join(tmp, fmt.Sprintf("tmux-%d", os.Getuid()), server))
	})
	defer b.Close()
	before := startTerminal(t, b, state)
	settings := b.Preferences.Snapshot()
	settings.ProcessBackend = sessionhost.BackendTmux
	if err = b.Preferences.Save(settings); err != nil {
		t.Fatal(err)
	}
	after := startTerminal(t, b, state)
	got, err := b.Processes(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	backends := map[string]HostedProcess{}
	for _, p := range got.Processes {
		backends[p.TerminalID] = p
	}
	if p := backends[before.Status().ID]; p.Backend != sessionhost.BackendSessionHost || p.AttachCommand != "" {
		t.Fatalf("a running process changed backend: %+v", p)
	}
	p := backends[after.Status().ID]
	if p.Backend != sessionhost.BackendTmux || !strings.HasPrefix(p.AttachCommand, "tmux -S ") || got.Hosting.Selected != sessionhost.BackendTmux || got.Hosting.TmuxVersion == "" {
		t.Fatalf("the next process did not use tmux: %+v %+v", p, got.Hosting)
	}
	// Both are re-attached by a Runner configured either way.
	if err = b.Detach(); err != nil {
		t.Fatal(err)
	}
	again, err := New(Config{State: state})
	if err != nil {
		t.Fatal(err)
	}
	defer again.Close()
	if n := len(again.Terminals.List("workspace-1")); n != 2 {
		t.Fatalf("re-attached %d terminals, want 2", n)
	}
}
