package sessionhost

import (
	"context"
	"crypto/rand"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"
)

// On Linux a session host leaving the Runner's session is not enough to
// outlive it. Under systemd, stopping or restarting a service kills every
// process in the service's cgroup, whatever its session or process group
// (systemd.kill(5), KillMode=control-group): `systemctl --user restart
// lazybox-runner` would take every agent with it. So each host starts in a
// transient scope of its own, `systemd-run --user --scope`, which moves it
// into lazybox-host-<id>.scope under the user manager before it runs. The
// host keeps its PID, its pipes and its session; only its cgroup changes.
//
// Where the user manager cannot give it one (no systemd, as in a container;
// no user manager, as under `su`; a cgroup the user manager may not move
// processes out of), the host starts as it does on macOS, in its own
// session: it then outlives a Runner restart, but not a stop of a systemd
// service it shares a cgroup with. The installed unit's KillMode=process
// covers that case for the Runner's own service (internal/installation).

// ScopeStatus says whether hosts start in scopes of their own, and why not.
type ScopeStatus struct {
	Scoped bool
	// Reason is why they do not, when they do not.
	Reason string
}

// scopes caches whether systemd-run can start scopes for this process. A
// failure is retried after a minute: a user manager can start later.
var scopes struct {
	sync.Mutex
	checked time.Time
	status  ScopeStatus
	runner  string
}

// scopeRetry is how long a failed check is trusted.
const scopeRetry = time.Minute

// disableScopes is set by tests that check what happens without a scope.
var disableScopes bool

// Scopes reports whether a host launched now starts in its own systemd
// scope, checking once (a trial scope that runs `true`) and remembering it.
func Scopes() ScopeStatus {
	if disableScopes {
		return ScopeStatus{Reason: "turned off"}
	}
	scopes.Lock()
	defer scopes.Unlock()
	if !scopes.checked.IsZero() && (scopes.status.Scoped || time.Since(scopes.checked) < scopeRetry) {
		return scopes.status
	}
	scopes.checked = time.Now()
	scopes.runner, scopes.status = checkScopes()
	return scopes.status
}

func checkScopes() (string, ScopeStatus) {
	if _, err := os.Stat("/run/systemd/system"); err != nil {
		return "", ScopeStatus{Reason: "systemd is not the init system here: a container, or WSL without systemd"}
	}
	runner, err := exec.LookPath("systemd-run")
	if err != nil {
		return "", ScopeStatus{Reason: "systemd-run is not on the Runner's PATH"}
	}
	truth, err := exec.LookPath("true")
	if err != nil {
		return "", ScopeStatus{Reason: "no `true` to try a scope with"}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, runner, append(scopeArguments("lazybox-check-"+strings.ToLower(rand.Text()[:12]), "Lazybox scope check"), truth)...).CombinedOutput()
	if err != nil {
		return "", ScopeStatus{Reason: fmt.Sprintf("systemd-run --user --scope failed: %v: %s", err, strings.TrimSpace(string(out)))}
	}
	return runner, ScopeStatus{Scoped: true}
}

// scopeArguments are systemd-run's arguments for a scope named unit, up to
// the command.
func scopeArguments(unit, description string) []string {
	return []string{"--user", "--scope", "--quiet", "--collect", "--unit=" + unit, "--description=" + description, "--"}
}

// ScopeUnit is the transient scope a host with this id runs in.
func ScopeUnit(id string) string { return "lazybox-host-" + strings.ToLower(id) + ".scope" }

// inOwnScope rewrites an unstarted cmd so its process starts in the transient
// scope unit (ScopeUnit). It reports whether it did; see Scopes for why not.
func inOwnScope(cmd *exec.Cmd, unit, description string) bool {
	status := Scopes()
	if !status.Scoped {
		return false
	}
	scopes.Lock()
	runner := scopes.runner
	scopes.Unlock()
	if runner == "" || cmd.Process != nil {
		return false
	}
	args := append([]string{runner}, scopeArguments(strings.TrimSuffix(unit, ".scope"), description)...)
	cmd.Args = append(append(args, cmd.Path), cmd.Args[1:]...)
	cmd.Path = runner
	return true
}

// scopeFailed records that a scope could not be started after the check
// passed, so hosts start without one until the check is made again.
func scopeFailed(reason string) {
	scopes.Lock()
	defer scopes.Unlock()
	scopes.checked = time.Now()
	scopes.status = ScopeStatus{Reason: reason}
	scopes.runner = ""
}
