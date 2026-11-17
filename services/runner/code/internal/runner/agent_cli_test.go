package runner

import (
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/accounts"
	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/agent/agenttest"
)

// hostileHome makes HOME a home whose zsh and bash startup files put a V1
// lazybox first on PATH, and returns this Runner's CLI directory, holding a
// lazybox that says "v2", and the V1's path.
func hostileHome(t *testing.T, shell string) (cliDir, v1 string) {
	t.Helper()
	root := t.TempDir()
	home := filepath.Join(root, "home")
	cliDir = filepath.Join(root, "cli")
	for dir, says := range map[string]string{filepath.Join(home, ".cargo", "bin"): "v1", cliDir: "v2"} {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, "lazybox"), []byte("#!/bin/sh\necho "+says+"\n"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	// The startup file that broke a real agent: ~/.zshenv rebuilding PATH.
	zshenv := "typeset -U path PATH\npath=(\"$HOME/.local/bin\" \"$HOME/.cargo/bin\" \"$HOME/go/bin\" $path)\nexport PATH\n"
	profile := "PATH=\"$HOME/.cargo/bin:$PATH\"; export PATH\n"
	for name, text := range map[string]string{".zshenv": zshenv, ".bash_profile": profile, ".profile": profile} {
		if err := os.WriteFile(filepath.Join(home, name), []byte(text), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	t.Setenv("HOME", home)
	t.Setenv("SHELL", shell)
	for _, name := range []string{"ZDOTDIR", "BASH_ENV", "CLAUDE_ENV_FILE"} {
		if value, ok := os.LookupEnv(name); ok {
			t.Setenv(name, value)
			os.Unsetenv(name)
		}
	}
	return cliDir, filepath.Join(home, ".cargo", "bin", "lazybox")
}

func runIn(t *testing.T, env []string, argv ...string) string {
	t.Helper()
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Env = env
	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("%q: %v\n%s", argv, err, out)
	}
	return strings.TrimSpace(string(out))
}

// cliNotices waits for the CLI probe, then reads the conversation's journal
// for Runner notices about the CLI.
func cliNotices(t *testing.T, b *Bench, e *Execution) []map[string]any {
	t.Helper()
	_, done := b.AgentCLI()
	select {
	case <-done:
	case <-time.After(30 * time.Second):
		t.Fatal("the CLI probe did not finish")
	}
	var found []map[string]any
	for cursor := uint64(0); ; cursor++ {
		// The notice is recorded just after the probe ends: wait a little for it.
		ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
		r, err := e.spool.Next(ctx, cursor)
		cancel()
		if err != nil {
			return found
		}
		var event map[string]any
		if json.Unmarshal(r.Value, &event) == nil && event["type"] == "cli_notice" {
			found = append(found, event)
		}
	}
}

func TestAgentShellsReachThisRunnersCLIDespiteTheirStartupFiles(t *testing.T) {
	zsh, err := exec.LookPath("zsh")
	if err != nil {
		t.Skip("no zsh")
	}
	cliDir, _ := hostileHome(t, zsh)
	b, err := New(Config{State: t.TempDir(), CLIDir: cliDir})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	fake := &agenttest.Adapter{}
	b.Adapters["codex"] = fake
	if _, err = b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct"); err != nil {
		t.Fatal(err)
	}
	env := fake.Session.Open.Env
	// How Codex runs every command, how Claude Code falls back without a
	// snapshot, a nested shell, and bash.
	for _, argv := range [][]string{{zsh, "-lc", "lazybox"}, {zsh, "-c", "-l", "lazybox"}, {zsh, "-c", "lazybox"}, {zsh, "-c", "bash -lc lazybox"}, {"/bin/bash", "-lc", "lazybox"}} {
		if got := runIn(t, env, argv...); got != "v2" {
			t.Errorf("%q in the agent's environment ran the %s lazybox", argv, got)
		}
	}
	if !contains(env, "LAZYBOX_V2_CLI="+filepath.Join(cliDir, "lazybox")) {
		t.Errorf("no full path for the briefing's fallback: %v", env)
	}
	if !strings.Contains(fake.Session.Open.Context, `"`+filepath.Join(cliDir, "lazybox")+`"`) {
		t.Error("the briefing does not name the CLI's full path")
	}
}

func TestShadowedCLIIsAnnouncedOnceWhenFixed(t *testing.T) {
	zsh, err := exec.LookPath("zsh")
	if err != nil {
		t.Skip("no zsh")
	}
	cliDir, v1 := hostileHome(t, zsh)
	b, err := New(Config{State: t.TempDir(), CLIDir: cliDir})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	first, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	notices := cliNotices(t, b, first)
	if len(notices) != 1 || notices[0]["status"] != "fixed" || !strings.Contains(notices[0]["text"].(string), v1) {
		t.Fatalf("notices %v", notices)
	}
	check, _ := b.AgentCLI()
	if check == nil || check.Shadow != v1 || !check.Fixed() {
		t.Fatalf("check %+v", check)
	}
	// A shadow Lazybox works around is said once, not in every conversation.
	second, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
	if err != nil {
		t.Fatal(err)
	}
	if notices = cliNotices(t, b, second); len(notices) != 0 {
		t.Fatalf("announced again: %v", notices)
	}
}

func TestUnfixableShadowIsAnnouncedInEveryConversationWithTheFullPath(t *testing.T) {
	// /bin/sh as a login shell reads ~/.profile and none of the integration's hooks.
	cliDir, v1 := hostileHome(t, "/bin/sh")
	b, err := New(Config{State: t.TempDir(), CLIDir: cliDir})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	b.Adapters["codex"] = &agenttest.Adapter{}
	for i := 0; i < 2; i++ {
		e, err := b.OpenEmptyWorkspace("codex-default", accounts.Options{}, "direct")
		if err != nil {
			t.Fatal(err)
		}
		notices := cliNotices(t, b, e)
		if len(notices) != 1 || notices[0]["status"] != "unfixed" {
			t.Fatalf("conversation %d: notices %v", i, notices)
		}
		text := notices[0]["text"].(string)
		if !strings.Contains(text, v1) || !strings.Contains(text, filepath.Join(cliDir, "lazybox")) {
			t.Fatalf("notice %q lacks the shadow or the full path", text)
		}
	}
}

func TestNoProbeWithoutARunnableCLI(t *testing.T) {
	b, err := New(Config{State: t.TempDir(), CLIDir: "/opt/lazybox/bin"})
	if err != nil {
		t.Fatal(err)
	}
	defer b.Close()
	check, done := b.AgentCLI()
	select {
	case <-done:
	default:
		t.Fatal("a probe started for a CLI that does not exist")
	}
	if check != nil {
		t.Fatalf("check %+v", check)
	}
}
