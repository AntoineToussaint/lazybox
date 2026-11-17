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
	Paths   Paths
	Layout  Layout
	Launchd Launchd
	// Shell reads the login shell's environment (ReadShellEnvironment).
	Shell func(context.Context) ShellEnvironment
}

// ServeArguments are the Runner's arguments under launchd: serve mode, as a
// supervised job (Restart Runner is available), with this person's paths.
func (i Installer) ServeArguments() []string {
	return []string{"serve", "--supervised",
		"--state", i.Paths.State,
		"--config", i.Paths.Config,
		"--worktrees", i.Paths.Worktrees,
		"--actions", i.Paths.ActionsFile(),
		"--restart-request", i.Paths.RestartRequest,
	}
}

// Plan is the job the installer would write, with the shell environment it read.
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
	// Previous is the job that was installed before, if any.
	Previous *Installed
	// Restarted: a running Runner was stopped and the new one started. Its
	// agents, commands and terminals kept running and are re-attached.
	Restarted bool
	Started   bool
}

// Install writes the job, replacing an installed one, and (with start)
// loads it. Replacing a loaded job stops the old Runner first: it detaches
// its agents, which the new Runner re-attaches. Nothing else is touched:
// state, credentials and the person's configuration stay where they are.
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
	if err = os.MkdirAll(filepath.Dir(i.Paths.Plist), 0o755); err != nil {
		return result, err
	}
	// The private Runner configuration (RUNNING.md): created once, never replaced.
	if err = writeNew(i.Paths.Config, []byte("{\n  \"profiles\": []\n}\n"), 0o600); err != nil && !errors.Is(err, os.ErrExist) {
		return result, err
	}
	if previous, err := ReadPlist(ctx, i.Paths.Plist); err == nil {
		result.Previous = &previous
	}
	data, err := service.Plist()
	if err != nil {
		return result, err
	}
	job, err := i.Launchd.Print(ctx, service.Label)
	if err != nil {
		return result, err
	}
	if job.Loaded {
		// launchd reads a plist when it loads it: an updated job takes effect
		// only through bootout and bootstrap.
		if err = i.Launchd.Bootout(ctx, service.Label); err != nil {
			return result, err
		}
		result.Restarted = job.Running
	}
	if err = writeAtomic(i.Paths.Plist, data, 0o644); err != nil {
		return result, err
	}
	if err = i.WriteLocator(); err != nil {
		return result, err
	}
	if start || job.Loaded {
		if err = i.Launchd.Bootstrap(ctx, i.Paths.Plist); err != nil {
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

// Start loads an installed job that is not loaded, or does nothing.
func (i Installer) Start(ctx context.Context) error {
	if _, err := os.Stat(i.Paths.Plist); err != nil {
		return fmt.Errorf("the Runner service is not installed; run `lazybox service install`: %w", err)
	}
	job, err := i.Launchd.Print(ctx, i.Paths.Label)
	if err != nil || job.Running {
		return err
	}
	if job.Loaded {
		return i.Launchd.Kickstart(ctx, i.Paths.Label)
	}
	return i.Launchd.Bootstrap(ctx, i.Paths.Plist)
}

// Stop unloads the job: the Runner stops, its agents keep running.
func (i Installer) Stop(ctx context.Context) error {
	return i.Launchd.Bootout(ctx, i.Paths.Label)
}

// Restart replaces the running Runner with a fresh one of the same release.
func (i Installer) Restart(ctx context.Context) error {
	job, err := i.Launchd.Print(ctx, i.Paths.Label)
	if err != nil {
		return err
	}
	if !job.Loaded {
		return i.Start(ctx)
	}
	return i.Launchd.Kickstart(ctx, i.Paths.Label)
}

// Uninstall unloads the job and removes its plist and locator. State, logs
// and credentials stay (`lazybox uninstall --remove-data` removes them).
func (i Installer) Uninstall(ctx context.Context) (removed []string, err error) {
	if err = i.Launchd.Bootout(ctx, i.Paths.Label); err != nil {
		return nil, err
	}
	for _, path := range []string{i.Paths.Plist, i.Paths.Locator()} {
		if err = os.Remove(path); err == nil {
			removed = append(removed, path)
		} else if !errors.Is(err, os.ErrNotExist) {
			return removed, err
		}
	}
	return removed, nil
}

// Status is the service as the system sees it, without asking the Runner.
type Status struct {
	Label     string `json:"label"`
	Plist     string `json:"plist"`
	Installed bool   `json:"installed"`
	Loaded    bool   `json:"loaded"`
	Running   bool   `json:"running"`
	PID       int    `json:"pid,omitempty"`
	LastExit  string `json:"lastExit,omitempty"`
	// Program and Version are the Runner the installed job runs.
	Program  string `json:"program,omitempty"`
	Version  string `json:"version,omitempty"`
	Path     string `json:"path,omitempty"`
	Endpoint string `json:"endpoint"`
	Log      string `json:"log"`
	State    string `json:"state"`
}

// Status reads the plist and launchd's view of the job.
func (i Installer) Status(ctx context.Context) (Status, error) {
	s := Status{Label: i.Paths.Label, Plist: i.Paths.Plist, Endpoint: i.Paths.Endpoint(), Log: i.Paths.Log(), State: i.Paths.State}
	if installed, err := ReadPlist(ctx, i.Paths.Plist); err == nil {
		s.Installed, s.Program, s.Version, s.Path = true, installed.Program, installed.Version, installed.Env["PATH"]
	} else if _, statErr := os.Stat(i.Paths.Plist); statErr == nil {
		return s, err
	}
	job, err := i.Launchd.Print(ctx, i.Paths.Label)
	if err != nil {
		return s, err
	}
	s.Loaded, s.Running, s.PID, s.LastExit = job.Loaded, job.Running, job.PID, job.LastExit
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

// writeAtomic replaces path in one rename, so launchd or the CLI never reads half a file.
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
