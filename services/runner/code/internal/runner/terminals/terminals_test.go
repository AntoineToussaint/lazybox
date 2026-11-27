package terminals

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

func private(t *testing.T) string {
	t.Helper()
	// Short and private, like the Runner's state; t.TempDir paths can be long.
	dir, err := os.MkdirTemp("", "lbx-term-")
	if err != nil {
		t.Fatal(err)
	}
	dir, _ = filepath.EvalSymlinks(dir)
	t.Cleanup(func() {
		ids, _ := sessionhost.Hosts(filepath.Join(dir, "hosts"))
		for _, id := range ids {
			_ = sessionhost.Stop(context.Background(), filepath.Join(dir, "hosts"), id)
		}
		_ = os.RemoveAll(dir)
	})
	return dir
}

func manager(t *testing.T, dir string) *Manager {
	t.Helper()
	m, err := Open(filepath.Join(dir, "terminals"), filepath.Join(dir, "hosts"), Options{})
	if err != nil {
		t.Fatal(err)
	}
	return m
}

// shellSpec runs /bin/sh with a private HOME, so no personal profile runs.
func shellSpec(t *testing.T, dir string) Spec {
	t.Helper()
	home := filepath.Join(dir, "home")
	workspace := filepath.Join(dir, "workspace")
	for _, path := range []string{home, workspace} {
		if err := os.MkdirAll(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	env := Environment([]string{"PATH=/usr/bin:/bin", "HOME=" + home, "PS1=$ ", "ENV="}, "workspace-1")
	return Spec{CollectionID: "workspace-1", Cwd: workspace, Title: "Workspace", Shell: "/bin/sh", Env: env, Cols: 80, Rows: 24}
}

// until reads a terminal's output from after until it contains want.
func until(t *testing.T, term *Terminal, after uint64, want string) (string, uint64) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	var seen strings.Builder
	for !strings.Contains(seen.String(), want) {
		data, offset, _, err := term.Next(ctx, after)
		if err != nil {
			t.Fatalf("waiting for %q: %v; output %q", want, err, seen.String())
		}
		seen.Write(data)
		after = offset + uint64(len(data))
	}
	return seen.String(), after
}

func TestTerminalShellLifecycle(t *testing.T) {
	dir := private(t)
	m := manager(t, dir)
	defer m.Close()
	spec := shellSpec(t, dir)
	term, err := m.Start(spec, "open-1")
	if err != nil {
		t.Fatal(err)
	}
	if again, err := m.Start(spec, "open-1"); err != nil || again != term {
		t.Fatalf("a retried request must return the same terminal: %v", err)
	}
	if _, err = os.Stat(filepath.Join(dir, "terminals", term.Status().ID+".json")); err != nil {
		t.Fatal("a running terminal must be recorded for the next Runner:", err)
	}
	// Input reaches the shell, which runs in the workspace with the CLI's variables.
	if err = term.Write([]byte("echo mark-$((40+2)) \"$PWD\" \"$LAZYBOX_V2_COLLECTION_ID\" \"$TERM\"\n")); err != nil {
		t.Fatal(err)
	}
	_, after := until(t, term, 0, "mark-42 "+spec.Cwd+" workspace-1 xterm-256color")
	if err = term.Resize(100, 40); err != nil {
		t.Fatal(err)
	}
	if err = term.Write([]byte("stty size\n")); err != nil {
		t.Fatal(err)
	}
	_, _ = until(t, term, after, "40 100")
	if status := term.Status(); status.Spec.Cols != 100 || status.Spec.Rows != 40 {
		t.Fatalf("resize not reflected: %+v", status.Spec)
	}
	if err = term.Write([]byte("exit 7\n")); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	status, err := term.Wait(ctx)
	if err != nil || status.ExitCode == nil || *status.ExitCode != 7 || status.Error != "" {
		t.Fatalf("unexpected exit %+v %v", status, err)
	}
	if _, _, _, err = term.Next(ctx, status.Offset); err != io.EOF {
		t.Fatalf("an exited terminal ends its output, got %v", err)
	}
	if err = term.Write([]byte("echo\n")); err == nil {
		t.Fatal("writing to an exited terminal must fail")
	}
	if _, err = os.Stat(filepath.Join(dir, "terminals", status.ID+".json")); !os.IsNotExist(err) {
		t.Fatal("an exited terminal's record must be removed")
	}
	// Its host exits on its own a moment later; it must not be listed in
	// between, or an inventory shows it and an orphan sweep stops it.
	if hosts, _ := sessionhost.Hosts(filepath.Join(dir, "hosts")); len(hosts) != 0 {
		t.Fatalf("an exited terminal's host is still listed: %v", hosts)
	}
	if _, err = m.Discard(ctx, status.ID); err != nil {
		t.Fatal(err)
	}
	if len(m.List("")) != 0 {
		t.Fatal("a discarded terminal must not be listed")
	}
}

// Requests with the same key at the same time start one shell: the second
// waits for the first and gets its terminal. Once that shell exits, the key
// is free, and a new request with it starts a new shell.
func TestConcurrentStartsWithOneKeyStartOneShell(t *testing.T) {
	dir := private(t)
	m := manager(t, dir)
	defer m.Close()
	spec := shellSpec(t, dir)
	const racers = 4
	started := make(chan *Terminal, racers)
	for range racers {
		go func() {
			term, err := m.Start(spec, "open-once")
			if err != nil {
				t.Error(err)
			}
			started <- term
		}()
	}
	first := <-started
	for range racers - 1 {
		if term := <-started; term != first {
			t.Fatal("one request key started two shells")
		}
	}
	if live := m.List(""); len(live) != 1 {
		t.Fatalf("%d terminals for one request", len(live))
	}
	if err := first.Write([]byte("exit 0\n")); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if _, err := first.Wait(ctx); err != nil {
		t.Fatal(err)
	}
	m.mu.Lock()
	kept := len(m.keys)
	m.mu.Unlock()
	if kept != 0 {
		t.Fatal("an exited shell kept its request key")
	}
	again, err := m.Start(spec, "open-once")
	if err != nil {
		t.Fatal(err)
	}
	if again == first || again.Status().Done {
		t.Fatal("a request after the shell exited got the exited terminal")
	}
}

func TestTerminalOutlivesTheRunner(t *testing.T) {
	dir := private(t)
	first := manager(t, dir)
	spec := shellSpec(t, dir)
	term, err := first.Start(spec, "")
	if err != nil {
		t.Fatal(err)
	}
	_ = term.Write([]byte("echo first-$((1+1))\n"))
	until(t, term, 0, "first-2")
	id := term.Status().ID
	if err = first.Detach(); err != nil {
		t.Fatal(err)
	}

	second := manager(t, dir)
	defer second.Close()
	listed := second.List("workspace-1")
	if len(listed) != 1 || listed[0].ID != id || listed[0].Done {
		t.Fatalf("the next Runner must re-attach the running shell: %+v", listed)
	}
	adopted, err := second.Get(id)
	if err != nil {
		t.Fatal(err)
	}
	// The scrollback came with it.
	history, after := until(t, adopted, 0, "first-2")
	if !strings.Contains(history, "first-2") {
		t.Fatal(history)
	}
	_ = adopted.Write([]byte("echo second-$((2+1))\n"))
	until(t, adopted, after, "second-3")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	status, err := second.Discard(ctx, id)
	if err != nil || !status.Done {
		t.Fatalf("closing must stop the shell: %+v %v", status, err)
	}
	if ids, _ := sessionhost.Hosts(filepath.Join(dir, "hosts")); len(ids) != 0 {
		eventuallyGone(t, filepath.Join(dir, "hosts"))
	}
}

func eventuallyGone(t *testing.T, root string) {
	t.Helper()
	for end := time.Now().Add(5 * time.Second); time.Now().Before(end); time.Sleep(20 * time.Millisecond) {
		if ids, _ := sessionhost.Hosts(root); len(ids) == 0 {
			return
		}
	}
	t.Fatal("the terminal's host did not exit")
}

func TestOpenForgetsATerminalWhoseHostIsGone(t *testing.T) {
	dir := private(t)
	m := manager(t, dir)
	r := record{ID: "GONE0000", Host: "missinghost00000", Spec: Spec{CollectionID: "workspace-1"}, CreatedAt: time.Now()}
	if err := m.save(r); err != nil {
		t.Fatal(err)
	}
	_ = m.Close()
	again := manager(t, dir)
	defer again.Close()
	if len(again.List("")) != 0 {
		t.Fatal("a terminal whose host is gone must not be listed")
	}
	if _, err := os.Stat(filepath.Join(dir, "terminals", "GONE0000.json")); !os.IsNotExist(err) {
		t.Fatal("its record must be removed")
	}
}

func TestCloseStopsEveryShell(t *testing.T) {
	dir := private(t)
	m := manager(t, dir)
	term, err := m.Start(shellSpec(t, dir), "")
	if err != nil {
		t.Fatal(err)
	}
	until(t, term, 0, "$ ")
	if err = m.Close(); err != nil {
		t.Fatal(err)
	}
	if status := term.Status(); !status.Done {
		t.Fatalf("Close must stop the shell: %+v", status)
	}
	eventuallyGone(t, filepath.Join(dir, "hosts"))
	if _, err = m.Start(shellSpec(t, dir), ""); err == nil {
		t.Fatal("a closed manager must refuse new terminals")
	}
}

func TestStartRejectsInvalidSpecs(t *testing.T) {
	dir := private(t)
	m := manager(t, dir)
	defer m.Close()
	spec := shellSpec(t, dir)
	for name, change := range map[string]func(*Spec){
		"relative shell": func(s *Spec) { s.Shell = "sh" },
		"relative cwd":   func(s *Spec) { s.Cwd = "workspace" },
		"no workspace":   func(s *Spec) { s.CollectionID = "" },
		"zero size":      func(s *Spec) { s.Cols = 0 },
		"huge size":      func(s *Spec) { s.Rows = 5000 },
	} {
		bad := spec
		change(&bad)
		if _, err := m.Start(bad, ""); err == nil {
			t.Fatalf("%s: expected an error", name)
		}
	}
}

func TestLiveTerminalsAreBounded(t *testing.T) {
	dir := private(t)
	m, err := Open(filepath.Join(dir, "terminals"), filepath.Join(dir, "hosts"), Options{MaxLive: 1})
	if err != nil {
		t.Fatal(err)
	}
	defer m.Close()
	spec := shellSpec(t, dir)
	term, err := m.Start(spec, "")
	if err != nil {
		t.Fatal(err)
	}
	if _, err = m.Start(spec, ""); err == nil || !strings.Contains(err.Error(), "limit") {
		t.Fatalf("a second terminal must be refused, got %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if _, err = m.Discard(ctx, term.Status().ID); err != nil {
		t.Fatal(err)
	}
	if _, err = m.Start(spec, ""); err != nil {
		t.Fatalf("closing one frees its place: %v", err)
	}
}

func TestResolveConfinesTheDirectoryToTheWorkspace(t *testing.T) {
	dir := private(t)
	root := filepath.Join(dir, "workspace")
	outside := filepath.Join(dir, "outside")
	for _, path := range []string{filepath.Join(root, "api", "src"), outside} {
		if err := os.MkdirAll(path, 0700); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.Symlink(outside, filepath.Join(root, "escape")); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "file"), nil, 0600); err != nil {
		t.Fatal(err)
	}
	for directory, want := range map[string]string{"": root, "api": filepath.Join(root, "api"), "api/src/..": filepath.Join(root, "api"), " api/src ": filepath.Join(root, "api", "src")} {
		got, err := Resolve(root, directory)
		if err != nil || got != want {
			t.Fatalf("Resolve(%q) = %q, %v; want %q", directory, got, err, want)
		}
	}
	for _, directory := range []string{"..", "../outside", "api/../../outside", outside, "/etc", "escape", "missing", "file", "a\x00b"} {
		if got, err := Resolve(root, directory); err == nil {
			t.Fatalf("Resolve(%q) = %q; a directory outside the workspace must be refused", directory, got)
		}
	}
}

func TestEnvironmentKeepsTheUsersAndOnlyTheDocumentedRunnerVariables(t *testing.T) {
	env := Environment([]string{"PATH=/usr/bin", "HOME=/home/me", "LAZYBOX_V2_STATE=/state", "LAZYBOX_V2_KEEP_AGENTS=1", "LAZYBOX_V2_ENDPOINT=/other", "CODEFLY_SCOPED_AUTH_SECRET=secret", "CODEFLY__SERVICE=runner", "TERM=dumb", "TERM_PROGRAM=iTerm.app", "CLAUDECODE=1", "PWD=/runner"}, "workspace-1")
	want := []string{"PATH=/usr/bin", "HOME=/home/me", "TERM=xterm-256color", "COLORTERM=truecolor", "LAZYBOX_V2_COLLECTION_ID=workspace-1"}
	if !slices.Equal(env, want) {
		t.Fatalf("environment %q, want %q", env, want)
	}
}

func TestShellFallsBackToASystemShell(t *testing.T) {
	if got, err := Shell("/bin/sh"); err != nil || got != "/bin/sh" {
		t.Fatalf("Shell(/bin/sh) = %q, %v", got, err)
	}
	for _, preferred := range []string{"", "zsh", "/nonexistent/shell", "/etc"} {
		got, err := Shell(preferred)
		if err != nil || (got != "/bin/zsh" && got != "/bin/sh") {
			t.Fatalf("Shell(%q) = %q, %v", preferred, got, err)
		}
	}
}

func TestInteractiveKeyboardSurvivesAdoption(t *testing.T) {
	dir := private(t)
	first := manager(t, dir)
	defer first.Close()
	spec := shellSpec(t, dir)
	spec.Place, spec.AgentConversation = InteractiveCommandPlace, "conversation"
	terminal, err := first.Start(spec, "")
	if err != nil {
		t.Fatal(err)
	}
	if err := terminal.SetPersonControls(true); err != nil {
		t.Fatal(err)
	}
	if err := first.Detach(); err != nil {
		t.Fatal(err)
	}
	second := manager(t, dir)
	defer second.Close()
	adopted, err := second.Get(terminal.Status().ID)
	if err != nil {
		t.Fatal(err)
	}
	if adopted.Status().Spec.KeyboardControl() != "person" {
		t.Fatal("lost keyboard ownership on adoption")
	}
	if err := adopted.WriteAgent([]byte("unexpected")); err == nil {
		t.Fatal("agent could type after adoption")
	}
	if err := adopted.Write([]byte("echo adopted-$((2+2))\n")); err != nil {
		t.Fatal(err)
	}
	until(t, adopted, 0, "adopted-4")
}
