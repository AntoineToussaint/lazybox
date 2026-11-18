package runner

import (
	"context"
	"crypto/rand"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/AntoineToussaint/lazybox/services/runner/code/internal/sessionhost"
)

// serviceRunner runs the fixture Runner (an agent blocked on an approval) as
// a transient systemd user service, the way the installed Runner runs, with
// systemd's default KillMode=control-group: stopping it kills every process
// left in its cgroup. It returns the unit, the execution's ID and the agent's PID.
func serviceRunner(t *testing.T, state string, extra ...string) (unit, id string, pid int) {
	t.Helper()
	self, _ := os.Executable()
	dir := t.TempDir()
	out, workerLog := filepath.Join(dir, "runner.out"), filepath.Join(dir, "worker.log")
	unit = "lazybox-test-runner-" + strings.ToLower(rand.Text()[:10]) + ".service"
	args := []string{"--user", "--quiet", "--collect", "--unit=" + unit,
		"--property=KillMode=control-group", "--property=TimeoutStopSec=5",
		"--property=StandardOutput=file:" + out, "--property=StandardError=file:" + out + ".err",
		"--setenv=RUNNER_FIXTURE=runner", "--setenv=RUNNER_STATE=" + state, "--setenv=FIXTURE_WORKER_LOG=" + workerLog}
	args = append(append(args, extra...), "--", self)
	if output, err := exec.Command("systemd-run", args...).CombinedOutput(); err != nil {
		t.Fatalf("systemd-run: %v %s", err, output)
	}
	t.Cleanup(func() {
		_ = exec.Command("systemctl", "--user", "stop", unit).Run()
		_ = exec.Command("systemctl", "--user", "reset-failed", unit).Run()
	})
	opened := regexp.MustCompile(`open resume=\S* pid=(\d+)`)
	for end := time.Now().Add(30 * time.Second); time.Now().Before(end); time.Sleep(50 * time.Millisecond) {
		data, _ := os.ReadFile(out)
		line, _, ok := strings.Cut(string(data), "\n")
		if ok && strings.HasPrefix(line, "error") {
			t.Fatalf("fixture Runner: %s", line)
		}
		notes, _ := os.ReadFile(workerLog)
		if m := opened.FindSubmatch(notes); ok && m != nil {
			pid, _ = strconv.Atoi(string(m[1]))
			return unit, strings.TrimSpace(line), pid
		}
	}
	errors, _ := os.ReadFile(out + ".err")
	t.Fatalf("the fixture Runner never started its agent:\n%s", errors)
	return "", "", 0
}

func stopService(t *testing.T, unit string) {
	t.Helper()
	if output, err := exec.Command("systemctl", "--user", "stop", unit).CombinedOutput(); err != nil {
		t.Fatalf("systemctl --user stop: %v %s", err, output)
	}
}

// The requirement on Linux: stopping or restarting the Runner's systemd
// service, which kills everything in its cgroup, is a no-op for the agent,
// because its session host runs in a scope of its own. The next Runner
// re-attaches and the same agent process finishes the same turn. Without the
// scope the same stop takes the agent with it: that is what the scope is for.
func TestStoppingTheRunnersSystemdServiceIsANoOpForTheAgent(t *testing.T) {
	if status := sessionhost.Scopes(); !status.Scoped {
		t.Skip("no systemd user manager to run a service in: " + status.Reason)
	}
	t.Run("in its own scope", func(t *testing.T) {
		state := t.TempDir()
		cleanupHosts(t, state)
		unit, id, pid := serviceRunner(t, state)
		cgroup, _ := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/cgroup")
		if !strings.Contains(string(cgroup), "/lazybox-host-") || strings.Contains(string(cgroup), unit) {
			t.Fatalf("the agent is not in its host's scope: %s", cgroup)
		}
		stopService(t, unit)
		if syscall.Kill(pid, 0) != nil {
			t.Fatal("the agent died with the Runner's service")
		}

		b := fixtureBench(t, state)
		if err := b.Adopt(context.Background()); err != nil {
			t.Fatal(err)
		}
		if got := workerPID(t, recorded(t, b, id)); got != pid {
			t.Fatalf("the journal names agent %d, the worker said %d", got, pid)
		}
		finishAfterAdoption(t, b, id, pid)
		if err := b.Close(); err != nil {
			t.Fatal(err)
		}
		stoppedEventually(t, pid)
	})
	t.Run("without a scope", func(t *testing.T) {
		state := t.TempDir()
		cleanupHosts(t, state)
		// No systemd-run on the Runner's PATH: the host starts in its own
		// session only, inside the service's cgroup.
		unit, _, pid := serviceRunner(t, state, "--setenv=PATH=/nonexistent")
		cgroup, _ := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/cgroup")
		if !strings.Contains(string(cgroup), unit) {
			t.Fatalf("without a scope the agent should be in the service's cgroup: %s", cgroup)
		}
		stopService(t, unit)
		stoppedEventually(t, pid)
	})
}
