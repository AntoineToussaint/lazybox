package installation

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func TestUnitKeepsTheRunnerAliveAndLeavesItsHostsAlone(t *testing.T) {
	s := Service{Label: DefaultLabel, Program: "/home/me/.local/share/lazybox/runner/versions/0.1.0/bin/lazybox-runner",
		Arguments:        []string{"serve", "--supervised", "--state", "/home/me/odd dir/100%/state"},
		Environment:      map[string]string{"PATH": "/home/me/.cargo/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "ODD": `a "quoted" $HOME 50% \ back`},
		WorkingDirectory: "/home/me/.local/share/lazybox", Version: "0.1.0"}
	data, err := s.Unit()
	if err != nil {
		t.Fatal(err)
	}
	unit := string(data)
	for _, want := range []string{"Restart=always", "KillMode=process", "StartLimitIntervalSec=0", "WantedBy=default.target", "TimeoutStopSec=30"} {
		if !strings.Contains(unit, "\n"+want+"\n") {
			t.Errorf("the unit lacks %s:\n%s", want, unit)
		}
	}
	// Specifiers and variables in arguments are escaped, so systemd passes them as written.
	if !strings.Contains(unit, `"/home/me/odd dir/100%%/state"`) {
		t.Errorf("an argument with a space and a %% is not quoted:\n%s", unit)
	}
	path := filepath.Join(t.TempDir(), "lazybox-runner.service")
	if err = os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
	installed, err := ReadUnit(path)
	if err != nil {
		t.Fatal(err)
	}
	if installed.Program != s.Program || installed.Version != "0.1.0" || installed.Env["ODD"] != s.Environment["ODD"] || installed.Env["PATH"] != s.Environment["PATH"] || installed.Env["LANG"] != "C.UTF-8" {
		t.Fatalf("read back %+v", installed)
	}
	if _, err = (Service{Label: "bad label", Program: "/x"}).Unit(); err == nil {
		t.Fatal("an invalid label was accepted")
	}
	if _, err = (Service{Label: "ok", Program: "relative/runner"}).Unit(); err == nil {
		t.Fatal("a relative program was accepted")
	}
	// systemd's own parser accepts it, where there is one.
	analyze, err := exec.LookPath("systemd-analyze")
	if err != nil {
		return
	}
	s.Program, s.WorkingDirectory = "/bin/true", t.TempDir()
	if data, err = s.Unit(); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
	if out, err := exec.Command(analyze, "verify", path).CombinedOutput(); err != nil {
		t.Fatalf("systemd-analyze rejects the unit: %v\n%s\n%s", err, out, data)
	}
	t.Logf("systemd-analyze verify: ok")
}

// fakeSystemd is systemctl --user for one unit, recording what it was asked.
type fakeSystemd struct {
	active, enabled, linger bool
	calls                   []string
}

func (f *fakeSystemd) systemd() Systemd {
	return Systemd{User: "me", Run: func(_ context.Context, program string, args ...string) ([]byte, error) {
		if program == "loginctl" {
			if f.linger {
				return []byte("Linger=yes\n"), nil
			}
			return []byte("Linger=no\n"), nil
		}
		f.calls = append(f.calls, strings.Join(args[1:], " "))
		switch args[1] {
		case "show":
			state, pid, file := "inactive", "0", "disabled"
			if f.active {
				state, pid = "active", "4242"
			}
			if f.enabled {
				file = "enabled"
			}
			return []byte("LoadState=loaded\nActiveState=" + state + "\nSubState=running\nMainPID=" + pid + "\nExecMainCode=0\nExecMainStatus=0\nUnitFileState=" + file + "\n"), nil
		case "enable":
			f.enabled, f.active = true, true
		case "stop":
			f.active = false
		case "restart":
			f.active = true
		case "disable":
			f.enabled, f.active = false, false
		}
		return nil, nil
	}}
}

func TestSystemdInstallUpdatesAndUninstallTheUnit(t *testing.T) {
	paths, err := PathsFor("linux", env(map[string]string{"HOME": t.TempDir()}))
	if err != nil {
		t.Fatal(err)
	}
	fake := &fakeSystemd{}
	in := Installer{Paths: paths, Layout: fakeRuntime(t, filepath.Join(t.TempDir(), "runtime"), "0.1.0"), Manager: fake.systemd(),
		Shell: func(context.Context) ShellEnvironment { return ShellEnvironment{Path: "/home/me/.cargo/bin:/usr/bin"} }}
	ctx := context.Background()
	first, err := in.Install(ctx, true)
	if err != nil {
		t.Fatal(err)
	}
	if !first.Started || first.Restarted || !fake.active || !fake.enabled || !slices.Equal(fake.calls, []string{"show lazybox-runner.service --property=LoadState,ActiveState,SubState,MainPID,ExecMainCode,ExecMainStatus,UnitFileState", "daemon-reload", "enable --now lazybox-runner.service"}) {
		t.Fatalf("first install: %+v, calls %q", first, fake.calls)
	}
	installed, err := ReadUnit(paths.Definition)
	if err != nil || installed.Program != in.Layout.Runner() || installed.Version != "0.1.0" || !strings.HasPrefix(installed.Env["PATH"], "/home/me/.cargo/bin:/usr/bin:") {
		t.Fatalf("installed %+v %v", installed, err)
	}
	status, err := in.Status(ctx)
	if err != nil || !status.Running || status.PID != 4242 || status.Manager != ManagerSystemd || status.Unit != "lazybox-runner.service" || status.Plist != "" || !strings.Contains(status.StartsWhen, "enable-linger me") || !strings.HasPrefix(status.Log, "journalctl --user --unit lazybox-runner.service") {
		t.Fatalf("status %+v %v", status, err)
	}
	fake.linger = true
	if status, _ = in.Status(ctx); status.StartsWhen != "starts at boot (lingering is on)" {
		t.Fatalf("with lingering: %q", status.StartsWhen)
	}

	// An update stops the old Runner and starts the new one from the rewritten unit.
	in.Layout = fakeRuntime(t, filepath.Join(t.TempDir(), "runtime"), "0.2.0")
	fake.calls = nil
	second, err := in.Install(ctx, true)
	if err != nil {
		t.Fatal(err)
	}
	if !second.Restarted || second.Previous == nil || second.Previous.Version != "0.1.0" || !slices.Equal(fake.calls[1:], []string{"stop lazybox-runner.service", "daemon-reload", "enable --now lazybox-runner.service"}) {
		t.Fatalf("update: %+v, calls %q", second, fake.calls)
	}
	if err = in.Restart(ctx); err != nil || fake.calls[len(fake.calls)-1] != "restart lazybox-runner.service" {
		t.Fatalf("restart: %v %q", err, fake.calls)
	}

	removed, err := in.Uninstall(ctx)
	if err != nil || fake.active || fake.enabled || len(removed) != 2 || fake.calls[len(fake.calls)-1] != "daemon-reload" {
		t.Fatalf("uninstall: %v %v, calls %q", removed, err, fake.calls)
	}
	if _, err = os.Stat(paths.Definition); !os.IsNotExist(err) {
		t.Fatal("the unit survived uninstall")
	}
}

func TestAnUnreachableUserManagerSaysWhatToDo(t *testing.T) {
	s := Systemd{User: "me", Run: func(context.Context, string, ...string) ([]byte, error) {
		return []byte("Failed to connect to bus: No medium found"), &exec.ExitError{}
	}}
	paths, _ := PathsFor("linux", env(map[string]string{"HOME": t.TempDir()}))
	_, err := s.Status(context.Background(), paths)
	if err == nil || !strings.Contains(err.Error(), "loginctl enable-linger me") || !strings.Contains(err.Error(), ManagerEnvironment+"=supervisor") {
		t.Fatalf("%v", err)
	}
}

// logind knows a user only while they have a session or linger, so a
// Runner's account with neither is "not lingering", not an error.
func TestAUserLogindDoesNotKnowIsNotLingering(t *testing.T) {
	s := Systemd{User: "me", Run: func(context.Context, string, ...string) ([]byte, error) {
		return []byte("Failed to get user: User ID 501 is not logged in or lingering\n"), &exec.ExitError{}
	}}
	if linger, err := s.Linger(context.Background()); err != nil || linger {
		t.Fatalf("%v %v", linger, err)
	}
}

func TestTheManagerFollowsThePlatform(t *testing.T) {
	booted := func(path string) bool { return path == "/run/systemd/system" }
	none := func(string) bool { return false }
	for _, c := range []struct {
		goos   string
		exists func(string) bool
		want   string
	}{{"darwin", none, ManagerLaunchd}, {"linux", booted, ManagerSystemd}, {"linux", none, ManagerSupervisor}} {
		if got := defaultManager(c.goos, c.exists); got != c.want {
			t.Errorf("%s: %s, want %s", c.goos, got, c.want)
		}
	}
	if m, err := SystemManager(env(map[string]string{ManagerEnvironment: "supervisor"})); err != nil || m.Kind() != ManagerSupervisor {
		t.Fatalf("%v %v", m, err)
	}
	if _, err := SystemManager(env(map[string]string{ManagerEnvironment: "upstart"})); err == nil {
		t.Fatal("an unknown manager was accepted")
	}
}
