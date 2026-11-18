package installation

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"testing"
)

// fakeRuntime lays out a release runtime with stand-in executables.
func fakeRuntime(t *testing.T, root, version string) Layout {
	t.Helper()
	l := Layout{Root: root}
	for _, exe := range []string{l.CLI(), l.Runner(), l.Node()} {
		if err := os.MkdirAll(filepath.Dir(exe), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(exe, []byte("#!/bin/sh\necho v0\n"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.MkdirAll(filepath.Dir(l.Worker()), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(l.Worker(), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(l.Version(), []byte(version+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	return l
}

func env(values map[string]string) func(string) string {
	return func(k string) string { return values[k] }
}

// fakeLaunchd is launchd for one job, recording what it was asked.
type fakeLaunchd struct {
	loaded bool
	calls  []string
	loads  []string
}

func (f *fakeLaunchd) launchd() Launchd {
	return Launchd{Domain: "gui/501", Run: func(_ context.Context, args ...string) ([]byte, error) {
		f.calls = append(f.calls, args[0])
		switch args[0] {
		case "print":
			if !f.loaded {
				return []byte("Bad request.\nCould not find service \"x\" in domain for user gui: 501\n"), errors.New("exit status 113")
			}
			return []byte("gui/501/ai.lazybox.runner = {\n\tstate = running\n\tpid = 4242\n\tlast exit code = (never exited)\n}\n"), nil
		case "bootstrap":
			if f.loaded {
				return []byte("Bootstrap failed: 37: Operation already in progress"), errors.New("exit status 37")
			}
			f.loaded = true
			f.loads = append(f.loads, args[2])
		case "bootout":
			if !f.loaded {
				return []byte("Boot-out failed: 3: No such process"), errors.New("exit status 3")
			}
			f.loaded = false
		}
		return nil, nil
	}}
}

func TestPlistIsAValidLaunchdJobThatKeepsTheRunnerAlive(t *testing.T) {
	macOS(t)
	s := Service{Label: "ai.lazybox.runner", Program: "/Applications/Lazybox.app/Contents/Resources/runtime/bin/lazybox-runner",
		Arguments:   []string{"serve", "--supervised", "--state", "/Users/me/Library/Application Support/Lazybox/state"},
		Environment: map[string]string{"PATH": "/opt/homebrew/bin:/usr/bin:/bin", "LANG": "en_US.UTF-8", "ODD": `a&b<c>"d'`},
		Log:         "/Users/me/Library/Logs/Lazybox/runner.log", WorkingDirectory: "/tmp", Version: "0.1.0"}
	data, err := s.Plist()
	if err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(t.TempDir(), "job.plist")
	if err = os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
	if out, err := exec.Command("/usr/bin/plutil", "-lint", path).CombinedOutput(); err != nil {
		t.Fatalf("plutil rejects the plist: %v %s\n%s", err, out, data)
	}
	out, err := exec.Command("/usr/bin/plutil", "-convert", "json", "-o", "-", path).Output()
	if err != nil {
		t.Fatal(err)
	}
	var doc map[string]any
	if err = json.Unmarshal(out, &doc); err != nil {
		t.Fatal(err)
	}
	if doc["RunAtLoad"] != true || doc["KeepAlive"] != true || doc["Label"] != "ai.lazybox.runner" || doc["StandardErrorPath"] != s.Log {
		t.Fatalf("job: %v", doc)
	}
	// AbandonProcessGroup is never set: what the Runner runs directly dies with it.
	if _, ok := doc["AbandonProcessGroup"]; ok {
		t.Fatal("the job abandons its process group")
	}
	installed, err := ReadPlist(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	if installed.Program != s.Program || installed.Version != "0.1.0" || installed.Env["ODD"] != `a&b<c>"d'` || installed.Env["PATH"] != s.Environment["PATH"] {
		t.Fatalf("read back %+v", installed)
	}
	if _, err = (Service{Label: "bad label", Program: "/x"}).Plist(); err == nil {
		t.Fatal("an invalid label was accepted")
	}
	if _, err = (Service{Label: "ok", Program: "relative/runner"}).Plist(); err == nil {
		t.Fatal("a relative program was accepted")
	}
}

// macOS skips a test that needs macOS's own tools (plutil, launchctl).
func macOS(t *testing.T) {
	t.Helper()
	if runtime.GOOS != "darwin" {
		t.Skip("needs macOS (plutil)")
	}
}

func TestUserPathsFollowHomeAndKeepATestServiceApart(t *testing.T) {
	p, err := PathsFor("darwin", env(map[string]string{"HOME": "/Users/me"}))
	if err != nil {
		t.Fatal(err)
	}
	if p.State != "/Users/me/Library/Application Support/Lazybox/state" || p.Definition != "/Users/me/Library/LaunchAgents/ai.lazybox.runner.plist" ||
		p.Endpoint() != "/Users/me/Library/Application Support/Lazybox/state/endpoint.json" || p.Log() != "/Users/me/Library/Logs/Lazybox/runner.log" ||
		p.Locator() != "/Users/me/.config/lazybox/runners/lazybox.json" || p.ActionsFile() != "/Users/me/.config/lazybox/actions.yaml" {
		t.Fatalf("%+v", p)
	}
	q, err := PathsFor("darwin", env(map[string]string{"HOME": "/Users/me", LabelEnvironment: "ai.lazybox.runner.test-1", "XDG_CONFIG_HOME": "/cfg"}))
	if err != nil {
		t.Fatal(err)
	}
	if q.State == p.State || q.Logs == p.Logs || q.Definition == p.Definition || q.Locator() != "/cfg/lazybox/runners/ai.lazybox.runner.test-1.json" {
		t.Fatalf("a test service shares the real one's paths: %+v", q)
	}
	if _, err = UserPaths(env(map[string]string{"HOME": "/Users/me", LabelEnvironment: "../escape"})); err == nil {
		t.Fatal("a label that is a path was accepted")
	}
}

// On Linux everything follows the XDG base directories, and the service is
// a systemd user unit named after the product rather than the label.
func TestLinuxPathsFollowTheXDGBaseDirectories(t *testing.T) {
	p, err := PathsFor("linux", env(map[string]string{"HOME": "/home/me"}))
	if err != nil {
		t.Fatal(err)
	}
	if p.Support != "/home/me/.local/share/lazybox" || p.State != "/home/me/.local/share/lazybox/state" || p.Config != "/home/me/.local/share/lazybox/runner.config.json" ||
		p.Logs != "/home/me/.local/state/lazybox" || p.Definition != "/home/me/.config/systemd/user/lazybox-runner.service" || p.Unit() != "lazybox-runner.service" ||
		p.Locator() != "/home/me/.config/lazybox/runners/lazybox.json" || p.Endpoint() != "/home/me/.local/share/lazybox/state/endpoint.json" {
		t.Fatalf("%+v", p)
	}
	q, err := PathsFor("linux", env(map[string]string{"HOME": "/home/me", "XDG_DATA_HOME": "/data", "XDG_STATE_HOME": "/st", "XDG_CONFIG_HOME": "/cfg", LabelEnvironment: "ai.lazybox.runner.test-1"}))
	if err != nil {
		t.Fatal(err)
	}
	if q.Support != "/data/lazybox/services/ai.lazybox.runner.test-1" || q.Logs != "/st/lazybox/ai.lazybox.runner.test-1" || q.Definition != "/cfg/systemd/user/ai.lazybox.runner.test-1.service" || q.ConfigDir != "/cfg/lazybox" {
		t.Fatalf("%+v", q)
	}
	// A relative XDG directory is ignored, as the specification says.
	if r, _ := PathsFor("linux", env(map[string]string{"HOME": "/home/me", "XDG_DATA_HOME": "data"})); r.Support != p.Support {
		t.Fatalf("a relative XDG_DATA_HOME was used: %s", r.Support)
	}
}

func TestInstallUpdatesAndUninstallTheService(t *testing.T) {
	macOS(t)
	home := t.TempDir()
	paths, err := UserPaths(env(map[string]string{"HOME": home}))
	if err != nil {
		t.Fatal(err)
	}
	fake := &fakeLaunchd{}
	in := Installer{Paths: paths, Layout: fakeRuntime(t, filepath.Join(t.TempDir(), "runtime"), "0.1.0"), Manager: fake.launchd(),
		Shell: func(context.Context) ShellEnvironment {
			return ShellEnvironment{Path: "/Users/me/.cargo/bin:/usr/bin", Lang: "fr_FR.UTF-8"}
		}}
	ctx := context.Background()

	first, err := in.Install(ctx, true)
	if err != nil {
		t.Fatal(err)
	}
	if !first.Started || first.Restarted || first.Previous != nil || !fake.loaded || !slices.Equal(fake.loads, []string{paths.Definition}) {
		t.Fatalf("first install: %+v, launchd %+v", first, fake)
	}
	installed, err := ReadPlist(ctx, paths.Definition)
	if err != nil {
		t.Fatal(err)
	}
	if installed.Program != in.Layout.Runner() || installed.Version != "0.1.0" || installed.Env["LANG"] != "fr_FR.UTF-8" || !strings.HasPrefix(installed.Env["PATH"], "/Users/me/.cargo/bin:/usr/bin:") {
		t.Fatalf("installed %+v", installed)
	}
	var locator Locator
	data, err := os.ReadFile(paths.Locator())
	if err != nil {
		t.Fatal(err)
	}
	if err = json.Unmarshal(data, &locator); err != nil || locator.Endpoint != paths.Endpoint() || locator.Name != "lazybox" {
		t.Fatalf("locator %s: %v", data, err)
	}
	// The person's configuration is created once and never replaced.
	if err = os.WriteFile(paths.Config, []byte(`{"profiles":[{"id":"mine"}]}`), 0o600); err != nil {
		t.Fatal(err)
	}

	// An update: a new release in another place, the running job replaced.
	in.Layout = fakeRuntime(t, filepath.Join(t.TempDir(), "runtime"), "0.2.0")
	fake.calls = nil
	second, err := in.Install(ctx, true)
	if err != nil {
		t.Fatal(err)
	}
	if !second.Restarted || second.Previous == nil || second.Previous.Version != "0.1.0" || !slices.Equal(fake.calls, []string{"print", "bootout", "bootstrap"}) {
		t.Fatalf("update: %+v, calls %v", second, fake.calls)
	}
	if installed, _ = ReadPlist(ctx, paths.Definition); installed.Program != in.Layout.Runner() || installed.Version != "0.2.0" {
		t.Fatalf("the update did not point the job at the new release: %+v", installed)
	}
	if data, _ = os.ReadFile(paths.Config); !strings.Contains(string(data), "mine") {
		t.Fatal("the update replaced the person's configuration")
	}

	status, err := in.Status(ctx)
	if err != nil || !status.Installed || !status.Running || status.PID != 4242 || status.Version != "0.2.0" {
		t.Fatalf("status %+v %v", status, err)
	}

	removed, err := in.Uninstall(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if fake.loaded || len(removed) != 2 {
		t.Fatalf("uninstall left %v loaded=%v", removed, fake.loaded)
	}
	for _, gone := range []string{paths.Definition, paths.Locator()} {
		if _, err = os.Stat(gone); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("%s survived uninstall", gone)
		}
	}
	if _, err = os.Stat(paths.Config); err != nil {
		t.Fatal("uninstall removed the person's configuration")
	}
	// Uninstalling again is not an error.
	if _, err = in.Uninstall(ctx); err != nil {
		t.Fatal(err)
	}
}

func TestInstallWithoutStartDoesNotLoad(t *testing.T) {
	paths, _ := UserPaths(env(map[string]string{"HOME": t.TempDir()}))
	fake := &fakeLaunchd{}
	in := Installer{Paths: paths, Layout: fakeRuntime(t, filepath.Join(t.TempDir(), "runtime"), "1.0.0"), Manager: fake.launchd()}
	result, err := in.Install(context.Background(), false)
	if err != nil || result.Started || fake.loaded {
		t.Fatalf("%+v %v loaded=%v", result, err, fake.loaded)
	}
}

func TestATranslocatedOrIncompleteReleaseIsRefused(t *testing.T) {
	paths, _ := UserPaths(env(map[string]string{"HOME": t.TempDir()}))
	root := filepath.Join(t.TempDir(), "private", "var", "folders", "AppTranslocation", "X", "d", "Lazybox.app", "Contents", "Resources", "runtime")
	in := Installer{Paths: paths, Layout: fakeRuntime(t, root, "1.0.0"), Manager: (&fakeLaunchd{}).launchd()}
	if _, err := in.Install(context.Background(), true); err == nil || !strings.Contains(err.Error(), "Applications") {
		t.Fatalf("a translocated app installed a service: %v", err)
	}
	if got := in.Layout.AppBundle(); !strings.HasSuffix(got, "Lazybox.app") {
		t.Fatalf("app bundle %q", got)
	}
	broken := fakeRuntime(t, filepath.Join(t.TempDir(), "runtime"), "1.0.0")
	if err := os.Remove(broken.Node()); err != nil {
		t.Fatal(err)
	}
	in.Layout = broken
	if _, err := in.Install(context.Background(), true); err == nil || !strings.Contains(err.Error(), "incomplete") {
		t.Fatalf("an incomplete runtime installed: %v", err)
	}
}

func TestFromExecutableFollowsTheLinkOnPath(t *testing.T) {
	l := fakeRuntime(t, filepath.Join(t.TempDir(), "runtime"), "1.0.0")
	link := filepath.Join(t.TempDir(), "lazybox")
	if err := os.Symlink(l.CLI(), link); err != nil {
		t.Fatal(err)
	}
	got, err := FromExecutable(link)
	if err != nil {
		t.Fatal(err)
	}
	if resolved, _ := filepath.EvalSymlinks(l.Root); got.Root != resolved {
		t.Fatalf("root %s, want %s", got.Root, resolved)
	}
	stray := filepath.Join(t.TempDir(), "bin", "lazybox")
	_ = os.MkdirAll(filepath.Dir(stray), 0o755)
	_ = os.WriteFile(stray, nil, 0o755)
	if _, err = FromExecutable(stray); !errors.Is(err, ErrNotInstalled) {
		t.Fatalf("a development build counted as installed: %v", err)
	}
}

func TestServicePathKeepsTheShellsOrderAndAddsTheUsualDirectories(t *testing.T) {
	got := ServicePath("/Users/me/.nvm/bin:relative:/usr/bin::/Users/me/.nvm/bin", "/Users/me")
	want := "/Users/me/.nvm/bin:/usr/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/bin:/usr/sbin:/sbin:/Users/me/.local/bin"
	if runtime.GOOS == "linux" {
		want = "/Users/me/.nvm/bin:/usr/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/sbin:/bin:/Users/me/.local/bin"
	}
	if got != want {
		t.Fatalf("got  %s\nwant %s", got, want)
	}
}

func TestReadShellEnvironmentReadsBetweenMarkers(t *testing.T) {
	shell := filepath.Join(t.TempDir(), "noisy-shell")
	// A login shell that prints a banner before running the command.
	script := "#!/bin/sh\necho 'Welcome!'\nshift 3\nPATH=/custom/bin:/usr/bin LANG=de_DE.UTF-8 /bin/sh -c \"$1\"\n"
	if err := os.WriteFile(shell, []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	got := ReadShellEnvironment(context.Background(), env(map[string]string{"SHELL": shell}))
	if got.Path != "/custom/bin:/usr/bin" || got.Lang != "de_DE.UTF-8" || got.Problem != "" {
		t.Fatalf("%+v", got)
	}
	failing := filepath.Join(t.TempDir(), "broken-shell")
	_ = os.WriteFile(failing, []byte("#!/bin/sh\nexit 3\n"), 0o755)
	if got = ReadShellEnvironment(context.Background(), env(map[string]string{"SHELL": failing})); got.Problem == "" || got.Path != "" {
		t.Fatalf("a failing shell: %+v", got)
	}
}

func TestPrintReadsLaunchdsView(t *testing.T) {
	fake := &fakeLaunchd{}
	l := fake.launchd()
	job, err := l.Print(context.Background(), "ai.lazybox.runner")
	if err != nil || job.Loaded {
		t.Fatalf("%+v %v", job, err)
	}
	fake.loaded = true
	if job, err = l.Print(context.Background(), "ai.lazybox.runner"); err != nil || !job.Running || job.PID != 4242 {
		t.Fatalf("%+v %v", job, err)
	}
}

// The identifier the uninstaller cleans up after is the one the release builds.
func TestAppIdentifierIsTheReleaseApps(t *testing.T) {
	data, err := os.ReadFile(filepath.Join("..", "..", "..", "..", "..", "packaging", "release.tauri.json"))
	if err != nil {
		t.Fatal(err)
	}
	var config struct {
		Identifier string `json:"identifier"`
	}
	if err = json.Unmarshal(data, &config); err != nil || config.Identifier != AppIdentifier {
		t.Fatalf("release.tauri.json identifier %q, want %q (%v)", config.Identifier, AppIdentifier, err)
	}
}
