package installation

import (
	"context"
	"fmt"
	"os"
	"runtime"
)

// Manager is the service manager that keeps the installed Runner running
// for one person: started at login (or boot), started again whenever it
// exits, and never tied to a window or a terminal. Three exist:
//
//   - launchd, a per-user agent on macOS (Launchd);
//   - systemd, a user unit on Linux (Systemd);
//   - Lazybox's own supervisor, where there is no systemd, as in a container
//     (Supervisor): it restarts the Runner, but nothing starts it at boot.
//
// Whichever runs it, the Runner's agents, commands and terminals live in
// session hosts outside its process group and, under systemd, outside its
// cgroup (SESSION_HOSTS.md), so stopping or replacing the service leaves them
// running for the next Runner to re-attach.
type Manager interface {
	// Kind is ManagerLaunchd, ManagerSystemd or ManagerSupervisor.
	Kind() string
	// DefinitionPath is the file Render's output is written to.
	DefinitionPath(p Paths) string
	// Render is the service definition for s.
	Render(s Service) ([]byte, error)
	// ReadDefinition reads a written definition back.
	ReadDefinition(ctx context.Context, path string) (Installed, error)
	// Status is the manager's view of the service.
	Status(ctx context.Context, p Paths) (Job, error)
	// Activate makes the manager read the written definition and start the
	// Runner, at once and from now on at login or boot.
	Activate(ctx context.Context, p Paths) error
	// Deactivate stops the Runner. The definition stays: it starts again at
	// the next login or boot, or with Activate.
	Deactivate(ctx context.Context, p Paths) error
	// Restart replaces the running Runner with a new one.
	Restart(ctx context.Context, p Paths) error
	// Remove stops the Runner and makes the manager forget the definition,
	// before its file is deleted.
	Remove(ctx context.Context, p Paths) error
	// Logs says where the Runner's output goes.
	Logs(p Paths) LogSource
}

// Manager kinds.
const (
	ManagerLaunchd    = "launchd"
	ManagerSystemd    = "systemd"
	ManagerSupervisor = "supervisor"
)

// ManagerEnvironment picks a manager instead of the platform's own:
// "supervisor" runs the Runner under Lazybox's own supervisor on a Linux
// machine whose systemd user manager cannot be used.
const ManagerEnvironment = "LAZYBOX_SERVICE_MANAGER"

// Job is the manager's view of the service.
type Job struct {
	// Loaded means the manager runs the service or is about to: a launchd
	// job that is bootstrapped, a systemd unit that is active or activating,
	// a supervisor that is alive.
	Loaded  bool
	Running bool
	// PID is the Runner's.
	PID      int
	LastExit string
	// Detail is anything else worth saying about how it runs (whether it
	// starts at boot, say).
	Detail string
}

// Installed is what a written definition says: the program and the release it runs.
type Installed struct {
	Program string
	Version string
	Env     map[string]string
}

// LogSource is where a service's output can be read: a file, or a command
// that prints it (journalctl). Follow is the command's form that keeps
// printing new lines.
type LogSource struct {
	File            string
	Command, Follow []string
	// Lines is the argument template that limits Command to the last n
	// lines: "-n" for journalctl.
	Lines string
}

// SystemManager is the manager for this machine, or the one
// LAZYBOX_SERVICE_MANAGER names.
func SystemManager(getenv func(string) string) (Manager, error) {
	kind := getenv(ManagerEnvironment)
	if kind == "" {
		kind = defaultManager(runtime.GOOS, func(path string) bool { _, err := os.Stat(path); return err == nil })
	}
	switch kind {
	case ManagerLaunchd:
		return UserLaunchd(), nil
	case ManagerSystemd:
		return UserSystemd(), nil
	case ManagerSupervisor:
		return Supervisor{}, nil
	}
	return nil, fmt.Errorf("%s=%q: the service manager is launchd, systemd or supervisor", ManagerEnvironment, kind)
}

// defaultManager is launchd on macOS. On Linux it is systemd when systemd is
// the init system (sd_booted(3): /run/systemd/system exists), and Lazybox's
// own supervisor otherwise: a container, or WSL without systemd.
func defaultManager(goos string, exists func(string) bool) string {
	switch {
	case goos == "darwin":
		return ManagerLaunchd
	case exists("/run/systemd/system"):
		return ManagerSystemd
	}
	return ManagerSupervisor
}
