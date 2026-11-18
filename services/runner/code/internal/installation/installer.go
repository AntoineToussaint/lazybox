package installation

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

// Installer installs, updates and removes the Runner service of one runtime
// for one person.
type Installer struct {
	Paths  Paths
	Layout Layout
	// Manager runs the service: launchd, systemd or Lazybox's own supervisor.
	Manager Manager
	// Shell reads the login shell's environment (ReadShellEnvironment).
	Shell func(context.Context) ShellEnvironment
}

// Definition is the file the service's definition is written to.
func (i Installer) Definition() string { return i.Manager.DefinitionPath(i.Paths) }

// ServeArguments are the Runner's arguments under its service manager: serve
// mode, as a supervised service (Restart Runner is available), with this
// person's paths.
func (i Installer) ServeArguments() []string {
	return []string{"serve", "--supervised",
		"--state", i.Paths.State,
		"--config", i.Paths.Config,
		"--worktrees", i.Paths.Worktrees,
		"--actions", i.Paths.ActionsFile(),
		"--restart-request", i.Paths.RestartRequest,
	}
}

// Plan is the service the installer would write, with the shell environment it read.
func (i Installer) Plan(ctx context.Context) (Service, ShellEnvironment, error) {
	if err := i.Layout.Check(); err != nil {
		return Service{}, ShellEnvironment{}, err
	}
	release, err := i.Layout.ReadVersion()
	if err != nil {
		return Service{}, ShellEnvironment{}, err
	}
	var shell ShellEnvironment
	if i.Shell != nil {
		shell = i.Shell(ctx)
	}
	env := map[string]string{"PATH": ServicePath(shell.Path, i.Paths.Home)}
	if shell.Lang != "" {
		env["LANG"] = shell.Lang
	}
	return Service{
		Label:            i.Paths.Label,
		Program:          i.Layout.Runner(),
		Arguments:        i.ServeArguments(),
		Environment:      env,
		WorkingDirectory: i.Paths.Support,
		Log:              i.Paths.Log(),
		Version:          release,
	}, shell, nil
}

// InstallResult says what Install changed.
type InstallResult struct {
	Service Service
	Shell   ShellEnvironment
	// Previous is the service that was installed before, if any.
	Previous *Installed
	// Restarted: a running Runner was stopped and the new one started. Its
	// agents, commands and terminals kept running and are re-attached.
	Restarted bool
	Started   bool
}

// Install writes the service definition, replacing an installed one, and
// (with start) starts it. Replacing a running service stops the old Runner
// first: it detaches its agents, which the new Runner re-attaches. Nothing
// else is touched: state, credentials and the person's configuration stay
// where they are.
func (i Installer) Install(ctx context.Context, start bool) (InstallResult, error) {
	if reason := i.Layout.Unstable(); reason != "" {
		return InstallResult{}, errors.New(reason)
	}
	service, shell, err := i.Plan(ctx)
	if err != nil {
		return InstallResult{}, err
	}
	result := InstallResult{Service: service, Shell: shell}
	for _, dir := range []string{i.Paths.Support, i.Paths.State, i.Paths.Logs} {
		if err = os.MkdirAll(dir, 0o700); err != nil {
			return result, err
		}
	}
	definition := i.Definition()
	if err = os.MkdirAll(filepath.Dir(definition), 0o755); err != nil {
		return result, err
	}
	// The private Runner configuration (RUNNING.md): created once, never replaced.
	if err = writeNew(i.Paths.Config, []byte("{\n  \"profiles\": []\n}\n"), 0o600); err != nil && !errors.Is(err, os.ErrExist) {
		return result, err
	}
	if previous, err := i.Manager.ReadDefinition(ctx, definition); err == nil {
		result.Previous = &previous
	}
	data, err := i.Manager.Render(service)
	if err != nil {
		return result, err
	}
	job, err := i.Manager.Status(ctx, i.Paths)
	if err != nil {
		return result, err
	}
	if job.Loaded {
		// A manager reads the definition when it loads it: an updated
		// service takes effect only through a stop and a start.
		if err = i.Manager.Deactivate(ctx, i.Paths); err != nil {
			return result, err
		}
		result.Restarted = job.Running
	}
	if err = writeAtomic(definition, data, 0o644); err != nil {
		return result, err
	}
	if err = i.WriteLocator(); err != nil {
		return result, err
	}
	if start || job.Loaded {
		if err = i.Manager.Activate(ctx, i.Paths); err != nil {
			return result, err
		}
		result.Started = true
	}
	return result, nil
}

// Locator is the record `lazybox` reads to find the installed Runner
// (cmd/lazybox endpoint.go; the Dev and Working launchers write theirs).
type Locator struct {
	Name     string `json:"name"`
	Product  string `json:"product"`
	Endpoint string `json:"endpoint"`
	State    string `json:"state"`
}

// WriteLocator registers the installed Runner with the CLI. It holds no
// credential, only where the private endpoint file is.
func (i Installer) WriteLocator() error {
	if err := os.MkdirAll(filepath.Dir(i.Paths.Locator()), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(Locator{Name: i.Paths.RunnerName(), Product: "Lazybox", Endpoint: i.Paths.Endpoint(), State: i.Paths.State}, "", "  ")
	if err != nil {
		return err
	}
	return writeAtomic(i.Paths.Locator(), append(data, '\n'), 0o600)
}

// Start starts an installed service that is not running, or does nothing.
func (i Installer) Start(ctx context.Context) error {
	if _, err := os.Stat(i.Definition()); err != nil {
		return fmt.Errorf("the Runner service is not installed; run `lazybox service install`: %w", err)
	}
	job, err := i.Manager.Status(ctx, i.Paths)
	if err != nil || job.Running {
		return err
	}
	if job.Loaded {
		return i.Manager.Restart(ctx, i.Paths)
	}
	return i.Manager.Activate(ctx, i.Paths)
}

// Stop stops the Runner; its agents keep running.
func (i Installer) Stop(ctx context.Context) error {
	return i.Manager.Deactivate(ctx, i.Paths)
}

// Restart replaces the running Runner with a fresh one of the same release.
func (i Installer) Restart(ctx context.Context) error {
	job, err := i.Manager.Status(ctx, i.Paths)
	if err != nil {
		return err
	}
	if !job.Loaded {
		return i.Start(ctx)
	}
	return i.Manager.Restart(ctx, i.Paths)
}

// forgetter is a manager that must be told a definition was deleted (systemd).
type forgetter interface {
	Forget(ctx context.Context) error
}

// Uninstall stops the service and removes its definition and locator.
// State, logs and credentials stay (`lazybox uninstall --remove-data` removes them).
func (i Installer) Uninstall(ctx context.Context) (removed []string, err error) {
	if err = i.Manager.Remove(ctx, i.Paths); err != nil {
		return nil, err
	}
	for _, path := range []string{i.Definition(), i.Paths.Locator()} {
		if err = os.Remove(path); err == nil {
			removed = append(removed, path)
		} else if !errors.Is(err, os.ErrNotExist) {
			return removed, err
		}
	}
	if f, ok := i.Manager.(forgetter); ok && len(removed) > 0 {
		if err = f.Forget(ctx); err != nil {
			return removed, err
		}
	}
	return removed, nil
}

// Status is the service as the system sees it, without asking the Runner.
type Status struct {
	Label string `json:"label"`
	// Manager is launchd, systemd or supervisor.
	Manager string `json:"manager"`
	// Unit is the systemd unit, under systemd.
	Unit string `json:"unit,omitempty"`
	// Definition is the service definition; Plist is the same file under
	// launchd, kept for the app's older readers.
	Definition string `json:"definition"`
	Plist      string `json:"plist,omitempty"`
	Installed  bool   `json:"installed"`
	Loaded     bool   `json:"loaded"`
	Running    bool   `json:"running"`
	PID        int    `json:"pid,omitempty"`
	LastExit   string `json:"lastExit,omitempty"`
	// StartsWhen says when the service starts by itself: at login, at boot, or not.
	StartsWhen string `json:"startsWhen,omitempty"`
	// Program and Version are the Runner the installed service runs.
	Program  string `json:"program,omitempty"`
	Version  string `json:"version,omitempty"`
	Path     string `json:"path,omitempty"`
	Endpoint string `json:"endpoint"`
	// Log is the Runner's log file, or the command that prints it (journalctl).
	Log   string `json:"log"`
	State string `json:"state"`
}

// LogDescription is where a person reads the Runner's output.
func (i Installer) LogDescription() string {
	src := i.Manager.Logs(i.Paths)
	if src.File != "" {
		return src.File
	}
	return shellJoin(src.Command)
}

// Status reads the definition and the manager's view of the service.
func (i Installer) Status(ctx context.Context) (Status, error) {
	definition := i.Definition()
	s := Status{Label: i.Paths.Label, Manager: i.Manager.Kind(), Definition: definition, Endpoint: i.Paths.Endpoint(), Log: i.LogDescription(), State: i.Paths.State}
	switch i.Manager.Kind() {
	case ManagerLaunchd:
		s.Plist = definition
	case ManagerSystemd:
		s.Unit = i.Paths.Unit()
	}
	if installed, err := i.Manager.ReadDefinition(ctx, definition); err == nil {
		s.Installed, s.Program, s.Version, s.Path = true, installed.Program, installed.Version, installed.Env["PATH"]
	} else if _, statErr := os.Stat(definition); statErr == nil {
		return s, err
	}
	job, err := i.Manager.Status(ctx, i.Paths)
	if err != nil {
		return s, err
	}
	s.Loaded, s.Running, s.PID, s.LastExit, s.StartsWhen = job.Loaded, job.Running, job.PID, job.LastExit, job.Detail
	return s, nil
}

func writeNew(path string, data []byte, mode os.FileMode) error {
	f, err := os.OpenFile(path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, mode)
	if err != nil {
		return err
	}
	_, err = f.Write(data)
	return errors.Join(err, f.Close())
}

// writeAtomic replaces path in one rename, so a service manager or the CLI
// never reads half a file.
func writeAtomic(path string, data []byte, mode os.FileMode) error {
	f, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".")
	if err != nil {
		return err
	}
	defer os.Remove(f.Name())
	if _, err = f.Write(data); err == nil {
		err = f.Sync()
	}
	if err = errors.Join(err, f.Close()); err != nil {
		return err
	}
	if err = os.Chmod(f.Name(), mode); err != nil {
		return err
	}
	return os.Rename(f.Name(), path)
}

// shellJoin quotes words for a POSIX shell, for a command shown to a person.
func shellJoin(words []string) string {
	out := make([]byte, 0, 64)
	for n, w := range words {
		if n > 0 {
			out = append(out, ' ')
		}
		if w != "" && plainWord.MatchString(w) {
			out = append(out, w...)
			continue
		}
		out = append(out, '\'')
		for _, c := range []byte(w) {
			if c == '\'' {
				out = append(out, `'\''`...)
				continue
			}
			out = append(out, c)
		}
		out = append(out, '\'')
	}
	return string(out)
}
