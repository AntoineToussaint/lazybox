package agentshell

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// hostile is a home whose every startup file puts another lazybox (a V1 in
// ~/v1bin) first on PATH, the way ~/.zshenv's `path=(~/.cargo/bin … $path)`,
// mise or Homebrew's shellenv do.
type hostile struct {
	home, v1, cli, integration string
}

func newHostile(t *testing.T) hostile {
	t.Helper()
	root := t.TempDir()
	h := hostile{home: filepath.Join(root, "home"), cli: filepath.Join(root, "lazybox-v2", "bin"), integration: filepath.Join(root, "state", "shell-integration")}
	h.v1 = filepath.Join(h.home, "v1bin")
	program := func(dir, says string) {
		if err := os.MkdirAll(dir, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(dir, "lazybox"), []byte("#!/bin/sh\necho "+says+"\n"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	program(h.v1, "v1")
	program(h.cli, "v2")
	zsh := "typeset -U path PATH\npath=(\"$HOME/v1bin\" $path)\nexport PATH\n"
	sh := "PATH=\"$HOME/v1bin:$PATH\"; export PATH\n"
	for name, text := range map[string]string{".zshenv": zsh, ".zprofile": zsh, ".zshrc": zsh, ".zlogin": zsh, ".bash_profile": sh, ".bashrc": sh, ".profile": sh} {
		if err := os.WriteFile(filepath.Join(h.home, name), []byte(text), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := Install(h.integration); err != nil {
		t.Fatal(err)
	}
	return h
}

// base is a clean environment: the hostile home, a standard PATH.
func (h hostile) base(extra ...string) []string {
	return append([]string{"HOME=" + h.home, "PATH=/usr/bin:/bin:/usr/sbin:/sbin", "TERM=dumb", "LANG=C"}, extra...)
}

func (h hostile) run(t *testing.T, env []string, argv ...string) string {
	t.Helper()
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Env = env
	cmd.Dir = h.home
	out, err := cmd.Output()
	if err != nil {
		t.Fatalf("%q: %v\n%s", argv, err, out)
	}
	return strings.TrimSpace(string(out))
}

func shells(t *testing.T, names ...string) map[string]string {
	found := map[string]string{}
	for _, name := range names {
		if path, err := exec.LookPath(name); err == nil {
			found[name] = path
		}
	}
	if len(found) == 0 {
		t.Skip("no shell to test")
	}
	return found
}

// invocations are the ways agents' tools start a shell: plain and login
// (`-lc` is how Codex runs every command), interactive, nested shells, and
// macOS's own bash 3.2 when the bash on PATH is newer.
func invocations(t *testing.T) [][]string {
	found := shells(t, "zsh", "bash")
	var out [][]string
	if zsh := found["zsh"]; zsh != "" {
		out = append(out, []string{zsh, "-c", "lazybox"}, []string{zsh, "-lc", "lazybox"}, []string{zsh, "-l", "-c", "lazybox"}, []string{zsh, "-ic", "lazybox"},
			[]string{zsh, "-c", zsh + " -lc lazybox"})
		if bash := found["bash"]; bash != "" {
			out = append(out, []string{zsh, "-lc", bash + " -c lazybox"})
		}
	}
	for _, bash := range []string{found["bash"], "/bin/bash"} {
		if bash == "" {
			continue
		}
		if _, err := os.Stat(bash); err != nil {
			continue
		}
		out = append(out, []string{bash, "-c", "lazybox"}, []string{bash, "-lc", "lazybox"}, []string{bash, "-c", bash + " -lc lazybox"})
	}
	return out
}

func TestHostileStartupFilesShadowTheCLIWithoutTheIntegration(t *testing.T) {
	h := newHostile(t)
	raw := Env(h.base(), "", h.cli)
	if !strings.HasPrefix(value(raw, "PATH"), h.cli+":") {
		t.Fatalf("CLI not first on the initial PATH: %s", value(raw, "PATH"))
	}
	shadowed := 0
	for _, argv := range invocations(t) {
		if h.run(t, raw, argv...) == "v1" {
			shadowed++
		}
	}
	// The premise of the fix: the startup files really do win over the PATH
	// a process is given.
	if shadowed == 0 {
		t.Fatal("no invocation reached the V1 lazybox: the hostile home does not reproduce the bug")
	}
}

func TestEveryAgentShellReachesTheCLIDespiteHostileStartupFiles(t *testing.T) {
	h := newHostile(t)
	env := Env(h.base(), h.integration, h.cli)
	for _, argv := range invocations(t) {
		if got := h.run(t, env, argv...); got != "v2" {
			t.Errorf("%q ran the %s lazybox, want this Runner's", argv, got)
		}
	}
}

// Claude Code runs each Bash tool command as `<shell> -c [-l] 'source
// <snapshot> && <CLAUDE_ENV_FILE text> && eval <command>'`. Its snapshot
// exports a PATH (the one in the person's Claude settings, when they set one),
// which comes after every startup file; CLAUDE_ENV_FILE's text runs after it.
func TestClaudeBashToolCommandReachesTheCLI(t *testing.T) {
	h := newHostile(t)
	env := Env(h.base(), h.integration, h.cli)
	snapshot := filepath.Join(h.home, "snapshot.sh")
	if err := os.WriteFile(snapshot, []byte("export PATH="+h.v1+":/usr/bin:/bin\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	envFile, err := os.ReadFile(value(env, "CLAUDE_ENV_FILE"))
	if err != nil {
		t.Fatal(err)
	}
	cwd := filepath.Join(h.home, "cwd")
	command := "source '" + snapshot + "' 2>/dev/null || true && " + strings.TrimSpace(string(envFile)) + "\n: && eval 'lazybox' && pwd -P >| '" + cwd + "'"
	for name, shell := range shells(t, "zsh", "bash") {
		for _, mode := range [][]string{{"-c"}, {"-c", "-l"}} {
			argv := append(append([]string{shell}, mode...), command)
			if got := h.run(t, env, argv...); got != "v2" {
				t.Errorf("%s %v: Claude's Bash tool ran the %s lazybox", name, mode, got)
			}
		}
	}
	// Without the env file, the snapshot's PATH wins: the case it covers.
	// `.`, not `source`: /bin/sh is dash on Debian and Ubuntu.
	plain := ". '" + snapshot + "' 2>/dev/null || true && eval 'lazybox'"
	if got := h.run(t, env, "/bin/sh", "-c", plain); got != "v1" {
		t.Fatalf("snapshot PATH did not shadow the CLI (%s): the test proves nothing", got)
	}
}

func TestPersonsOwnHooksStillRun(t *testing.T) {
	h := newHostile(t)
	zdot := filepath.Join(h.home, ".config", "zsh")
	if err := os.MkdirAll(zdot, 0o755); err != nil {
		t.Fatal(err)
	}
	for name, text := range map[string]string{
		".zshenv":   "export SEEN_ZSHENV=$ZDOTDIR\npath=(\"$HOME/v1bin\" $path)\n",
		".zprofile": "export SEEN_ZPROFILE=1\npath=(\"$HOME/v1bin\" $path)\n",
		".zshrc":    "export SEEN_ZSHRC=$ZDOTDIR\n",
	} {
		if err := os.WriteFile(filepath.Join(zdot, name), []byte(text), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	bashEnv := filepath.Join(h.home, "bash-env")
	claudeEnv := filepath.Join(h.home, "claude-env")
	if err := os.WriteFile(bashEnv, []byte("export SEEN_BASH_ENV=1\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(claudeEnv, []byte("export SEEN_CLAUDE_ENV=1\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	env := Env(h.base("ZDOTDIR="+zdot, "BASH_ENV="+bashEnv, "CLAUDE_ENV_FILE="+claudeEnv), h.integration, h.cli)
	for _, want := range []string{"LAZYBOX_V2_USER_ZDOTDIR=" + zdot, "LAZYBOX_V2_USER_BASH_ENV=" + bashEnv, "LAZYBOX_V2_USER_CLAUDE_ENV_FILE=" + claudeEnv} {
		if !contains(env, want) {
			t.Errorf("missing %s in %v", want, env)
		}
	}
	found := shells(t, "zsh", "bash")
	if zsh := found["zsh"]; zsh != "" {
		got := h.run(t, env, zsh, "-ilc", `lazybox; echo "$SEEN_ZSHENV|$SEEN_ZPROFILE|$SEEN_ZSHRC|$ZDOTDIR|${HISTFILE-}"`)
		// The person's files ran with their own ZDOTDIR; the integration's stays
		// for nested shells; history is not moved into the integration.
		want := "v2\n" + zdot + "|1|" + zdot + "|" + filepath.Join(h.integration, "zsh") + "|"
		if !strings.HasPrefix(got, want) || strings.Contains(got, h.integration+"/zsh/.zsh_history") {
			t.Errorf("zsh: got %q, want prefix %q", got, want)
		}
	}
	if bash := found["bash"]; bash != "" {
		if got := h.run(t, env, bash, "-c", `lazybox; echo "$SEEN_BASH_ENV"`); got != "v2\n1" {
			t.Errorf("bash: got %q", got)
		}
	}
	envFile, err := os.ReadFile(value(env, "CLAUDE_ENV_FILE"))
	if err != nil {
		t.Fatal(err)
	}
	if got := h.run(t, env, "/bin/sh", "-c", string(envFile)+`echo "$SEEN_CLAUDE_ENV"`); got != "1" {
		t.Errorf("CLAUDE_ENV_FILE: the person's own was not chained: %q", got)
	}
}

// A ZDOTDIR the person's ~/.zshenv sets (to move zsh's files) is followed:
// their later files come from there.
func TestZDOTDIRSetInZshenvIsFollowed(t *testing.T) {
	zsh := shells(t, "zsh")["zsh"]
	if zsh == "" {
		t.Skip("no zsh")
	}
	h := newHostile(t)
	moved := filepath.Join(h.home, ".zdot")
	if err := os.MkdirAll(moved, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(h.home, ".zshenv"), []byte("export ZDOTDIR=$HOME/.zdot\npath=(\"$HOME/v1bin\" $path)\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(moved, ".zprofile"), []byte("export SEEN_MOVED=1\npath=(\"$HOME/v1bin\" $path)\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	env := Env(h.base(), h.integration, h.cli)
	if got := h.run(t, env, zsh, "-lc", `lazybox; echo "$SEEN_MOVED|$LAZYBOX_V2_USER_ZDOTDIR"`); got != "v2\n1|"+moved {
		t.Errorf("got %q", got)
	}
}

// An integration inherited from a Lazybox terminal (a Runner started from
// one) is never chained to itself: the person's saved value is used.
func TestInheritedIntegrationIsNotChainedToItself(t *testing.T) {
	h := newHostile(t)
	other := filepath.Join(t.TempDir(), "other")
	if err := Install(other); err != nil {
		t.Fatal(err)
	}
	inherited := h.base("ZDOTDIR="+filepath.Join(other, "zsh"), "LAZYBOX_V2_USER_ZDOTDIR=/home/zdot", "BASH_ENV="+filepath.Join(other, "bash-env.sh"), "CLAUDE_ENV_FILE="+filepath.Join(other, "claude-env.sh"))
	env := Env(inherited, h.integration, h.cli)
	if got := value(env, "LAZYBOX_V2_USER_ZDOTDIR"); got != "/home/zdot" {
		t.Errorf("user ZDOTDIR %q", got)
	}
	for _, name := range []string{"LAZYBOX_V2_USER_BASH_ENV", "LAZYBOX_V2_USER_CLAUDE_ENV_FILE"} {
		if got, ok := lookup(env, name); ok {
			t.Errorf("%s chained to an integration: %q", name, got)
		}
	}
	if value(env, "ZDOTDIR") != filepath.Join(h.integration, "zsh") {
		t.Errorf("ZDOTDIR %q", value(env, "ZDOTDIR"))
	}
}

func TestEnvWithoutCLIOrIntegration(t *testing.T) {
	base := []string{"PATH=/usr/bin", "ZDOTDIR=/z", "BASH_ENV=/b"}
	if got := Env(base, "/nowhere", ""); strings.Join(got, ",") != strings.Join(base, ",") {
		t.Errorf("no CLI changed env: %v", got)
	}
	// No integration installed: the CLI first, the person's hooks untouched.
	got := Env(append(base, "PATH=/bin:/cli:/usr/bin"), filepath.Join(t.TempDir(), "missing"), "/cli")
	if value(got, "PATH") != "/cli:/bin:/usr/bin" || value(got, "ZDOTDIR") != "/z" || value(got, "BASH_ENV") != "/b" || value(got, "LAZYBOX_V2_CLI") != "/cli/lazybox" {
		t.Errorf("env %v", got)
	}
}

func TestInstallKeepsUnchangedFilesAndIsPrivate(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "integration")
	if err := Install(dir); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(dir, "zsh", ".zshenv")
	before, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if before.Mode().Perm()&0o077 != 0 {
		t.Errorf("integration file readable by others: %v", before.Mode())
	}
	if err = Install(dir); err != nil {
		t.Fatal(err)
	}
	after, _ := os.Stat(path)
	if !os.SameFile(before, after) {
		t.Error("an unchanged file was replaced")
	}
	if err = Install("relative"); err == nil {
		t.Error("relative directory accepted")
	}
}

func TestProbeReportsShadowAndFix(t *testing.T) {
	h := newHostile(t)
	found := shells(t, "zsh", "bash")
	var list []string
	for _, name := range []string{"zsh", "bash"} {
		if found[name] != "" {
			list = append(list, found[name])
		}
	}
	c := Probe(context.Background(), list, Env(h.base(), "", h.cli), Env(h.base(), h.integration, h.cli), filepath.Join(h.cli, "lazybox"))
	if c.Shadow != filepath.Join(h.v1, "lazybox") || len(c.ShadowIn) == 0 || !c.Fixed() {
		t.Fatalf("probe %+v", c)
	}
	if n := c.Notice(); !strings.Contains(n, c.Shadow) || !strings.Contains(n, "nothing to do") {
		t.Errorf("notice %q", n)
	}
	// /bin/sh as a login shell reads ~/.profile and no hook of ours: the probe
	// says so, and the notice gives the full path.
	c = Probe(context.Background(), []string{"/bin/sh"}, Env(h.base(), "", h.cli), Env(h.base(), h.integration, h.cli), filepath.Join(h.cli, "lazybox"))
	if c.Fixed() || len(c.Unfixed) == 0 || !strings.Contains(c.Unfixed[0], "sh -lc finds "+filepath.Join(h.v1, "lazybox")) {
		t.Fatalf("unfixable probe %+v", c)
	}
	if n := c.Notice(); !strings.Contains(n, filepath.Join(h.cli, "lazybox")) {
		t.Errorf("notice %q lacks the full path", n)
	}
	// Nothing shadows the CLI: nothing to say. (Not probed: a login shell
	// reads the system's profile, which on a developer's machine may well put
	// a Homebrew lazybox first.)
	if clean := (Check{CLI: "/x/lazybox"}); clean.Notice() != "" || !clean.Fixed() {
		t.Errorf("clean check %+v", clean)
	}
	// A shell that does not run is a problem, not a shadow.
	broken := Probe(context.Background(), []string{filepath.Join(t.TempDir(), "nosh")}, nil, nil, "/x/lazybox")
	if broken.Problem == "" || broken.Notice() != "" {
		t.Errorf("broken probe %+v", broken)
	}
}

func TestProbeShells(t *testing.T) {
	if got := ProbeShells(func(string) string { return "/bin/zsh" }); len(got) != 1 || got[0] != "/bin/zsh" {
		t.Errorf("zsh: %v", got)
	}
	if got := ProbeShells(func(string) string { return "/usr/local/bin/fish" }); got[0] != "/usr/local/bin/fish" || (len(got) == 2 && filepath.Base(got[1]) != "bash") {
		t.Errorf("fish: %v", got)
	}
	if got := ProbeShells(func(string) string { return "" }); len(got) == 0 || !filepath.IsAbs(got[0]) {
		t.Errorf("unset: %v", got)
	}
}

func lookup(env []string, name string) (string, bool) {
	for i := len(env) - 1; i >= 0; i-- {
		if v, ok := strings.CutPrefix(env[i], name+"="); ok {
			return v, true
		}
	}
	return "", false
}

func value(env []string, name string) string { v, _ := lookup(env, name); return v }

func contains(env []string, entry string) bool {
	for _, e := range env {
		if e == entry {
			return true
		}
	}
	return false
}

// An interactive shell (a workspace terminal) marks where each command line
// ended and its status, ahead of the person's own prompt hooks, so the window
// can tell that a command it typed finished and how.
func TestInteractiveZshMarksEachCommandsEnd(t *testing.T) {
	zsh := shells(t, "zsh")["zsh"]
	if zsh == "" {
		t.Skip("no zsh")
	}
	h := newHostile(t)
	// The person's own prompt hook, which changes $? itself, runs after the mark.
	if err := os.WriteFile(filepath.Join(h.home, ".zshrc"), []byte("theirs() { true; }\nprecmd_functions+=(theirs)\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	cmd := exec.Command(zsh, "-i")
	cmd.Env = Env(h.base(), h.integration, h.cli)
	cmd.Dir = h.home
	cmd.Stdin = strings.NewReader("false\n(exit 3)\nexit\n")
	out, _ := cmd.CombinedOutput()
	marks := []string{}
	for _, part := range strings.Split(string(out), "\x1b]133;D;")[1:] {
		status, _, _ := strings.Cut(part, "\a")
		marks = append(marks, status)
	}
	if strings.Join(marks, ",") != "0,1,3" {
		t.Fatalf("marks %v in %q", marks, out)
	}
	// A command an agent runs is not interactive: no marks in its output.
	if got := h.run(t, Env(h.base(), h.integration, h.cli), zsh, "-lc", "false; echo done"); strings.Contains(got, "\x1b]133") {
		t.Fatalf("a non-interactive shell marked: %q", got)
	}
}
