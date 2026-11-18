package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/installation"
)

// release is an installed Lazybox for one test: a runtime with stand-in
// programs, a temporary home, and a service manager that only records: this
// platform's (launchd on macOS, systemd on Linux), so the definition written
// and read back is the real one.
type release struct {
	t       *testing.T
	home    string
	runtime installation.Layout
	paths   installation.Paths
	loaded  bool
	calls   []string
	ready   string // the version the "started" Runner answers with
	path    string // the PATH lazybox was started with (the app's, say)
	// shell is the login shell's PATH, the one a terminal gets; shellProblem
	// makes reading it fail.
	shell, shellProblem string
	// logs, when set, is where the manager says the Runner's output goes
	// (the journal would be the real one's on Linux).
	logs *installation.LogSource
}

func newRelease(t *testing.T, version string) *release {
	t.Helper()
	r := &release{t: t, home: t.TempDir(), ready: version, shell: "/usr/bin:/bin"}
	r.runtime = installation.Layout{Root: filepath.Join(t.TempDir(), "Lazybox.app", "Contents", "Resources", "runtime")}
	r.write(version)
	var err error
	if r.paths, err = installation.UserPaths(r.getenv); err != nil {
		t.Fatal(err)
	}
	return r
}

// write lays out the runtime at version: the CLI, Runner and a Node that answers --version.
func (r *release) write(version string) {
	r.t.Helper()
	for path, body := range map[string]string{
		r.runtime.CLI():     "#!/bin/sh\n",
		r.runtime.Runner():  "#!/bin/sh\n",
		r.runtime.Node():    "#!/bin/sh\necho v26.13.1\n",
		r.runtime.Worker():  "",
		r.runtime.Version(): version + "\n",
	} {
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			r.t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(body), 0o755); err != nil {
			r.t.Fatal(err)
		}
	}
	r.ready = version
}

func (r *release) getenv(name string) string {
	switch name {
	case "HOME":
		return r.home
	case "SHELL":
		return "/bin/zsh"
	case "PATH":
		return r.path
	}
	return ""
}

func (r *release) setup() setup {
	return setup{
		installer: func() (installation.Installer, error) {
			return installation.Installer{Paths: r.paths, Layout: r.runtime, Manager: r.manager(),
				Shell: func(context.Context) installation.ShellEnvironment {
					if r.shellProblem != "" {
						return installation.ShellEnvironment{Shell: "/bin/zsh", Problem: r.shellProblem}
					}
					return installation.ShellEnvironment{Shell: "/bin/zsh", Path: r.shell}
				}}, nil
		},
		ready: func(context.Context, string, string) (string, error) { return r.ready, nil },
	}
}

// manager is this platform's service manager, driven by a fake.
func (r *release) manager() installation.Manager {
	var m installation.Manager = installation.Launchd{Domain: "gui/501", Run: r.launchctl}
	if runtime.GOOS == "linux" {
		m = installation.Systemd{User: "me", Run: r.systemctl}
	}
	if r.logs != nil {
		return logsAt{m, *r.logs}
	}
	return m
}

// logsAt is a manager whose Runner output goes where the test says.
type logsAt struct {
	installation.Manager
	source installation.LogSource
}

func (l logsAt) Logs(installation.Paths) installation.LogSource { return l.source }

// definition is where the service definition is written.
func (r *release) definition() string { return r.manager().DefinitionPath(r.paths) }

func (r *release) systemctl(_ context.Context, program string, args ...string) ([]byte, error) {
	r.calls = append(r.calls, program+" "+strings.Join(args, " "))
	if program == "loginctl" {
		return []byte("Linger=yes\n"), nil
	}
	switch args[1] {
	case "show":
		if !r.loaded {
			return []byte("LoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\nUnitFileState=enabled\n"), nil
		}
		return []byte("LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=777\nUnitFileState=enabled\n"), nil
	case "enable", "restart", "start":
		r.loaded = true
	case "stop", "disable":
		r.loaded = false
	}
	return nil, nil
}

func (r *release) launchctl(_ context.Context, args ...string) ([]byte, error) {
	r.calls = append(r.calls, strings.Join(args, " "))
	switch args[0] {
	case "print":
		if !r.loaded {
			return []byte("Could not find service"), errors.New("exit status 113")
		}
		return []byte("state = running\npid = 777\n"), nil
	case "bootstrap":
		r.loaded = true
	case "bootout":
		r.loaded = false
	}
	return nil, nil
}

func (r *release) run(stdin string, tty bool, args ...string) result {
	r.t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	var stdout, stderr bytes.Buffer
	c := &cli{ctx: ctx, stdin: strings.NewReader(stdin), stdout: &stdout, stderr: &stderr, getenv: r.getenv, now: time.Now, poll: 20 * time.Millisecond, stdinTTY: tty, install: r.setup()}
	handled, err := powerVerb(c, args)
	if !handled {
		r.t.Fatalf("%v is not a verb", args)
	}
	code := 0
	if err != nil {
		code = exitCode(err)
	}
	return result{stdout.String(), stderr.String(), code, err}
}

func TestServiceInstallUpdateStatusAndUninstall(t *testing.T) {
	r := newRelease(t, "0.1.0")
	got := r.run("", false, "service", "install")
	if got.code != 0 || !strings.Contains(got.stdout, "The Runner (0.1.0) is running") || !r.loaded {
		t.Fatalf("install: %+v calls %v", got, r.calls)
	}
	if _, err := os.Stat(r.definition()); err != nil {
		t.Fatal(err)
	}

	// The app updated in place: the same command moves the service to the new release.
	r.write("0.2.0")
	got = r.run("", false, "service", "install")
	if got.code != 0 || !strings.Contains(got.stdout, "Updated the Runner from 0.1.0 to 0.2.0") {
		t.Fatalf("update: %+v", got)
	}

	got = r.run("", false, "service", "status", "--json")
	var status serviceReport
	if err := json.Unmarshal([]byte(got.stdout), &status); err != nil {
		t.Fatalf("%v: %s", err, got.stdout)
	}
	if !status.Installed || !status.Running || status.PID != 777 || status.Version != "0.2.0" || status.ReleaseVersion != "0.2.0" {
		t.Fatalf("status %+v", status)
	}
	// No Runner answers in this test, and status says so with exit 6.
	if got = r.run("", false, "service", "status"); got.code != exitUnavailable || !strings.Contains(got.stdout, "running · pid 777") {
		t.Fatalf("status: %+v", got)
	}

	if got = r.run("", false, "service", "stop"); got.code != 0 || r.loaded || !strings.Contains(got.stdout, "keep running") {
		t.Fatalf("stop: %+v", got)
	}
	if got = r.run("", false, "service", "start"); got.code != 0 || !r.loaded {
		t.Fatalf("start: %+v", got)
	}
	if got = r.run("", false, "service", "uninstall"); got.code != 0 || r.loaded || !strings.Contains(got.stdout, "Removed the Runner service") {
		t.Fatalf("uninstall: %+v", got)
	}
	if _, err := os.Stat(r.definition()); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("the plist survived uninstall")
	}
	if got = r.run("", false, "service", "bogus"); got.code != exitUsage {
		t.Fatalf("unknown subcommand: %+v", got)
	}
}

func TestServiceLogsPrintsTheTail(t *testing.T) {
	r := newRelease(t, "0.1.0")
	r.logs = &installation.LogSource{File: r.paths.Log()}
	if got := r.run("", false, "service", "logs"); got.code != exitNotFound {
		t.Fatalf("no log yet: %+v", got)
	}
	_ = os.MkdirAll(r.paths.Logs, 0o700)
	var log strings.Builder
	for i := range 200 {
		log.WriteString("line " + string(rune('a'+i%26)) + "\n")
	}
	log.WriteString("the last line\n")
	_ = os.WriteFile(r.paths.Log(), []byte(log.String()), 0o600)
	got := r.run("", false, "service", "logs", "--lines", "3")
	if lines := strings.Split(strings.TrimSpace(got.stdout), "\n"); len(lines) != 3 || lines[2] != "the last line" {
		t.Fatalf("%q", got.stdout)
	}
}

// Under systemd the Runner's output is in the journal: `service logs` runs
// the command that prints it, limited to the lines asked for.
func TestServiceLogsReadsTheJournal(t *testing.T) {
	r := newRelease(t, "0.1.0")
	r.logs = &installation.LogSource{Command: []string{"/bin/sh", "-c", `echo "journal $*"`, "journalctl"}, Lines: "--lines"}
	got := r.run("", false, "service", "logs", "--lines", "5")
	if got.code != 0 || strings.TrimSpace(got.stdout) != "journal --lines 5" {
		t.Fatalf("%+v", got)
	}
	r.logs.Command = []string{"/bin/sh", "-c", "exit 3"}
	if got = r.run("", false, "service", "logs"); got.code != exitUnavailable {
		t.Fatalf("a failing journal: %+v", got)
	}
}

// Doctor runs every check and names the fix: here the Runner answers but runs
// another version than the release, which is the one failure.
func TestDoctorChecksTheInstallationAndNamesTheFix(t *testing.T) {
	r := newRelease(t, "0.3.0")
	runner := newTestRunner(t)
	r.loaded = true
	if err := os.MkdirAll(r.paths.State, 0o700); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(runner.endpoint)
	if err := os.WriteFile(r.paths.Endpoint(), data, 0o600); err != nil {
		t.Fatal(err)
	}
	if got := r.run("", false, "service", "install", "--no-start"); got.code != 0 {
		t.Fatalf("%+v", got)
	}
	r.path = filepath.Join(r.home, "bin")
	got := r.run("", false, "doctor", "--json")
	var report struct {
		Healthy bool    `json:"healthy"`
		Checks  []check `json:"checks"`
	}
	if err := json.Unmarshal([]byte(got.stdout), &report); err != nil {
		t.Fatalf("%v: %s %s", err, got.stdout, got.stderr)
	}
	status := map[string]check{}
	for _, k := range report.Checks {
		status[k.ID] = k
	}
	for id, want := range map[string]string{"release": checkOK, "service": checkOK, "endpoint": checkOK, "versions": checkFail, "node": checkOK, "worker": checkOK, "disk": checkOK, "cli": checkWarn, "github": checkWarn} {
		if status[id].Status != want {
			t.Errorf("%s: %+v, want %s", id, status[id], want)
		}
	}
	if status["versions"].Fix == "" || !strings.Contains(status["versions"].Detail, "0.3.0") {
		t.Errorf("versions: %+v", status["versions"])
	}
	// Whether agents can start work comes from the Runner's provider availability.
	signIn := 0
	for id, k := range status {
		if strings.HasPrefix(id, "signin") {
			signIn++
			if id == "signin" {
				t.Errorf("could not ask the Runner for sign-in: %+v", k)
			}
		}
	}
	if signIn == 0 && status["claude"].Status == checkOK {
		t.Errorf("Claude Code runs but doctor says nothing of its sign-in: %+v", report.Checks)
	}
	if report.Healthy || got.code != exitFailure {
		t.Fatalf("a failing check reported healthy: %+v", got)
	}
	// The same, for a person.
	got = r.run("", false, "doctor")
	if !strings.Contains(got.stdout, "✗ Versions") || !strings.Contains(got.stdout, "→ `lazybox service install`") {
		t.Fatalf("%s", got.stdout)
	}
}

func TestLinkPutsTheCLIOnPathAndUninstallTakesItBack(t *testing.T) {
	r := newRelease(t, "0.1.0")
	bin := filepath.Join(r.home, "bin")
	r.path = "/usr/bin"
	got := r.run("", false, "link", "--dir", bin, "--shell", "fish")
	if got.code != 0 {
		t.Fatalf("%+v", got)
	}
	link := filepath.Join(bin, "lazybox")
	if target, err := os.Readlink(link); err != nil || target != r.runtime.CLI() {
		t.Fatalf("link %s → %s (%v)", link, target, err)
	}
	completion := filepath.Join(r.home, ".config", "fish", "completions", "lazybox.fish")
	if data, err := os.ReadFile(completion); err != nil || !strings.Contains(string(data), "complete -c lazybox") {
		t.Fatalf("completion %v", err)
	}
	if !strings.Contains(got.stdout, "is not on your PATH: add `fish_add_path "+bin+"`") {
		t.Fatalf("no PATH hint: %s", got.stdout)
	}
	// Linking again replaces our own link.
	if got = r.run("", false, "link", "--dir", bin, "--shell", "fish"); got.code != 0 {
		t.Fatalf("relink: %+v", got)
	}
	// Another lazybox (the V1 TUI, say) is never replaced silently.
	other := filepath.Join(r.home, "other")
	_ = os.MkdirAll(other, 0o755)
	_ = os.WriteFile(filepath.Join(other, "lazybox"), []byte("v1"), 0o755)
	if got = r.run("", false, "link", "--dir", other); got.code == 0 || !strings.Contains(got.err.Error(), "--force") {
		t.Fatalf("replaced a foreign lazybox: %+v", got)
	}

	// The service too, so uninstall has everything to remove.
	if got = r.run("", false, "service", "install"); got.code != 0 {
		t.Fatal(got)
	}
	_ = os.MkdirAll(filepath.Join(r.paths.Worktrees, "alpha"), 0o700)

	got = r.run("", false, "uninstall", "--dry-run")
	for _, want := range []string{"Would remove:", "Runner service", "CLI link", "shell completions", "CLI locator", "--remove-data"} {
		if !strings.Contains(got.stdout, want) {
			t.Errorf("dry run lacks %q:\n%s", want, got.stdout)
		}
	}
	if !r.loaded {
		t.Fatal("a dry run stopped the service")
	}
	for _, kept := range []string{link, completion, r.definition()} {
		if _, err := os.Lstat(kept); err != nil {
			t.Fatalf("a dry run removed %s", kept)
		}
	}
	got = r.run("", false, "uninstall", "--dry-run", "--remove-data", "--json")
	var plan struct {
		Remove []removal `json:"remove"`
	}
	if err := json.Unmarshal([]byte(got.stdout), &plan); err != nil {
		t.Fatal(err)
	}
	data := false
	for _, item := range plan.Remove {
		if item.Path == r.paths.Support {
			data = item.Data && strings.Contains(item.Detail, "1 workspace folder")
		}
	}
	if !data {
		t.Fatalf("the data removal does not warn about workspaces: %+v", plan.Remove)
	}
	// Removing data without a terminal to ask in needs --yes.
	if got = r.run("", false, "uninstall", "--remove-data"); got.code != exitUsage {
		t.Fatalf("%+v", got)
	}
	// From a terminal, anything but "delete" removes nothing.
	if got = r.run("no\n", true, "uninstall", "--remove-data"); got.code == 0 || !r.loaded {
		t.Fatalf("%+v", got)
	}

	got = r.run("", false, "uninstall")
	if got.code != 0 || r.loaded || !strings.Contains(got.stdout, "drag ") {
		t.Fatalf("uninstall: %+v", got)
	}
	for _, gone := range []string{link, completion, r.definition()} {
		if _, err := os.Lstat(gone); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("%s survived uninstall", gone)
		}
	}
	if _, err := os.Stat(r.paths.Support); err != nil {
		t.Fatal("uninstall without --remove-data removed the person's data")
	}
	if got = r.run("", false, "uninstall", "--remove-data", "--yes"); got.code != 0 {
		t.Fatalf("%+v", got)
	}
	if _, err := os.Stat(r.paths.Support); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("--remove-data kept the data")
	}
}

// A development build is not an installed release, and says what to use instead.
func TestServiceVerbsExplainADevelopmentBuild(t *testing.T) {
	var stdout, stderr bytes.Buffer
	c := &cli{ctx: context.Background(), stdin: strings.NewReader(""), stdout: &stdout, stderr: &stderr, getenv: func(k string) string {
		if k == "HOME" {
			return t.TempDir()
		}
		return ""
	}, now: time.Now}
	_, err := powerVerb(c, []string{"service", "status"})
	if exitCode(err) != exitUnavailable || !strings.Contains(err.Error(), "make dev") {
		t.Fatalf("%v", err)
	}
}
