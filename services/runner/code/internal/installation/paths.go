package installation

import (
	"os"
	"path/filepath"
	"regexp"
	"runtime"
)

// DefaultLabel is the launchd label of the installed Runner service, and the
// name of its plist in ~/Library/LaunchAgents. On Linux the service is the
// systemd user unit DefaultUnit.
const DefaultLabel = "ai.lazybox.runner"

// DefaultUnit is the installed Runner's systemd user unit on Linux.
const DefaultUnit = "lazybox-runner.service"

// LabelEnvironment selects another label, so a qualification run installs a
// uniquely named test service beside a real one (cmd/lazybox release tests).
const LabelEnvironment = "LAZYBOX_SERVICE_LABEL"

// AppIdentifier is the release app's bundle identifier
// (packaging/release.tauri.json): where macOS keeps its window state.
const AppIdentifier = "ai.lazybox.app"

// LocatorName is the name the installed Runner is registered under in
// ~/.config/lazybox/runners (`lazybox runners`, `--runner lazybox`), beside
// the Dev and Working launchers' "dev" and "working".
const LocatorName = "lazybox"

var labelPattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$`)

// Paths are the per-person locations of an installed release. Each one
// derives from HOME and the XDG base directories, so tests and qualification
// runs use a temporary home.
//
// On macOS (launchd):
//
//	Support    ~/Library/Application Support/Lazybox
//	Logs       ~/Library/Logs/Lazybox
//	Definition ~/Library/LaunchAgents/<label>.plist
//
// On Linux (systemd --user, or Lazybox's own supervisor where there is no
// systemd, as in a container):
//
//	Support    ${XDG_DATA_HOME:-~/.local/share}/lazybox
//	Logs       ${XDG_STATE_HOME:-~/.local/state}/lazybox (the journal, under systemd)
//	Definition ${XDG_CONFIG_HOME:-~/.config}/systemd/user/lazybox-runner.service
//
// State, the configuration, the worktrees and the restart request live under
// Support on both, so the rest of Lazybox never asks which platform it is on.
type Paths struct {
	Home string
	// Label names the service: the launchd job label, and on Linux the
	// systemd unit derived from it (Unit).
	Label string
	// Support holds everything the Runner owns.
	Support string
	// State is the Runner's state directory (sessions, journals, endpoint.json).
	State string
	// Config is the Runner's private configuration (runner.config.json).
	Config string
	// Worktrees is where workspaces' worktrees are created.
	Worktrees string
	// Logs holds the service's output where it goes to a file (launchd, and
	// Lazybox's own supervisor); under systemd the journal has it.
	Logs string
	// Definition is the service definition the system's service manager
	// reads: the launchd plist, or the systemd user unit. Lazybox's own
	// supervisor keeps its record beside the state instead (Supervisor).
	Definition string
	// RestartRequest is where Restart Runner records its request (internal/restart);
	// the service manager, which keeps the Runner alive, is the supervisor
	// that keeps the promise.
	RestartRequest string
	// ConfigDir is ~/.config/lazybox: actions.yaml and the Runner locators,
	// shared with Dev and Working (POWER.md).
	ConfigDir string
	// OS is the platform the paths are for: "darwin" or "linux".
	OS string
}

// Unit is the systemd unit name of the service: lazybox-runner.service for
// the real one, <label>.service for a test service.
func (p Paths) Unit() string {
	if p.Label == DefaultLabel {
		return DefaultUnit
	}
	return p.Label + ".service"
}

// Endpoint is the owner's endpoint file the Runner writes on every start.
func (p Paths) Endpoint() string { return filepath.Join(p.State, "endpoint.json") }

// Log is the file the service's output is appended to, where it goes to a file.
func (p Paths) Log() string { return filepath.Join(p.Logs, "runner.log") }

// Locator is the file `lazybox` reads to find this Runner from any terminal.
func (p Paths) Locator() string { return filepath.Join(p.ConfigDir, "runners", p.RunnerName()+".json") }

// RunnerName is the name the CLI knows this Runner by: "lazybox", or a test
// service's own label.
func (p Paths) RunnerName() string {
	if p.Label == DefaultLabel {
		return LocatorName
	}
	return p.Label
}

// ActionsFile is the person's actions and hooks file, the one Dev and Working use too.
func (p Paths) ActionsFile() string { return filepath.Join(p.ConfigDir, "actions.yaml") }

// UserPaths resolves this platform's paths from an environment (os.Getenv in production).
func UserPaths(getenv func(string) string) (Paths, error) {
	return PathsFor(runtime.GOOS, getenv)
}

// PathsFor resolves the paths an installed release uses on goos, so either
// layout can be checked on any machine.
func PathsFor(goos string, getenv func(string) string) (Paths, error) {
	home := getenv("HOME")
	if home == "" {
		var err error
		if home, err = os.UserHomeDir(); err != nil {
			return Paths{}, err
		}
	}
	label := getenv(LabelEnvironment)
	if label == "" {
		label = DefaultLabel
	}
	if !labelPattern.MatchString(label) {
		return Paths{}, &os.PathError{Op: "service label", Path: label, Err: os.ErrInvalid}
	}
	// xdg is an XDG base directory: the variable when it is absolute, as the
	// specification requires, otherwise its default under HOME.
	xdg := func(variable string, fallback ...string) string {
		if base := getenv(variable); base != "" && filepath.IsAbs(base) {
			return base
		}
		return filepath.Join(append([]string{home}, fallback...)...)
	}
	configHome := xdg("XDG_CONFIG_HOME", ".config")
	p := Paths{Home: home, Label: label, ConfigDir: filepath.Join(configHome, "lazybox"), OS: goos}
	switch goos {
	case "linux":
		p.Support = filepath.Join(xdg("XDG_DATA_HOME", ".local", "share"), "lazybox")
		p.Logs = filepath.Join(xdg("XDG_STATE_HOME", ".local", "state"), "lazybox")
		p.Definition = filepath.Join(configHome, "systemd", "user", p.Unit())
	default:
		p.Support = filepath.Join(home, "Library", "Application Support", "Lazybox")
		p.Logs = filepath.Join(home, "Library", "Logs", "Lazybox")
		p.Definition = filepath.Join(home, "Library", "LaunchAgents", label+".plist")
	}
	if label != DefaultLabel {
		// A test service never shares state, logs or a locator with the real one.
		p.Support = filepath.Join(p.Support, "services", label)
		p.Logs = filepath.Join(p.Logs, label)
	}
	p.State = filepath.Join(p.Support, "state")
	p.Config = filepath.Join(p.Support, "runner.config.json")
	p.Worktrees = filepath.Join(p.Support, "workspaces")
	p.RestartRequest = filepath.Join(p.Support, "restart-request.json")
	return p, nil
}
