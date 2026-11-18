package terminals

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agentshell"
)

// zshSpec runs a real zsh login shell with Lazybox's shell integration and a
// private HOME whose .zshrc takes a second, like a heavy prompt theme.
func zshSpec(t *testing.T, dir string) Spec {
	t.Helper()
	zsh := "/bin/zsh"
	if _, err := os.Stat(zsh); err != nil {
		t.Skip("zsh is not installed")
	}
	spec := shellSpec(t, dir)
	home := filepath.Join(dir, "home")
	if err := os.WriteFile(filepath.Join(home, ".zshrc"), []byte("sleep 1\nPROMPT='zsh> '\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	integration, cli := filepath.Join(dir, "integration"), filepath.Join(dir, "cli")
	if err := os.MkdirAll(cli, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := agentshell.Install(integration); err != nil {
		t.Fatal(err)
	}
	spec.Shell = zsh
	spec.Env = agentshell.Env(Environment([]string{"PATH=/usr/bin:/bin", "HOME=" + home}, "workspace-1"), integration, cli)
	return spec
}

// waitFor polls a terminal's status until ok, or fails after ten seconds.
func waitFor(t *testing.T, term *Terminal, what string, ok func(Status) bool) Status {
	t.Helper()
	for end := time.Now().Add(10 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
		if s := term.Status(); ok(s) {
			return s
		}
	}
	t.Fatalf("%s: %+v", what, term.Status())
	return Status{}
}

func output(t *testing.T, term *Terminal) string {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	var all strings.Builder
	for after := uint64(0); after < term.Status().Offset; {
		data, from, _, err := term.Next(ctx, after)
		if err != nil {
			t.Fatal(err)
		}
		all.Write(data)
		after = from + uint64(len(data))
	}
	return all.String()
}

// The bug this mark fixes, and the fix, on a real zsh that starts slowly:
// input written while the shell is still starting is echoed by the terminal
// and then shown again at the prompt; input written once the shell says its
// prompt is ready (OSC 133;B) shows once, and runs. After it the shell is
// idle again in its folder; a running command or another folder is not.
func TestZshSaysWhenItsPromptIsReady(t *testing.T) {
	dir := private(t)
	m := manager(t, dir)
	defer m.Close()
	spec := zshSpec(t, dir)
	command := "echo ran-$((6*7))"

	early, err := m.Start(spec, "")
	if err != nil {
		t.Fatal(err)
	}
	if err = early.Write([]byte(command)); err != nil {
		t.Fatal(err)
	}
	waitFor(t, early, "the early shell's prompt", func(s Status) bool { return s.PromptsReady > 0 })
	time.Sleep(200 * time.Millisecond)
	if n := strings.Count(output(t, early), command); n != 2 {
		t.Fatalf("typed before the prompt, the command shows %d times, want 2 (echoed, then at the prompt)", n)
	}

	term, err := m.Start(spec, "")
	if err != nil {
		t.Fatal(err)
	}
	if !term.Status().Spec.PromptMarks {
		t.Fatal("zsh with the integration says when its prompt is ready")
	}
	started := time.Now()
	ready := waitFor(t, term, "the prompt", func(s Status) bool { return s.PromptsReady > 0 })
	if waited := time.Since(started); waited < 800*time.Millisecond {
		t.Fatalf("ready after %v, before the .zshrc finished", waited)
	}
	if !ready.Idle() || ready.ShellCwd == "" {
		t.Fatalf("at its first prompt the shell is idle in its folder: %+v", ready)
	}
	if err = term.Write([]byte(command + "\r")); err != nil {
		t.Fatal(err)
	}
	done := waitFor(t, term, "the command's end", func(s Status) bool { return s.CommandEnds > ready.CommandEnds && s.PromptsReady > ready.PromptsReady })
	all := output(t, term)
	if n := strings.Count(all, command); n != 1 || !strings.Contains(all, "ran-42") {
		t.Fatalf("typed at the prompt, the command shows %d times (want 1) and runs: %q", n, all)
	}
	if done.LastCommandStatus != 0 || !done.Idle() {
		t.Fatalf("after the command the shell is idle again: %+v", done)
	}

	if err = term.Write([]byte("sleep 2\r")); err != nil {
		t.Fatal(err)
	}
	waitFor(t, term, "busy while a command runs", func(s Status) bool { return !s.AtPrompt && !s.Idle() })
	waitFor(t, term, "idle once it ends", func(s Status) bool { return s.Idle() })
	if err = term.Write([]byte("cd /\r")); err != nil {
		t.Fatal(err)
	}
	elsewhere := waitFor(t, term, "at a prompt in another folder", func(s Status) bool { return s.AtPrompt && s.ShellCwd == "/" })
	if elsewhere.InStartFolder || elsewhere.Idle() {
		t.Fatalf("a shell in another folder is not idle for its own: %+v", elsewhere)
	}
}

// A shell without the marks (sh here; bash and fish alike) never claims a
// ready prompt: a window falls back to waiting for its output to settle.
func TestAShellWithoutMarksIsNeverIdle(t *testing.T) {
	dir := private(t)
	m := manager(t, dir)
	defer m.Close()
	term, err := m.Start(shellSpec(t, dir), "")
	if err != nil {
		t.Fatal(err)
	}
	until(t, term, 0, "$ ")
	if s := term.Status(); s.Spec.PromptMarks || s.PromptsReady != 0 || s.Idle() {
		t.Fatalf("sh says nothing about its prompt: %+v", s)
	}
}
