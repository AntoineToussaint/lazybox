package installation

import (
	"os"
	"path/filepath"
	"regexp"
)

// DefaultLabel is the launchd label of the installed Runner service, and the
// name of its plist in ~/Library/LaunchAgents.
const DefaultLabel = "ai.lazybox.runner"

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
// derives from HOME (and XDG_CONFIG_HOME for the configuration shared with
// Dev and Working), so tests and qualification runs use a temporary home.
type Paths struct {
	Home string
	// Label is the launchd job label.
	Label string
	// Support holds everything the Runner owns: ~/Library/Application Support/Lazybox.
	Support string
	// State is the Runner's state directory (sessions, journals, endpoint.json).
	State string
	// Config is the Runner's private configuration (runner.config.json).
	Config string
	// Worktrees is where workspaces' git worktrees are created.
	Worktrees string
	// Logs holds the service's output: ~/Library/Logs/Lazybox.
	Logs string
	// Plist is the launchd job definition.
	Plist string
	// RestartRequest is where Restart Runner records its request (internal/restart);
	// launchd, which keeps the job alive, is the supervisor that keeps the promise.
	RestartRequest string
	// ConfigDir is ~/.config/lazybox: actions.yaml and the Runner locators,
	// shared with Dev and Working (POWER.md).
	ConfigDir string
}

// Endpoint is the owner's endpoint file the Runner writes on every start.
func (p Paths) Endpoint() string { return filepath.Join(p.State, "endpoint.json") }

// Log is the file launchd appends the Runner's output to.
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

// UserPaths resolves the paths from an environment (os.Getenv in production).
func UserPaths(getenv func(string) string) (Paths, error) {
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
	config := filepath.Join(home, ".config", "lazybox")
	if base := getenv("XDG_CONFIG_HOME"); base != "" && filepath.IsAbs(base) {
		config = filepath.Join(base, "lazybox")
	}
	support := filepath.Join(home, "Library", "Application Support", "Lazybox")
	logs := filepath.Join(home, "Library", "Logs", "Lazybox")
	if label != DefaultLabel {
		// A test service never shares state, logs or a locator with the real one.
		support = filepath.Join(support, "services", label)
		logs = filepath.Join(logs, label)
	}
	return Paths{
		Home:           home,
		Label:          label,
		Support:        support,
		State:          filepath.Join(support, "state"),
		Config:         filepath.Join(support, "runner.config.json"),
		Worktrees:      filepath.Join(support, "workspaces"),
		Logs:           logs,
		Plist:          filepath.Join(home, "Library", "LaunchAgents", label+".plist"),
		RestartRequest: filepath.Join(support, "restart-request.json"),
		ConfigDir:      config,
	}, nil
}
